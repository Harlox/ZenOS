//! Texture/asset loading: dock icon PNGs and the wallpaper, decoded on the CPU
//! and uploaded as premultiplied GLES textures.

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
use smithay::input::keyboard::{keysyms, FilterResult, Keysym};
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::reexports::input::Libinput;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::element::{PixelShaderElement, TextureShaderElement};
use smithay::desktop::{Space, Window};
use smithay::render_elements;
use smithay::utils::{Scale, Transform};
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Offscreen};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use smithay::reexports::drm::control::{connector, crtc, Device as _, Mode as DrmMode, ResourceHandles};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{DeviceFd, Logical, Point, Rectangle, Size, SERIAL_COUNTER};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::socket::ListeningSocketSource;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::state::{ClientState, MoveGrab};
use crate::text::TextRenderer;

use crate::state::ZenState;

use crate::config::*;
use crate::shaders::*;

/// Texture resolution for dock icons (> ICON_SIZE so magnified icons stay crisp).
pub const ICON_TEX: i32 = 128;

/// Decode an embedded icon PNG and upload it as a square texture. The pixels are
/// premultiplied (smithay blends premultiplied alpha) so the icons' transparent
/// corners and anti-aliased edges render correctly.
pub fn load_icon(renderer: &mut GlesRenderer, bytes: &[u8]) -> Option<TextureBuffer<GlesTexture>> {
    let img = image::load_from_memory(bytes).ok()?;
    let scaled =
        img.resize_to_fill(ICON_TEX as u32, ICON_TEX as u32, image::imageops::FilterType::Lanczos3);
    let mut rgba = scaled.to_rgba8();
    for px in rgba.pixels_mut() {
        let alpha = px[3] as u16;
        px[0] = (px[0] as u16 * alpha / 255) as u8;
        px[1] = (px[1] as u16 * alpha / 255) as u8;
        px[2] = (px[2] as u16 * alpha / 255) as u8;
    }
    TextureBuffer::from_memory(
        renderer,
        rgba.as_raw(),
        Fourcc::Abgr8888,
        (ICON_TEX, ICON_TEX),
        false,
        1,
        Transform::Normal,
        None,
    )
    .ok()
}

/// Load the wallpaper from `$ZENOS_WALLPAPER` (default
/// `/usr/local/share/zenos/wallpaper.png`), cover-scale it to the output on the
/// CPU, and upload as a GLES texture. Returns None (flat CLEAR bg) on any error.
pub fn load_wallpaper(
    renderer: &mut GlesRenderer,
    w: i32,
    h: i32,
) -> Option<TextureBuffer<GlesTexture>> {
    let path = std::env::var("ZENOS_WALLPAPER")
        .unwrap_or_else(|_| "/usr/local/share/zenos/wallpaper.png".to_string());
    let img = match image::open(&path) {
        Ok(img) => img,
        Err(e) => {
            tracing::warn!("no wallpaper at {path} ({e}); using flat background");
            return None;
        }
    };
    // Cover-scale: fill w×h exactly, cropping overflow, no distortion.
    let scaled = img.resize_to_fill(w as u32, h as u32, image::imageops::FilterType::Lanczos3);
    let rgba = scaled.to_rgba8();
    // image RGBA8 byte order is R,G,B,A == DRM Abgr8888 (little-endian).
    match TextureBuffer::from_memory(
        renderer,
        rgba.as_raw(),
        Fourcc::Abgr8888,
        (w, h),
        false,
        1,
        Transform::Normal,
        Some(vec![Rectangle::from_size((w, h).into())]),
    ) {
        Ok(buf) => {
            tracing::info!("wallpaper loaded from {path} ({w}x{h})");
            Some(buf)
        }
        Err(e) => {
            tracing::error!("wallpaper import failed: {e}");
            None
        }
    }
}
