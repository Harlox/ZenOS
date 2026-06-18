use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::LoopSignal;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

use smithay::backend::session::libseat::LibSeatSession;

use crate::config::{MOVE_LERP, WIN_MIN_H, WIN_MIN_W};
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
    /// Set when the *scene* (wallpaper + windows + bar/clock) changed, as opposed
    /// to just the cursor/dock overlay. Lets render skip the offscreen compose
    /// pass on pure pointer motion and reuse the cached scene texture.
    pub scene_dirty: bool,
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
    /// Active interactive resize (drag a window edge/corner).
    pub resize_grab: Option<ResizeGrab>,
    /// Toplevels already centered once (on their first sized commit), so the
    /// user's later moves aren't yanked back to center.
    pub placed: HashSet<WlSurface>,
    /// Maximized windows -> their pre-maximize (loc, size), for restore.
    pub maximized: HashMap<WlSurface, ((i32, i32), (i32, i32))>,
    /// Top-bar power dropdown (Restart / Shut Down) open state.
    pub power_menu_open: bool,
    /// Interpolated window origin during a move (smooths a low-Hz touchpad to the
    /// display refresh). `None` when no move is active.
    pub move_current: Option<(f64, f64)>,
    /// How far the interpolated window/cursor currently trails the real pointer.
    /// Applied to the rendered cursor too, so titlebar + cursor stay locked.
    pub move_lag: (f64, f64),
}

/// Tracks an interactive window move.
pub struct MoveGrab {
    pub window: Window,
    pub start_ptr: Point<f64, Logical>,
    pub start_win: Point<i32, Logical>,
}

/// Tracks an interactive window resize: which edges are dragged + the window's
/// geometry when the grab started.
pub struct ResizeGrab {
    pub window: Window,
    pub start_ptr: Point<f64, Logical>,
    pub start_loc: Point<i32, Logical>,
    pub start_size: (i32, i32),
    /// Last size sent to the client, to skip duplicate configures (terminals
    /// snap to a cell grid, so most motions don't change the size).
    pub last_size: (i32, i32),
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
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
            scene_dirty: true,
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
            resize_grab: None,
            placed: HashSet::new(),
            maximized: HashMap::new(),
            power_menu_open: false,
            move_current: None,
            move_lag: (0.0, 0.0),
        }
    }

    /// Compose + flip, but only when something changed (`dirty`). The 2-pass
    /// renderer fully re-composes each call, so we gate on dirty to avoid
    /// flipping every VBlank. Does NOT send frame callbacks — those go out on
    /// VBlank so clients are throttled to the monitor refresh.
    /// Apply an active interactive move once per frame. The window eases toward
    /// the pointer target (`MOVE_LERP`) instead of snapping, so a low-Hz touchpad
    /// looks smooth at the display refresh. `move_lag` records the residual gap so
    /// render() can offset the cursor by the same amount — titlebar and cursor
    /// stay locked under the finger. No grab → reset the interpolation state.
    fn apply_move_grab(&mut self) {
        let Some(grab) = self.move_grab.as_ref() else {
            self.move_current = None;
            self.move_lag = (0.0, 0.0);
            return;
        };
        let (start_win, start_ptr, window) =
            (grab.start_win, grab.start_ptr, grab.window.clone());

        let tx = start_win.x as f64 + (self.pointer_location.x - start_ptr.x);
        let ty = start_win.y as f64 + (self.pointer_location.y - start_ptr.y);
        // First frame of a grab starts on-target (no initial jump).
        let cur = self.move_current.get_or_insert((tx, ty));
        cur.0 += (tx - cur.0) * MOVE_LERP;
        cur.1 += (ty - cur.1) * MOVE_LERP;
        // Snap the last sub-pixel so it settles exactly instead of crawling.
        if (tx - cur.0).abs() < 0.5 {
            cur.0 = tx;
        }
        if (ty - cur.1).abs() < 0.5 {
            cur.1 = ty;
        }
        let (cx, cy) = *cur;
        self.move_lag = (tx - cx, ty - cy);
        self.space
            .map_element(window, (cx.round() as i32, cy.round() as i32), false);
    }

    /// Apply an active interactive resize once per frame, from the latest pointer
    /// position. Coalesced like the move: one xdg configure per frame instead of
    /// one per ~1000Hz motion event, which is what made corner-resize lag and the
    /// stretched-buffer text tear. Deduped by `last_size` so a grid-snapping
    /// client (terminal) doesn't get redundant configures.
    fn apply_resize_grab(&mut self) {
        let Some(grab) = self.resize_grab.as_ref() else {
            return;
        };
        let dx = (self.pointer_location.x - grab.start_ptr.x) as i32;
        let dy = (self.pointer_location.y - grab.start_ptr.y) as i32;
        let (sw, sh) = grab.start_size;
        let (sx, sy) = (grab.start_loc.x, grab.start_loc.y);
        let (gl, gr, gt, gb) = (grab.left, grab.right, grab.top, grab.bottom);
        let last = grab.last_size;
        let window = grab.window.clone();

        let mut nw = sw;
        let mut nh = sh;
        if gr {
            nw = sw + dx;
        }
        if gl {
            nw = sw - dx;
        }
        if gb {
            nh = sh + dy;
        }
        if gt {
            nh = sh - dy;
        }
        nw = nw.max(WIN_MIN_W);
        nh = nh.max(WIN_MIN_H);
        if (nw, nh) == last {
            return;
        }
        // Left/top edges move the origin as the size changes.
        let nx = if gl { sx + (sw - nw) } else { sx };
        let ny = if gt { sy + (sh - nh) } else { sy };
        if let Some(t) = window.toplevel() {
            t.with_pending_state(|s| s.size = Some((nw, nh).into()));
            t.send_configure();
        }
        self.space.map_element(window, (nx, ny), false);
        if let Some(g) = &mut self.resize_grab {
            g.last_size = (nw, nh);
        }
    }

    /// True while the interpolated move hasn't caught up to the pointer yet, so
    /// render() should keep composing each vblank until it settles.
    fn move_settling(&self) -> bool {
        self.move_grab.is_some() && (self.move_lag.0.abs() > 0.5 || self.move_lag.1.abs() > 0.5)
    }

    pub fn render(&mut self) {
        if !self.dirty {
            return;
        }
        self.apply_move_grab();
        self.apply_resize_grab();
        let shot = self.screenshot;
        let scene_dirty = self.scene_dirty;
        let menu_open = self.power_menu_open;
        let move_lag = self.move_lag;
        let Self {
            gpu,
            space,
            pointer_location,
            ..
        } = self;
        let Some(gpu) = gpu else { return };

        // Offset the cursor by the same lag as the eased window, so the titlebar
        // stays under the pointer during an interpolated drag.
        let cursor = (
            (pointer_location.x - move_lag.0) as i32,
            (pointer_location.y - move_lag.1) as i32,
        );
        // Clear dirty only if every output was rendered (none mid-flip); a
        // skipped output retries on its next VBlank-driven render.
        let rendered = gpu.render_all(space, cursor, shot, scene_dirty, menu_open);
        if rendered {
            self.dirty = false;
            self.scene_dirty = false;
        }
        if rendered || !shot {
            self.screenshot = false;
        }
        // Keep composing each vblank until the eased move catches the pointer,
        // even after input events stop arriving.
        if self.move_settling() {
            self.dirty = true;
            self.scene_dirty = true;
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
