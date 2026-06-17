//! Texture/asset loading: dock icon PNGs and the wallpaper, decoded on the CPU
//! and uploaded as premultiplied GLES textures.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::TextureBuffer;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Rectangle, Transform};

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
