//! DRM/KMS device setup: open the GPU, build the GLES renderer + shaders,
//! scan connectors, and create one scanout `Surface` per output. The `Gpu`/
//! `Surface` types and rendering live in `render.rs`.

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::DrmCompositor;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::gles::{GlesRenderer, UniformName, UniformType};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::Offscreen;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::drm::control::{connector, crtc, Device as _, Mode as DrmMode, ResourceHandles};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{DeviceFd, Size, Transform};

use std::collections::{HashMap, HashSet};

use crate::text::TextRenderer;
use crate::state::ZenState;
use crate::config::*;
use crate::shaders::*;
use crate::render::*;
use crate::assets::{load_icon, load_wallpaper};
use crate::layout::relayout_outputs;

/// Open the DRM device + GBM allocator + EGL/GLES renderer and compile the
/// shaders. Outputs are added later by `scan_connectors`.
pub fn open_device(
    session: &mut LibSeatSession,
    node: DrmNode,
    path: &std::path::Path,
) -> Result<(Gpu, DrmDeviceNotifier), Box<dyn std::error::Error>> {
    let fd = session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, drm_notifier) = DrmDevice::new(drm_fd.clone(), true)?;
    let gbm = GbmDevice::new(drm_fd)?;

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };

    // Rounded-rect shader, reused by the bar, dock, lights and cursor.
    let rounded = renderer.compile_custom_pixel_shader(
        ROUNDED_SHADER,
        &[
            UniformName::new("u_color", UniformType::_4f),
            UniformName::new("u_radius", UniformType::_1f),
            UniformName::new("u_size", UniformType::_2f),
        ],
    )?;

    // Titlebar shader with separate top/bottom radii.
    let rounded_top = renderer.compile_custom_pixel_shader(
        TOP_ROUNDED_SHADER,
        &[
            UniformName::new("u_color", UniformType::_4f),
            UniformName::new("u_radius_top", UniformType::_1f),
            UniformName::new("u_radius_bottom", UniformType::_1f),
            UniformName::new("u_size", UniformType::_2f),
        ],
    )?;

    // Dock shader: rounded body + highlight border.
    let bordered = renderer.compile_custom_pixel_shader(
        BORDERED_SHADER,
        &[
            UniformName::new("u_color", UniformType::_4f),
            UniformName::new("u_border_color", UniformType::_4f),
            UniformName::new("u_border", UniformType::_1f),
            UniformName::new("u_radius", UniformType::_1f),
            UniformName::new("u_size", UniformType::_2f),
        ],
    )?;

    // Frosted-backdrop texture shader (samples blurred wallpaper, rounded mask).
    let blur_mask = renderer.compile_custom_texture_shader(
        BLUR_MASK_SHADER,
        &[
            UniformName::new("u_radius", UniformType::_1f),
            UniformName::new("u_size", UniformType::_2f),
            UniformName::new("u_texel", UniformType::_2f),
            UniformName::new("u_src_origin", UniformType::_2f),
            UniformName::new("u_src_size", UniformType::_2f),
        ],
    )?;

    let icon_mask = renderer.compile_custom_texture_shader(
        ICON_MASK_SHADER,
        &[
            UniformName::new("u_radius", UniformType::_1f),
            UniformName::new("u_size", UniformType::_2f),
        ],
    )?;

    // Dock icons (load once at device open).
    let dock_icons = DOCK_APPS
        .iter()
        .map(|app| load_icon(&mut renderer, app.icon))
        .collect();

    Ok((
        Gpu {
            node,
            drm,
            gbm,
            allocator,
            renderer,
            rounded,
            rounded_top,
            bordered,
            blur_mask,
            icon_mask,
            dock_icons,
            surfaces: HashMap::new(),
            frames: 0,
            fps_since: std::time::Instant::now(),
            text: TextRenderer::new(),
        },
        drm_notifier,
    ))
}

/// (Re)scan the device's connectors. Drops outputs whose connector disconnected,
/// adds a Surface for each newly connected one, and re-lays-out the outputs.
/// Returns true if anything changed.
pub fn scan_connectors(
    gpu: &mut Gpu,
    space: &mut Space<Window>,
    dh: &DisplayHandle,
) -> Result<bool, Box<dyn std::error::Error>> {
    let res = gpu.drm.resource_handles()?;
    let connected: Vec<connector::Info> = res
        .connectors()
        .iter()
        // force_probe=true: re-read EDID so we get the full mode list (some
        // drivers cache a degraded set otherwise).
        .filter_map(|h| gpu.drm.get_connector(*h, true).ok())
        .filter(|c| c.state() == connector::State::Connected)
        .collect();
    let connected_handles: HashSet<connector::Handle> =
        connected.iter().map(|c| c.handle()).collect();

    let mut changed = false;

    // Remove outputs whose connector is gone.
    let gone: Vec<crtc::Handle> = gpu
        .surfaces
        .iter()
        .filter(|(_, s)| !connected_handles.contains(&s.connector))
        .map(|(crtc, _)| *crtc)
        .collect();
    for crtc in gone {
        if let Some(s) = gpu.surfaces.remove(&crtc) {
            tracing::info!("output removed: {:?}", s.output.name());
            space.unmap_output(&s.output);
            dh.remove_global::<ZenState>(s.global);
            changed = true;
        }
    }

    // Add outputs for newly connected connectors.
    let have: HashSet<connector::Handle> =
        gpu.surfaces.values().map(|s| s.connector).collect();
    for conn in &connected {
        if have.contains(&conn.handle()) {
            continue;
        }
        match create_surface(gpu, conn, &res, dh) {
            Ok(()) => changed = true,
            Err(e) => tracing::error!("failed to add output {:?}: {e}", conn.interface()),
        }
    }

    if changed {
        relayout_outputs(gpu, space);
    }
    Ok(changed)
}

/// Build a Surface (CRTC + scanout compositor + Output + global) for one
/// connected connector and insert it into the Gpu.
fn create_surface(
    gpu: &mut Gpu,
    conn: &connector::Info,
    res: &ResourceHandles,
    dh: &DisplayHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    // Log every mode the connector exposes (helps diagnose missing resolutions).
    for m in conn.modes() {
        tracing::info!(
            "  mode available: {}x{}@{}Hz",
            m.size().0,
            m.size().1,
            m.vrefresh()
        );
    }
    let mode = pick_mode(conn.modes()).ok_or("connector has no modes")?;

    // Pick a CRTC drivable by this connector's encoders that isn't already used.
    let used: HashSet<crtc::Handle> = gpu.surfaces.keys().copied().collect();
    let crtc = conn
        .encoders()
        .iter()
        .filter_map(|e| gpu.drm.get_encoder(*e).ok())
        .flat_map(|enc| res.filter_crtcs(enc.possible_crtcs()))
        .find(|c| !used.contains(c))
        .ok_or("no free CRTC for connector")?;

    let drm_surface = gpu.drm.create_surface(crtc, mode, &[conn.handle()])?;

    let name = format!("{:?}-{}", conn.interface(), conn.interface_id());
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "ZenOS".into(),
            model: name.clone(),
        },
    );
    let wl_mode = OutputMode {
        size: (mode.size().0 as i32, mode.size().1 as i32).into(),
        refresh: mode.vrefresh() as i32 * 1000,
    };
    output.change_current_state(Some(wl_mode), None, None, None);
    output.set_preferred(wl_mode);

    let render_formats = gpu.renderer.egl_context().dmabuf_render_formats().clone();
    let compositor = DrmCompositor::new(
        &output,
        drm_surface,
        None,
        gpu.allocator.clone(),
        GbmFramebufferExporter::new(gpu.gbm.clone(), Some(gpu.node)),
        [Fourcc::Argb8888, Fourcc::Xrgb8888],
        render_formats,
        (64u32, 64u32).into(),
        Some(gpu.gbm.clone()),
    )?;

    let size = (mode.size().0 as i32, mode.size().1 as i32);
    let wallpaper = load_wallpaper(&mut gpu.renderer, size.0, size.1);
    let scene_tex = gpu
        .renderer
        .create_buffer(Fourcc::Abgr8888, Size::from((size.0, size.1)))?;
    let scene_damage = OutputDamageTracker::new((size.0, size.1), 1.0, Transform::Normal);
    let global = output.create_global::<ZenState>(dh);

    tracing::info!(
        "output added: {name} {}x{}@{}Hz",
        size.0,
        size.1,
        mode.vrefresh()
    );

    gpu.surfaces.insert(
        crtc,
        Surface {
            connector: conn.handle(),
            output,
            global,
            compositor,
            size,
            location: (0, 0),
            pending_flip: false,
            wallpaper,
            scene_tex,
            scene_damage,
        },
    );
    Ok(())
}

/// Pick a DRM mode. Honors `$ZENOS_MODE` ("WxH" or "WxH@Hz", applied to any
/// connector that offers it); otherwise the highest resolution, tie-broken on
/// refresh. A connector without the requested mode falls back to its max.
fn pick_mode(modes: &[DrmMode]) -> Option<DrmMode> {
    if let Ok(spec) = std::env::var("ZENOS_MODE") {
        let (res, rate) = match spec.split_once('@') {
            Some((r, hz)) => (r, hz.parse::<u32>().ok()),
            None => (spec.as_str(), None),
        };
        if let Some((ws, hs)) = res.split_once('x') {
            if let (Ok(w), Ok(h)) = (ws.trim().parse::<u16>(), hs.trim().parse::<u16>()) {
                let found = modes
                    .iter()
                    .copied()
                    .filter(|m| m.size() == (w, h))
                    .filter(|m| rate.map_or(true, |r| m.vrefresh() == r))
                    .max_by_key(|m| m.vrefresh());
                if let Some(m) = found {
                    tracing::info!(
                        "ZENOS_MODE={spec} -> {}x{}@{}Hz",
                        m.size().0,
                        m.size().1,
                        m.vrefresh()
                    );
                    return Some(m);
                }
                tracing::warn!("ZENOS_MODE={spec} not offered by this output; using max");
            }
        }
    }
    modes.iter().copied().max_by_key(|m| {
        let (w, h) = m.size();
        (w as u64 * h as u64, m.vrefresh())
    })
}
