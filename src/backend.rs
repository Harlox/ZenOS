//! DRM/KMS backend: ZenOS owns the screen directly, no weston/X11.
//!
//! Milestones covered by this scaffold:
//!   1. libseat session (GPU/input access from a TTY without root)
//!   2. udev: discover the primary GPU
//!   3. open the DRM device + GBM allocator
//!   4. EGL + GlesRenderer, set a CRTC mode, clear the screen each frame
//!
//! NOT yet: input (libinput), Wayland clients, hotplug, multi-GPU, the ZenOS
//! UI (bar/dock). Those are milestones 5-8.
//!
//! smithay 0.7 API. This file is Linux-only and will need compiler iteration on
//! the Arch target — the DRM output/compositor construction (marked TODO) is the
//! most version-sensitive part.

use std::time::Duration;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{primary_gpu, all_gpus, UdevBackend, UdevEvent};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;

use crate::state::ZenState;

/// One GPU: the DRM device, GBM allocator, GLES renderer and scanout surface.
pub struct Gpu {
    pub node: DrmNode,
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub allocator: GbmAllocator<DrmDeviceFd>,
    pub renderer: GlesRenderer,
    // TODO(milestone-4): scanout surface. In smithay 0.7 use either
    // `DrmCompositor` (handles allocation + page flip) or a raw `DrmSurface`.
    // Its generic params depend on the render element type — fill against the
    // compiler. Storing as Option until wired.
    // pub compositor: DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmDevice<DrmDeviceFd>, (), DrmDeviceFd>,
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

    // Resolve the primary GPU now so we can open it before entering the loop.
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

    // Open + init the primary GPU (steps 3-4).
    let dev_path = primary
        .dev_path()
        .ok_or("primary GPU has no device path")?;
    let gpu = open_gpu(&mut state.session, primary, &dev_path)?;
    state.gpu = Some(gpu);

    // --- Event sources -------------------------------------------------------
    // Session pause/resume (VT switch): drop/regain DRM master.
    handle.insert_source(session_notifier, move |event, _, data| match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused");
            if let Some(gpu) = &mut data.gpu {
                gpu.drm.pause();
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("session resumed");
            // TODO(milestone-4): drm.activate() + reset CRTC + redraw.
        }
    })?;

    // Hotplug. For milestone 1-4 we already opened the primary GPU above, so
    // just log; full add/remove handling is later.
    handle.insert_source(udev_backend, move |event, _, _data| match event {
        UdevEvent::Added { device_id, .. } => tracing::debug!("udev add {device_id}"),
        UdevEvent::Changed { device_id } => tracing::debug!("udev change {device_id}"),
        UdevEvent::Removed { device_id } => tracing::debug!("udev remove {device_id}"),
    })?;

    // TODO(milestone-7): libinput source -> state.process_input (Esc to quit).

    // --- Dispatch loop -------------------------------------------------------
    tracing::info!("ZenOS compositor running");
    while state.running {
        // Render a frame, then wait up to ~16ms (≈60Hz) for events. Event-driven
        // damage redraw comes with the UI port; for now we clear every tick.
        render(&mut state);
        event_loop.dispatch(Some(Duration::from_millis(16)), &mut state)?;
    }

    Ok(())
}

/// Steps 3-4: open the DRM device, create the GBM allocator, EGL display and
/// GLES renderer for one GPU.
fn open_gpu(
    session: &mut LibSeatSession,
    node: DrmNode,
    path: &std::path::Path,
) -> Result<Gpu, Box<dyn std::error::Error>> {
    // Open via the session so we get a DRM-master-capable fd without root.
    let fd = session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // `true` = disable atomic? In 0.7 the second arg is `disable_connectors`.
    // Verify against the compiler.
    let (drm, _drm_notifier) = DrmDevice::new(drm_fd.clone(), true)?;
    let gbm = GbmDevice::new(drm_fd)?;

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    // EGL display from the GBM device, then a GLES renderer.
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let renderer = unsafe { GlesRenderer::new(egl_context)? };

    // TODO(milestone-4): pick connector + CRTC + preferred mode, build the
    // scanout surface (DrmCompositor / DrmSurface) and store it on Gpu.
    //   - enumerate `drm.resource_handles()` connectors, find one Connected
    //   - pick `connector.modes()` preferred (or first)
    //   - find a free crtc for that connector's encoder
    //   - DrmCompositor::new(&output, surface, planes, allocator, gbm, ...)

    Ok(Gpu {
        node,
        drm,
        gbm,
        allocator,
        renderer,
    })
}

/// Step 4: clear the screen. Placeholder until the scanout surface exists.
fn render(_state: &mut ZenState) {
    // TODO(milestone-4): with the DrmCompositor in place:
    //   let elements: &[ZenRenderElement] = &[];
    //   compositor.render_frame(&mut renderer, elements, CLEAR_COLOR)?;
    //   compositor.queue_frame(())?;   // page flip
    // CLEAR_COLOR = [0.08, 0.08, 0.08, 1.0] (matches the old wgpu clear).
}
