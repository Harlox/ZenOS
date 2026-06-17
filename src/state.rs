use std::time::{Duration, Instant};

use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::LoopSignal;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

use smithay::backend::session::libseat::LibSeatSession;

use crate::backend::Gpu;

/// Whole-compositor state: DRM backend + Wayland frontend. Passed as `&mut data`
/// to every calloop event source.
pub struct ZenState {
    pub running: bool,
    pub start_time: Instant,

    // --- backend ---
    pub session: LibSeatSession,
    pub gpu: Option<Gpu>,

    // --- wayland frontend ---
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub space: Space<Window>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<ZenState>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<ZenState>,

    /// Cursor position in global logical coords.
    pub pointer_location: Point<f64, Logical>,
    /// Active interactive move (Super+drag): window + pointer/window start.
    pub move_grab: Option<MoveGrab>,
}

/// Tracks an interactive window move.
pub struct MoveGrab {
    pub window: Window,
    pub start_ptr: Point<f64, Logical>,
    pub start_win: Point<i32, Logical>,
}

impl ZenState {
    pub fn new(
        dh: DisplayHandle,
        loop_signal: LoopSignal,
        session: LibSeatSession,
        seat_name: String,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, seat_name);
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();

        Self {
            running: true,
            start_time: Instant::now(),
            session,
            gpu: None,
            display_handle: dh,
            loop_signal,
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            pointer_location: (0.0, 0.0).into(),
            move_grab: None,
        }
    }

    /// Render the current frame via the GPU (split borrows: gpu + space).
    /// Skips if a flip is already pending; only marks pending when one is queued.
    pub fn render(&mut self) {
        let Self {
            gpu,
            space,
            start_time,
            pointer_location,
            ..
        } = self;
        let Some(gpu) = gpu else { return };

        gpu.cursor_pos = (pointer_location.x as i32, pointer_location.y as i32);
        if !gpu.pending_flip {
            match gpu.render(space) {
                Ok(true) => gpu.pending_flip = true,
                Ok(false) => {}
                Err(e) => tracing::error!("render failed: {e}"),
            }
        }

        // Frame callbacks: tell clients (e.g. foot) they may draw the next
        // frame. Without this they draw once and never update (typed text,
        // cursor blink, etc. never appear).
        let now = start_time.elapsed();
        let output = gpu.output.clone();
        for w in space.elements() {
            w.send_frame(&output, now, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }
    }
}

/// Per-client data: holds the client's compositor state.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
