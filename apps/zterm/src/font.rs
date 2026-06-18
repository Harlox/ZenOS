//! Monospace glyph cache for the terminal grid. Rasterizes each char once
//! (grayscale coverage via fontdue) and blits it with a per-cell foreground
//! colour over the already-filled background — so colour isn't baked into the
//! cache (a cell's fg changes constantly, the glyph shape doesn't).

use std::collections::HashMap;

/// One rasterized glyph: coverage bitmap + placement metrics.
struct Glyph {
    cov: Vec<u8>,
    w: usize,
    h: usize,
    /// x offset from the cell's left to the bitmap.
    left: i32,
    /// y offset from the baseline up to the bitmap top.
    top: i32,
}

pub struct Font {
    font: fontdue::Font,
    px: f32,
    /// Fixed cell box, in px.
    pub cell_w: usize,
    pub cell_h: usize,
    /// Baseline from the cell top.
    ascent: i32,
    cache: HashMap<char, Glyph>,
}

impl Font {
    /// Load a monospace TTF at `px`. Prefers Helvetica-ish mono clones, then the
    /// usual Linux mono fonts; `$ZTERM_FONT` overrides.
    pub fn load(px: f32) -> Option<Font> {
        let mut paths: Vec<String> = Vec::new();
        if let Ok(p) = std::env::var("ZTERM_FONT") {
            paths.push(p);
        }
        paths.extend(
            [
                "/usr/share/fonts/gsfonts/NimbusMonoPS-Regular.otf",
                "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
                "/usr/share/fonts/TTF/LiberationMono-Regular.ttf",
                "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
                "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/TTF/Hack-Regular.ttf",
            ]
            .into_iter()
            .map(String::from),
        );
        let font = paths.into_iter().find_map(|p| {
            std::fs::read(&p)
                .ok()
                .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
        })?;

        // Cell width = advance of a representative glyph (mono → uniform).
        let cell_w = font.metrics('M', px).advance_width.round().max(1.0) as usize;
        let lm = font.horizontal_line_metrics(px)?;
        let cell_h = lm.new_line_size.ceil().max(1.0) as usize;
        let ascent = lm.ascent.ceil() as i32;
        Some(Font {
            font,
            px,
            cell_w,
            cell_h,
            ascent,
            cache: HashMap::new(),
        })
    }

    fn glyph(&mut self, c: char) -> &Glyph {
        self.cache.entry(c).or_insert_with(|| {
            let (m, cov) = self.font.rasterize(c, self.px);
            Glyph {
                cov,
                w: m.width,
                h: m.height,
                left: m.xmin,
                top: m.ymin + m.height as i32,
            }
        })
    }

    /// Blit `c` in colour `fg` (0xRRGGBB) into `buf` (ARGB 0xAARRGGBB, opaque),
    /// at cell-top-left (`x0`,`y0`) px. Background is assumed already filled.
    pub fn draw(&mut self, buf: &mut [u32], stride: usize, height: usize, x0: usize, y0: usize, c: char, fg: u32) {
        if c == ' ' || c == '\0' {
            return;
        }
        let ascent = self.ascent;
        let g = self.glyph(c);
        if g.w == 0 || g.h == 0 {
            return;
        }
        let (fr, fgc, fb) = ((fg >> 16) & 0xff, (fg >> 8) & 0xff, fg & 0xff);
        for gy in 0..g.h {
            let py = y0 as i32 + ascent - g.top + gy as i32;
            if py < 0 || py as usize >= height {
                continue;
            }
            for gx in 0..g.w {
                let px = x0 as i32 + g.left + gx as i32;
                if px < 0 || px as usize >= stride {
                    continue;
                }
                let a = g.cov[gy * g.w + gx] as u32;
                if a == 0 {
                    continue;
                }
                let idx = py as usize * stride + px as usize;
                let dst = buf[idx];
                let (dr, dg, db) = ((dst >> 16) & 0xff, (dst >> 8) & 0xff, dst & 0xff);
                // Alpha blend fg over dst.
                let nr = (fr * a + dr * (255 - a)) / 255;
                let ng = (fgc * a + dg * (255 - a)) / 255;
                let nb = (fb * a + db * (255 - a)) / 255;
                buf[idx] = 0xff00_0000 | (nr << 16) | (ng << 8) | nb;
            }
        }
    }
}
