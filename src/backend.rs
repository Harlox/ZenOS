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
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::Color32F;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, Device as _};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::{DeviceFd, Logical, Point};

use crate::state::ZenState;

/// Background clear color (matches the old wgpu clear).
const CLEAR: [f32; 4] = [0.08, 0.08, 0.08, 1.0];
const BAR_COLOR: [f32; 4] = [0.18, 0.18, 0.18, 1.0];
const DOCK_COLOR: [f32; 4] = [0.25, 0.25, 0.25, 1.0];
const BAR_H: i32 = 30;
const DOCK_W: i32 = 500;
const DOCK_H: i32 = 65;
const DOCK_MARGIN: i32 = 15;

/// The DrmCompositor type for one GPU: GBM allocator + GBM framebuffer exporter,
/// `()` queue user-data, DrmDeviceFd-backed GBM.
type ZenCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// One GPU: the DRM device, GBM allocator, GLES renderer and scanout compositor.
pub struct Gpu {
    pub node: DrmNode,
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub allocator: GbmAllocator<DrmDeviceFd>,
    pub renderer: GlesRenderer,
    /// Kept alive: DrmCompositor holds a Weak reference to it for mode/scale.
    pub _output: Output,
    pub compositor: ZenCompositor,
    /// Screen size in px (from the DRM mode).
    pub size: (i32, i32),
    /// UI rect buffers (borrowed by render elements each frame).
    pub bar_buf: SolidColorBuffer,
    pub dock_buf: SolidColorBuffer,
}

impl Gpu {
    /// Render one frame: top bar + bottom dock over a clear background, page-flip.
    fn render(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (w, h) = self.size;
        let dock_x = (w - DOCK_W) / 2;
        let dock_y = h - DOCK_H - DOCK_MARGIN;
        let elements = [
            SolidColorRenderElement::from_buffer(
                &self.bar_buf,
                Point::<i32, Logical>::from((0, 0)),
                1.0,
                1.0,
                Kind::Unspecified,
            ),
            SolidColorRenderElement::from_buffer(
                &self.dock_buf,
                Point::<i32, Logical>::from((dock_x, dock_y)),
                1.0,
                1.0,
                Kind::Unspecified,
            ),
        ];
        self.compositor.render_frame::<_, SolidColorRenderElement>(
            &mut self.renderer,
            &elements,
            Color32F::from(CLEAR),
            FrameFlags::DEFAULT,
        )?;
        self.compositor.queue_frame(())?;
        Ok(())
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // --- Step 1: session -----------------------------------------------------
    let (session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("seat: {seat_name}");

    let mut event_loop: EventLoop<ZenState> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    let mut state = ZenState::new(session, seat_name.clone());

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
    let mut gpu = open_gpu(&mut state.session, primary, &dev_path)?;

    // First frame: clear to gray.
    if let Err(e) = gpu.render() {
        tracing::error!("first render failed: {e}");
    } else {
        tracing::info!("first frame submitted (gray clear)");
    }
    state.gpu = Some(gpu);

    handle.insert_source(udev_backend, move |event, _, _data| match event {
        UdevEvent::Added { device_id, .. } => tracing::debug!("udev add {device_id}"),
        UdevEvent::Changed { device_id } => tracing::debug!("udev change {device_id}"),
        UdevEvent::Removed { device_id } => tracing::debug!("udev remove {device_id}"),
    })?;

    // TODO(milestone-7): libinput source -> Esc to quit.

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
) -> Result<Gpu, Box<dyn std::error::Error>> {
    let fd = session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (mut drm, _drm_notifier) = DrmDevice::new(drm_fd.clone(), true)?;
    let gbm = GbmDevice::new(drm_fd)?;

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let renderer = unsafe { GlesRenderer::new(egl_context)? };

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
    let bar_buf = SolidColorBuffer::new((size.0, BAR_H), BAR_COLOR);
    let dock_buf = SolidColorBuffer::new((DOCK_W, DOCK_H), DOCK_COLOR);

    Ok(Gpu {
        node,
        drm,
        gbm,
        allocator,
        renderer,
        _output: output,
        compositor,
        size,
        bar_buf,
        dock_buf,
    })
}
