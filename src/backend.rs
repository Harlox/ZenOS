//! DRM/KMS backend: ZenOS owns the screen directly, no weston/X11.
//!
//! Milestones covered:
//!   1. libseat session (GPU/input access from a TTY without root)
//!   2. udev: discover the primary GPU
//!   3. open the DRM device + GBM allocator
//!   4. EGL + GlesRenderer, pick connector/CRTC/mode, clear the screen (gray)
//!
//! NOT yet: input (libinput), Wayland clients, hotplug, multi-GPU, the ZenOS
//! UI (bar/dock). Those are milestones 5-8.
//!
//! User-mode DRM master is currently not granted by logind in this setup; run
//! as root for now (see notes). smithay 0.7 API — Linux-only, iterate on the
//! Arch target.

use std::time::Duration;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmDeviceNotifier, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::{
    ButtonState, Event, InputEvent, KeyState, KeyboardKeyEvent, PointerButtonEvent,
    PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::reexports::input::Libinput;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::desktop::{Space, Window};
use smithay::render_elements;
use smithay::utils::Scale;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::Color32F;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, Device as _};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Rectangle, SERIAL_COUNTER};
use smithay::wayland::socket::ListeningSocketSource;

use std::sync::Arc;

use crate::state::{ClientState, MoveGrab};

use crate::state::ZenState;

/// Background clear color (matches the old wgpu clear).
const CLEAR: [f32; 4] = [0.08, 0.08, 0.08, 1.0];
const BAR_COLOR: [f32; 4] = [0.18, 0.18, 0.18, 1.0];
const DOCK_COLOR: [f32; 4] = [0.25, 0.25, 0.25, 1.0];
const BAR_H: i32 = 30;
const DOCK_W: i32 = 500;
const DOCK_H: i32 = 65;
const DOCK_MARGIN: i32 = 15;
/// xkb keycodes (evdev + 8). smithay's Keycode is xkb-space.
const KEY_ESC: u32 = 9; // evdev KEY_ESC 1 -> quit
const KEY_F1: u32 = 67; // evdev KEY_F1 59 -> spawn a terminal (Enter stays free)
const BAR_RADIUS: f32 = 0.0;
const DOCK_RADIUS: f32 = 16.0;
const CURSOR_SIZE: i32 = 12;
const CURSOR_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 1.0];
/// Left mouse button (evdev BTN_LEFT).
const BTN_LEFT: u32 = 0x110;

/// Rounded-rectangle pixel shader (GLSL ES 100; no #version per smithay).
/// Built-in uniforms: `size` (px), `alpha`. Custom: `u_color`, `u_radius`.
/// `v_coords` is normalized [0,1] across the element.
const ROUNDED_SHADER: &str = r#"
#extension GL_OES_standard_derivatives : enable
precision mediump float;
varying vec2 v_coords;
uniform float alpha;
uniform vec4 u_color;
uniform float u_radius;
uniform vec2 u_size;

float sd_rounded_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}

void main() {
    vec2 p = v_coords * u_size - u_size * 0.5;
    float d = sd_rounded_box(p, u_size * 0.5, u_radius);
    float aa = fwidth(d);
    float cov = 1.0 - smoothstep(-aa, aa, d);
    float a = u_color.a * cov * alpha;
    // smithay expects premultiplied alpha.
    gl_FragColor = vec4(u_color.rgb * a, a);
}
"#;

/// The DrmCompositor type for one GPU: GBM allocator + GBM framebuffer exporter,
/// `()` queue user-data, DrmDeviceFd-backed GBM.
type ZenCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

render_elements! {
    /// One frame's elements: client window surfaces + ZenOS UI (bar/dock).
    /// Renderer-specific (GlesRenderer) since PixelShaderElement is GLES-only.
    pub ZenElement<=GlesRenderer>;
    Window = WaylandSurfaceRenderElement<GlesRenderer>,
    Ui = PixelShaderElement,
}

/// One GPU: the DRM device, GBM allocator, GLES renderer and scanout compositor.
pub struct Gpu {
    pub node: DrmNode,
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub allocator: GbmAllocator<DrmDeviceFd>,
    pub renderer: GlesRenderer,
    /// Logical output: drives the DrmCompositor mode and is mapped into the Space.
    pub output: Output,
    pub compositor: ZenCompositor,
    /// Screen size in px (from the DRM mode).
    pub size: (i32, i32),
    /// Compiled rounded-rect pixel shader, reused for bar + dock.
    pub rounded: GlesPixelProgram,
    /// True while a page-flip is in flight (cleared on VBlank). Avoids queuing a
    /// second flip before the first completes.
    pub pending_flip: bool,
    /// Cursor position in px (updated each frame from pointer_location).
    pub cursor_pos: (i32, i32),
}

impl Gpu {
    /// Render one frame: client windows below, ZenOS UI (bar/dock) on top, over
    /// a clear background, then page-flip.
    /// Returns true if a frame was actually queued (had damage).
    pub fn render(&mut self, space: &Space<Window>) -> Result<bool, Box<dyn std::error::Error>> {
        let (w, h) = self.size;
        let dock_x = (w - DOCK_W) / 2;
        let dock_y = h - DOCK_H - DOCK_MARGIN;
        let bar = PixelShaderElement::new(
            self.rounded.clone(),
            Rectangle::from_loc_and_size((0, 0), (w, BAR_H)),
            None,
            1.0,
            vec![
                Uniform::new("u_color", BAR_COLOR),
                Uniform::new("u_radius", BAR_RADIUS),
                Uniform::new("u_size", [w as f32, BAR_H as f32]),
            ],
            Kind::Unspecified,
        );
        let dock = PixelShaderElement::new(
            self.rounded.clone(),
            Rectangle::from_loc_and_size((dock_x, dock_y), (DOCK_W, DOCK_H)),
            None,
            1.0,
            vec![
                Uniform::new("u_color", DOCK_COLOR),
                Uniform::new("u_radius", DOCK_RADIUS),
                Uniform::new("u_size", [DOCK_W as f32, DOCK_H as f32]),
            ],
            Kind::Unspecified,
        );

        let (cx, cy) = self.cursor_pos;
        let cursor = PixelShaderElement::new(
            self.rounded.clone(),
            Rectangle::from_loc_and_size((cx, cy), (CURSOR_SIZE, CURSOR_SIZE)),
            None,
            1.0,
            vec![
                Uniform::new("u_color", CURSOR_COLOR),
                Uniform::new("u_radius", 2.0f32),
                Uniform::new("u_size", [CURSOR_SIZE as f32, CURSOR_SIZE as f32]),
            ],
            Kind::Unspecified,
        );

        // Front-to-back: cursor, UI, then client windows.
        let mut elements: Vec<ZenElement> = vec![
            ZenElement::Ui(cursor),
            ZenElement::Ui(bar),
            ZenElement::Ui(dock),
        ];
        let scale = Scale::from(1.0);
        for window in space.elements() {
            let loc = space
                .element_location(window)
                .unwrap_or_default()
                .to_physical_precise_round(1.0);
            let rels = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                &mut self.renderer,
                loc,
                scale,
                1.0,
            );
            elements.extend(rels.into_iter().map(ZenElement::Window));
        }

        let res = self.compositor.render_frame::<_, ZenElement>(
            &mut self.renderer,
            &elements,
            Color32F::from(CLEAR),
            FrameFlags::DEFAULT,
        )?;
        if res.is_empty {
            return Ok(false); // no damage -> nothing to flip
        }
        self.compositor.queue_frame(())?;
        Ok(true)
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // --- Step 1: session -----------------------------------------------------
    let (session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("seat: {seat_name}");

    let mut event_loop: EventLoop<ZenState> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    // Wayland display (kept local; dispatched each loop tick so existing calloop
    // sources keep their &mut ZenState signature, no CalloopData wrapper needed).
    let mut display: Display<ZenState> = Display::new()?;
    let dh = display.handle();

    let mut state = ZenState::new(dh, event_loop.get_signal(), session, seat_name.clone());

    // Wayland client socket. Clients connecting here get a ClientState.
    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    handle.insert_source(socket, move |stream, _, data: &mut ZenState| {
        if let Err(e) = data
            .display_handle
            .insert_client(stream, Arc::new(ClientState::default()))
        {
            tracing::warn!("failed to accept client: {e}");
        }
    })?;
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    tracing::info!("WAYLAND_DISPLAY={socket_name}");

    // --- Step 2: udev (GPU discovery) ---------------------------------------
    let udev_backend = UdevBackend::new(&seat_name)?;

    let primary = primary_gpu(&seat_name)
        .ok()
        .flatten()
        .and_then(|p| DrmNode::from_path(p).ok())
        .or_else(|| {
            all_gpus(&seat_name)
                .ok()?
                .into_iter()
                .find_map(|p| DrmNode::from_path(p).ok())
        })
        .ok_or("no GPU found via udev")?;
    tracing::info!("primary GPU: {:?}", primary);

    let dev_path = primary
        .dev_path()
        .ok_or("primary GPU has no device path")?;

    // --- Event sources -------------------------------------------------------
    handle.insert_source(session_notifier, move |event, _, data| match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused");
            if let Some(gpu) = &mut data.gpu {
                gpu.drm.pause();
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("session resumed");
            // TODO(milestone-7): drm.activate() + reset CRTC + redraw.
        }
    })?;

    // Pump until the session is active before opening the GPU (SET_MASTER).
    let mut tries = 0;
    while !state.session.is_active() && tries < 200 {
        event_loop.dispatch(Some(Duration::from_millis(10)), &mut state)?;
        tries += 1;
    }
    if !state.session.is_active() {
        return Err("session never became active (no DRM master)".into());
    }
    tracing::info!("session active after {tries} dispatch(es)");

    // Open + init the primary GPU (steps 3-4).
    let (mut gpu, drm_notifier) = open_gpu(&mut state.session, primary, &dev_path)?;

    // First frame: starts the page-flip chain.
    match gpu.render(&state.space) {
        Ok(true) => {
            gpu.pending_flip = true;
            tracing::info!("first frame submitted");
        }
        Ok(false) => {}
        Err(e) => tracing::error!("first render failed: {e}"),
    }
    // Advertise the output to clients (wl_output global) so they see a monitor,
    // then map it into the Space.
    let _output_global = gpu.output.create_global::<ZenState>(&state.display_handle);
    state.space.map_output(&gpu.output, (0, 0));
    state.gpu = Some(gpu);

    // DRM VBlank: ack the completed flip, then render + queue the next frame.
    // This is what keeps the screen updating (so client windows appear).
    handle.insert_source(drm_notifier, move |event, _, data| match event {
        DrmEvent::VBlank(_crtc) => {
            if let Some(gpu) = &mut data.gpu {
                let _ = gpu.compositor.frame_submitted();
                gpu.pending_flip = false;
            }
        }
        DrmEvent::Error(e) => tracing::error!("DRM error: {e:?}"),
    })?;

    handle.insert_source(udev_backend, move |event, _, _data| match event {
        UdevEvent::Added { device_id, .. } => tracing::debug!("udev add {device_id}"),
        UdevEvent::Changed { device_id } => tracing::debug!("udev change {device_id}"),
        UdevEvent::Removed { device_id } => tracing::debug!("udev remove {device_id}"),
    })?;

    // Input: libinput on the session seat. Esc quits cleanly.
    let mut libinput =
        Libinput::new_with_udev(LibinputSessionInterface::from(state.session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "libinput udev_assign_seat failed")?;
    let libinput_backend = LibinputInputBackend::new(libinput);
    handle.insert_source(libinput_backend, move |event, _, data| match event {
        InputEvent::Keyboard { event } => {
            let keyboard = data.seat.get_keyboard().unwrap();
            let serial = SERIAL_COUNTER.next_serial();
            let time = event.time_msec();
            let code = event.key_code();
            let key_state = event.state();
            // Forward to the focused client, unless it's a compositor shortcut.
            keyboard.input::<(), _>(data, code, key_state, serial, time, |data, _mods, _sym| {
                if key_state == KeyState::Pressed {
                    if code == KEY_ESC.into() {
                        tracing::info!("Esc pressed, exiting");
                        data.running = false;
                        return FilterResult::Intercept(());
                    } else if code == KEY_F1.into() {
                        tracing::info!("F1 pressed, launching foot");
                        let _ = std::process::Command::new("foot").spawn();
                        return FilterResult::Intercept(());
                    }
                }
                FilterResult::Forward
            });
        }
        InputEvent::PointerMotion { event } => {
            let (sw, sh) = data.gpu.as_ref().map(|g| g.size).unwrap_or((0, 0));
            let mut loc = data.pointer_location;
            loc.x = (loc.x + event.delta_x()).clamp(0.0, sw as f64);
            loc.y = (loc.y + event.delta_y()).clamp(0.0, sh as f64);
            data.pointer_location = loc;

            if let Some(grab) = &data.move_grab {
                let dx = (loc.x - grab.start_ptr.x) as i32;
                let dy = (loc.y - grab.start_ptr.y) as i32;
                let new = (grab.start_win.x + dx, grab.start_win.y + dy);
                let window = grab.window.clone();
                data.space.map_element(window, new, false);
            } else {
                let focus = data
                    .space
                    .element_under(loc)
                    .and_then(|(w, p)| w.toplevel().map(|t| (t.wl_surface().clone(), p.to_f64())));
                let pointer = data.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                pointer.motion(
                    data,
                    focus,
                    &MotionEvent {
                        location: loc,
                        serial,
                        time,
                    },
                );
                pointer.frame(data);
            }
        }
        InputEvent::PointerButton { event } => {
            let serial = SERIAL_COUNTER.next_serial();
            let time = event.time_msec();
            let button = event.button_code();
            let button_state = event.state();
            if button_state == ButtonState::Pressed {
                let loc = data.pointer_location;
                let under = data.space.element_under(loc).map(|(w, p)| (w.clone(), p));
                if let Some((window, win_loc)) = under {
                    let keyboard = data.seat.get_keyboard().unwrap();
                    let mods = keyboard.modifier_state();
                    if let Some(s) = window.toplevel().map(|t| t.wl_surface().clone()) {
                        keyboard.set_focus(data, Some(s), serial);
                    }
                    // Super + left drag = move the window.
                    if mods.logo && button == BTN_LEFT {
                        data.move_grab = Some(MoveGrab {
                            window,
                            start_ptr: loc,
                            start_win: win_loc,
                        });
                    }
                }
            } else {
                data.move_grab = None;
            }
            let pointer = data.seat.get_pointer().unwrap();
            pointer.button(
                data,
                &ButtonEvent {
                    button,
                    state: button_state,
                    serial,
                    time,
                },
            );
            pointer.frame(data);
        }
        _ => {}
    })?;

    // --- Safety auto-exit ----------------------------------------------------
    let timeout = std::env::var("ZENOS_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10);
    let deadline =
        (timeout > 0).then(|| std::time::Instant::now() + Duration::from_secs(timeout));
    if deadline.is_some() {
        tracing::info!("auto-exit in {timeout}s (set ZENOS_TIMEOUT to change, 0 to disable)");
    }

    // --- Dispatch loop -------------------------------------------------------
    // Single static frame for now; just service events (vblank, session) and
    // honor the auto-exit deadline. Continuous redraw comes with the UI port.
    tracing::info!("ZenOS compositor running");
    while state.running {
        event_loop.dispatch(Some(Duration::from_millis(16)), &mut state)?;

        // Service Wayland clients, refresh layout, then render (queues a flip
        // only if there is damage and none is pending).
        display.dispatch_clients(&mut state)?;
        state.space.refresh();
        state.render();
        display.flush_clients()?;

        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                tracing::info!("auto-exit timeout reached, shutting down");
                state.running = false;
            }
        }
    }

    tracing::info!("releasing GPU + session");
    state.gpu = None; // drop DrmDevice -> release DRM master -> restore TTY
    Ok(())
}

/// Steps 3-4: open the DRM device, GBM allocator, EGL/GLES renderer, then pick a
/// connected connector + CRTC + mode and build the scanout compositor.
fn open_gpu(
    session: &mut LibSeatSession,
    node: DrmNode,
    path: &std::path::Path,
) -> Result<(Gpu, DrmDeviceNotifier), Box<dyn std::error::Error>> {
    let fd = session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (mut drm, drm_notifier) = DrmDevice::new(drm_fd.clone(), true)?;
    let gbm = GbmDevice::new(drm_fd)?;

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };

    // Rounded-rect shader, reused by the bar and dock.
    let rounded = renderer.compile_custom_pixel_shader(
        ROUNDED_SHADER,
        &[
            UniformName::new("u_color", UniformType::_4f),
            UniformName::new("u_radius", UniformType::_1f),
            UniformName::new("u_size", UniformType::_2f),
        ],
    )?;

    // --- pick a connected connector + its preferred mode --------------------
    let res = drm.resource_handles()?;
    let conn = res
        .connectors()
        .iter()
        .filter_map(|h| drm.get_connector(*h, false).ok())
        .find(|c| c.state() == connector::State::Connected)
        .ok_or("no connected connector")?;
    let mode = *conn.modes().first().ok_or("connector has no modes")?;
    tracing::info!(
        "connector {:?}, mode {}x{}",
        conn.interface(),
        mode.size().0,
        mode.size().1
    );

    // --- find a CRTC drivable by this connector's encoders ------------------
    let crtc = conn
        .encoders()
        .iter()
        .filter_map(|e| drm.get_encoder(*e).ok())
        .flat_map(|enc| res.filter_crtcs(enc.possible_crtcs()))
        .next()
        .ok_or("no CRTC for connector")?;

    // --- scanout surface ----------------------------------------------------
    let surface = drm.create_surface(crtc, mode, &[conn.handle()])?;

    // --- logical output (drives the compositor's mode/scale) ----------------
    let output = Output::new(
        "ZenOS-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "ZenOS".into(),
            model: "Virtual".into(),
        },
    );
    let wl_mode = OutputMode {
        size: (mode.size().0 as i32, mode.size().1 as i32).into(),
        refresh: mode.vrefresh() as i32 * 1000,
    };
    output.change_current_state(Some(wl_mode), None, None, None);
    output.set_preferred(wl_mode);

    // --- compositor ---------------------------------------------------------
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    let compositor = DrmCompositor::new(
        &output,
        surface,
        None,
        allocator.clone(),
        GbmFramebufferExporter::new(gbm.clone(), Some(node)),
        [Fourcc::Argb8888, Fourcc::Xrgb8888],
        render_formats,
        (64u32, 64u32).into(),
        Some(gbm.clone()),
    )?;

    let size = (mode.size().0 as i32, mode.size().1 as i32);

    Ok((
        Gpu {
            node,
            drm,
            gbm,
            allocator,
            renderer,
            output,
            compositor,
            size,
            rounded,
            pending_flip: false,
            cursor_pos: (0, 0),
        },
        drm_notifier,
    ))
}
