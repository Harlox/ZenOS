//! Wayland protocol handler trait impls + delegate macros for ZenState.

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{PopupKind, Window};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::utils::{Serial, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
    CompositorState,
};
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
};

use crate::state::{ClientState, ZenState};

impl CompositorHandler for ZenState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // Bind before the `if let` so the elements() iterator temporary is
            // dropped here, freeing *self for set_focus below.
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == &root).unwrap_or(false))
                .cloned();
            if let Some(window) = window {
                window.on_commit();
                // Send the initial configure once, so the client attaches a
                // buffer and actually draws.
                if let Some(toplevel) = window.toplevel() {
                    let initial_sent = with_states(surface, |states| {
                        states
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .initial_configure_sent
                    });
                    if !initial_sent {
                        toplevel.send_configure();
                    }
                }
                // (Re)assert keyboard focus now that the window is mapped, so
                // typed keys actually reach it. set_focus is a no-op if already
                // focused.
                if let Some(keyboard) = self.seat.get_keyboard() {
                    if keyboard.current_focus().as_ref() != Some(&root) {
                        let serial = SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(root.clone()), serial);
                    }
                }
            }
        }
        self.popups.commit(surface);
    }
}
delegate_compositor!(ZenState);

impl BufferHandler for ZenState {
    fn buffer_destroyed(&mut self, _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {}
}

impl ShmHandler for ZenState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(ZenState);

impl XdgShellHandler for ZenState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::info!("new toplevel window");
        // Force server-side decorations up front, so clients that read the mode
        // before binding xdg-decoration (or never bind) still drop their CSD.
        surface.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        let window = Window::new_wayland_window(surface);
        // Map below the top bar AND with room above for the SSD titlebar
        // (top bar 30 + titlebar 28); the titlebar is drawn at surf_y - 28.
        self.space.map_element(window, (60, 80), false);
        // Keyboard focus is set on commit (once the client is ready), not here.
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}
delegate_xdg_shell!(ZenState);

impl XdgDecorationHandler for ZenState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // ZenOS draws decorations (macOS-style titlebar) — never client-side.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // Ignore the client's preference; always server-side.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}
delegate_xdg_decoration!(ZenState);

impl SeatHandler for ZenState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}
delegate_seat!(ZenState);

impl OutputHandler for ZenState {}
delegate_output!(ZenState);

impl SelectionHandler for ZenState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ZenState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for ZenState {}
impl ServerDndGrabHandler for ZenState {}
delegate_data_device!(ZenState);
