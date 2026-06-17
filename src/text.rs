//! CPU glyph rasterization (fontdue) cached as GLES textures, for on-screen
//! text: the top-bar clock, window titles, etc. Each glyph is rasterized once
//! per (char, pixel-size, color) and reused.

use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Point, Transform};

/// A rasterized glyph: its texture (None for blank glyphs like space) plus
/// placement metrics relative to the pen position / baseline.
struct Glyph {
    tex: Option<TextureBuffer<GlesTexture>>,
    width: i32,
    height: i32,
    /// x offset from the pen to the bitmap's left edge.
    left: i32,
    /// y distance from the baseline up to the bitmap's top edge.
    top: i32,
    /// horizontal advance to the next glyph.
    advance: i32,
}

/// Text renderer with a per-(char,size,color) glyph cache.
pub struct TextRenderer {
    font: Option<fontdue::Font>,
    /// key: (char, px size, packed rgb).
    cache: HashMap<(char, u32, u32), Glyph>,
}

impl TextRenderer {
    pub fn new() -> Self {
        TextRenderer {
            font: load_font(),
            cache: HashMap::new(),
        }
    }

    fn pack_color(color: [f32; 4]) -> u32 {
        let r = (color[0].clamp(0.0, 1.0) * 255.0) as u32;
        let g = (color[1].clamp(0.0, 1.0) * 255.0) as u32;
        let b = (color[2].clamp(0.0, 1.0) * 255.0) as u32;
        (r << 16) | (g << 8) | b
    }

    fn glyph(
        &mut self,
        renderer: &mut GlesRenderer,
        c: char,
        px: f32,
        color: [f32; 4],
    ) -> Option<&Glyph> {
        let font = self.font.as_ref()?;
        let key = (c, px as u32, Self::pack_color(color));
        if !self.cache.contains_key(&key) {
            let (metrics, coverage) = font.rasterize(c, px);
            let (cr, cg, cb) = (color[0], color[1], color[2]);
            let glyph = if metrics.width == 0 || metrics.height == 0 {
                Glyph {
                    tex: None,
                    width: 0,
                    height: 0,
                    left: metrics.xmin,
                    top: metrics.ymin + metrics.height as i32,
                    advance: metrics.advance_width.round() as i32,
                }
            } else {
                // Coverage (8-bit) -> premultiplied RGBA (smithay wants premult).
                let mut rgba = Vec::with_capacity(coverage.len() * 4);
                for &cov in &coverage {
                    let a = cov as f32 / 255.0;
                    rgba.push((cr * a * 255.0) as u8);
                    rgba.push((cg * a * 255.0) as u8);
                    rgba.push((cb * a * 255.0) as u8);
                    rgba.push(cov);
                }
                let tex = TextureBuffer::from_memory(
                    renderer,
                    &rgba,
                    Fourcc::Abgr8888,
                    (metrics.width as i32, metrics.height as i32),
                    false,
                    1,
                    Transform::Normal,
                    None,
                )
                .ok();
                Glyph {
                    tex,
                    width: metrics.width as i32,
                    height: metrics.height as i32,
                    left: metrics.xmin,
                    top: metrics.ymin + metrics.height as i32,
                    advance: metrics.advance_width.round() as i32,
                }
            };
            self.cache.insert(key, glyph);
        }
        self.cache.get(&key)
    }

    /// Width in px the string would occupy at `px` (for centering/right-align).
    pub fn measure(&mut self, renderer: &mut GlesRenderer, s: &str, px: f32) -> i32 {
        let mut w = 0;
        for c in s.chars() {
            if let Some(g) = self.glyph(renderer, c, px, [1.0; 4]) {
                w += g.advance;
            }
        }
        w
    }

    /// Build render elements for `s`, with the left edge at `x` and the baseline
    /// at `baseline` (both in output-local px). Returns one textured quad per
    /// visible glyph.
    pub fn text(
        &mut self,
        renderer: &mut GlesRenderer,
        s: &str,
        x: i32,
        baseline: i32,
        px: f32,
        color: [f32; 4],
    ) -> Vec<TextureRenderElement<GlesTexture>> {
        let mut out = Vec::new();
        let mut pen = x;
        for c in s.chars() {
            let Some(g) = self.glyph(renderer, c, px, color) else {
                continue;
            };
            if let Some(tex) = &g.tex {
                let gx = pen + g.left;
                let gy = baseline - g.top;
                out.push(TextureRenderElement::from_texture_buffer(
                    Point::from((gx as f64, gy as f64)),
                    tex,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            pen += g.advance;
        }
        out
    }
}

/// Find a usable TTF: `$ZENOS_FONT` first, then common Arch font paths.
fn load_font() -> Option<fontdue::Font> {
    let mut paths: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("ZENOS_FONT") {
        paths.push(p);
    }
    paths.extend(
        [
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/Hack-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
        ]
        .into_iter()
        .map(String::from),
    );
    for p in paths {
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(f) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                tracing::info!("font loaded: {p}");
                return Some(f);
            }
        }
    }
    tracing::warn!("no font found (set $ZENOS_FONT); text disabled");
    None
}
