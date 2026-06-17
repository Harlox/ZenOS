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

use crate::render::Gpu;

/// Whole-compositor state: DRM backend + Wayland frontend. Passed as `&mut data`
/// to every calloop event source.
pub struct ZenState {
    pub running: bool,
    pub start_time: Instant,
    /// Set when anything visible changed (input, client commit, hotplug, clock).
    /// `render` only composes + flips when dirty, then clears it — this is what
    /// keeps the 2-pass renderer from flipping every VBlank.
    pub dirty: bool,
    /// Set by F2: capture the next frame to /tmp/zenos-shot.png.
    pub screenshot: bool,

    // --- backend ---
    pub session: LibSeatSession,
    pub gpu: Option<Gpu>,

    // --- wayland frontend ---
    pub display_handle: DisplayHandle,
    /// Held to wake/stop the loop; not read directly yet.
    #[allow(dead_code)]
    pub loop_signal: LoopSignal,
    pub space: Space<Window>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Held to keep the Wayland global alive; not read directly.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    /// Held to keep the Wayland global alive; not read directly.
    #[allow(dead_code)]
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
            dirty: true,
            screenshot: false,
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

    /// Compose + flip, but only when something changed (`dirty`). The 2-pass
    /// renderer fully re-composes each call, so we gate on dirty to avoid
    /// flipping every VBlank. Does NOT send frame callbacks — those go out on
    /// VBlank so clients are throttled to the monitor refresh.
    pub fn render(&mut self) {
        if !self.dirty {
            return;
        }
        let shot = self.screenshot;
        let Self {
            gpu,
            space,
            pointer_location,
            ..
        } = self;
        let Some(gpu) = gpu else { return };

        let cursor = (pointer_location.x as i32, pointer_location.y as i32);
        // Clear dirty only if every output was rendered (none mid-flip); a
        // skipped output retries on its next VBlank-driven render.
        let rendered = gpu.render_all(space, cursor, shot);
        if rendered {
            self.dirty = false;
        }
        if rendered || !shot {
            self.screenshot = false;
        }
    }

    /// Tell clients they may draw the next frame. Called once per VBlank, so a
    /// client (e.g. foot) is paced to the monitor's refresh rate (60/120/165Hz)
    /// instead of spinning as fast as the event loop. Without this, clients
    /// draw once and never update (typed text, cursor blink, etc.).
    pub fn send_frame_callbacks(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let now = self.start_time.elapsed();
        for surface in gpu.surfaces.values() {
            let output = surface.output.clone();
            for w in self.space.elements() {
                w.send_frame(&output, now, Some(Duration::ZERO), |_, _| Some(output.clone()));
            }
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
