//! Rendering: the GPU/output model (`Gpu`, `Surface`), the per-frame compose
//! + scanout passes, output layout, and texture/asset loading. DRM device
//! setup lives in `drm.rs`; the event loop in `backend.rs`.

use smithay::backend::allocator::gbm::{GbmAllocator, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmNode};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::element::{PixelShaderElement, TextureShaderElement};
use smithay::desktop::{Space, Window};
use smithay::render_elements;
use smithay::utils::{Scale, Transform};
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Offscreen};
use smithay::output::Output;
use smithay::reexports::drm::control::{connector, crtc};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use std::collections::HashMap;

use crate::text::TextRenderer;

use crate::config::*;
use crate::shaders::*;
use crate::layout::{dock_icon_pos, power_btn_rect, power_menu_item_rect, power_menu_rect};
use crate::assets::ICON_TEX;

/// The DrmCompositor type for one output: GBM allocator + GBM framebuffer
/// exporter, `()` queue user-data, DrmDeviceFd-backed GBM.
pub type ZenCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

render_elements! {
    /// One frame's elements: client window surfaces + ZenOS UI (bar/dock).
    /// Renderer-specific (GlesRenderer) since PixelShaderElement is GLES-only.
    pub ZenElement<=GlesRenderer>;
    Window = WaylandSurfaceRenderElement<GlesRenderer>,
    Ui = PixelShaderElement,
    // Any GLES texture: wallpaper + text glyphs + dock icons.
    Texture = TextureRenderElement<GlesTexture>,
    // Texture sampled through a custom shader (frosted dock backdrop).
    Blur = TextureShaderElement,
}


/// One connected output: its CRTC's scanout compositor + logical Output, placed
/// at `location` in the global Space.
pub struct Surface {
    pub connector: connector::Handle,
    pub output: Output,
    pub global: GlobalId,
    pub compositor: ZenCompositor,
    /// Screen size in px (from the DRM mode).
    pub size: (i32, i32),
    /// Top-left position of this output in the global Space.
    pub location: (i32, i32),
    /// True while a page-flip is in flight (cleared on this output's VBlank).
    pub pending_flip: bool,
    /// Fullscreen wallpaper, pre-scaled to this output. None = flat CLEAR bg.
    pub wallpaper: Option<TextureBuffer<GlesTexture>>,
    /// Offscreen texture holding the composed scene (wallpaper + windows + bar).
    /// Rendered each frame, then drawn fullscreen on the scanout and
    /// sampled+blurred for the dock's frosted backdrop.
    pub scene_tex: GlesTexture,
    /// Damage tracker for the offscreen scene pass.
    pub scene_damage: OutputDamageTracker,
}

/// One GPU: the DRM device, GBM allocator, GLES renderer, shaders, and one
/// scanout Surface per connected output (keyed by CRTC).
pub struct Gpu {
    pub node: DrmNode,
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub allocator: GbmAllocator<DrmDeviceFd>,
    pub renderer: GlesRenderer,
    /// Compiled rounded-rect pixel shader, reused for bar + dock + lights.
    pub rounded: GlesPixelProgram,
    /// Top-only rounded-rect shader, for SSD titlebars.
    pub rounded_top: GlesPixelProgram,
    /// Rounded-rect-with-border shader, for the dock.
    pub bordered: GlesPixelProgram,
    /// Texture shader: gaussian-blurred + rounded-masked sampling (dock backdrop).
    pub blur_mask: GlesTexProgram,
    /// Texture shader: rounded mask for dock icons (squircle clip).
    pub icon_mask: GlesTexProgram,
    /// Dock app icons (device-level, loaded once), one per DOCK_APPS entry.
    /// None = icon file missing -> a placeholder square is drawn.
    pub dock_icons: Vec<Option<TextureBuffer<GlesTexture>>>,
    /// One Surface per connected output, keyed by its CRTC handle.
    pub surfaces: HashMap<crtc::Handle, Surface>,
    /// Frames flipped since `fps_since`, for the once-a-second FPS log.
    pub frames: u32,
    pub fps_since: std::time::Instant,
    /// Glyph cache / text renderer (clock, titles).
    pub text: TextRenderer,
}

impl Gpu {
    /// Render every output that isn't mid-flip. Returns true if all outputs were
    /// rendered (none skipped for an in-flight flip), so the caller can clear the
    /// dirty flag.
    pub fn render_all(
        &mut self,
        space: &Space<Window>,
        cursor: (i32, i32),
        shot: bool,
        scene_dirty: bool,
        menu_open: bool,
    ) -> bool {
        let crtcs: Vec<crtc::Handle> = self.surfaces.keys().copied().collect();
        let mut all_done = true;
        for crtc in crtcs {
            match self.render_surface(crtc, space, cursor, shot, scene_dirty, menu_open) {
                Ok(true) => self.frames += 1,
                Ok(false) => all_done = false, // mid-flip; retry after its VBlank
                Err(e) => tracing::error!("render surface failed: {e}"),
            }
        }
        let elapsed = self.fps_since.elapsed();
        if elapsed.as_secs() >= 1 {
            tracing::info!("{} flips/s", self.frames as f64 / elapsed.as_secs_f64());
            self.frames = 0;
            self.fps_since = std::time::Instant::now();
        }
        all_done
    }

    /// Render one output in two passes:
    ///  1. Compose the scene (wallpaper + windows + SSD + bar + clock) into an
    ///     offscreen texture.
    ///  2. Scanout: draw that scene fullscreen, then the dock — whose frosted
    ///     backdrop samples + blurs the scene under it (so windows behind the
    ///     dock are blurred too) — then the cursor.
    /// Returns true if a frame was queued. Only called when something changed
    /// (the caller's dirty flag), so the offscreen is fully re-composed each time.
    fn render_surface(
        &mut self,
        crtc: crtc::Handle,
        space: &Space<Window>,
        cursor: (i32, i32),
        shot: bool,
        scene_dirty: bool,
        menu_open: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Gpu {
            renderer,
            rounded,
            rounded_top,
            bordered,
            blur_mask,
            icon_mask,
            dock_icons,
            surfaces,
            text,
            ..
        } = self;
        let Some(surface) = surfaces.get_mut(&crtc) else {
            return Ok(false);
        };
        if surface.pending_flip {
            return Ok(false);
        }
        let Surface {
            scene_tex,
            scene_damage,
            compositor,
            wallpaper,
            size,
            location,
            pending_flip,
            ..
        } = surface;
        let (w, h) = *size;
        let (ox, oy) = *location;

        let dw = dock_width(DOCK_APPS.len());
        let dock_x = (w - dw) / 2;
        let dock_y = h - DOCK_H - DOCK_MARGIN;
        let scale = Scale::from(1.0);

        // --- Pass 1: compose the scene into the offscreen texture --------------
        let mut scene: Vec<ZenElement> = Vec::new();

        // Top-bar clock, right-aligned.
        let now = chrono::Local::now().format("%H:%M").to_string();
        let cw = text.measure(renderer, &now, BAR_TEXT_PX);
        let clock = text.text(
            renderer,
            &now,
            w - cw - 14,
            BAR_H / 2 + (BAR_TEXT_PX as i32) / 3,
            BAR_TEXT_PX,
            BAR_TEXT_COLOR,
        );
        scene.extend(clock.into_iter().map(ZenElement::Texture));

        // Top bar.
        scene.push(ZenElement::Ui(PixelShaderElement::new(
            rounded.clone(),
            Rectangle::new(Point::from((0, 0)), Size::from((w, BAR_H))),
            None,
            1.0,
            vec![
                Uniform::new("u_color", BAR_COLOR),
                Uniform::new("u_radius", BAR_RADIUS),
                Uniform::new("u_size", [w as f32, BAR_H as f32]),
            ],
            Kind::Unspecified,
        )));

        // space.elements() is bottom-to-top; the scene list is front-to-back
        // (index 0 = topmost). Reverse so the topmost window draws on top.
        for window in space.elements().rev() {
            let g = space.element_location(window).unwrap_or_default();
            let lx = g.x - ox;
            let ly = g.y - oy;
            let geo = window.geometry();

            if geo.size.w > 0 {
                let tx = lx;
                let ty = ly - TITLEBAR_H;
                let titlebar = PixelShaderElement::new(
                    rounded_top.clone(),
                    Rectangle::new(Point::from((tx, ty)), Size::from((geo.size.w, TITLEBAR_H))),
                    None,
                    1.0,
                    vec![
                        Uniform::new("u_color", TITLEBAR_COLOR),
                        Uniform::new("u_radius_top", TITLEBAR_RADIUS),
                        Uniform::new("u_radius_bottom", 0.0f32),
                        Uniform::new("u_size", [geo.size.w as f32, TITLEBAR_H as f32]),
                    ],
                    Kind::Unspecified,
                );
                let light_y = ty + (TITLEBAR_H - LIGHT_DIA) / 2;
                for (i, color) in [LIGHT_CLOSE, LIGHT_MIN, LIGHT_MAX].into_iter().enumerate() {
                    let lcx = tx + LIGHT_MARGIN + i as i32 * LIGHT_SPACING;
                    scene.push(ZenElement::Ui(PixelShaderElement::new(
                        rounded.clone(),
                        Rectangle::new(Point::from((lcx, light_y)), Size::from((LIGHT_DIA, LIGHT_DIA))),
                        None,
                        1.0,
                        vec![
                            Uniform::new("u_color", color),
                            Uniform::new("u_radius", LIGHT_DIA as f32 / 2.0),
                            Uniform::new("u_size", [LIGHT_DIA as f32, LIGHT_DIA as f32]),
                        ],
                        Kind::Unspecified,
                    )));
                }

                let title = window
                    .toplevel()
                    .and_then(|t| {
                        with_states(t.wl_surface(), |states| {
                            states
                                .data_map
                                .get::<XdgToplevelSurfaceData>()
                                .and_then(|d| d.lock().unwrap().title.clone())
                        })
                    })
                    .unwrap_or_default();
                if !title.is_empty() {
                    let tw_text = text.measure(renderer, &title, TITLE_PX);
                    let tx_text = tx + (geo.size.w - tw_text) / 2;
                    let bl = ty + TITLEBAR_H / 2 + (TITLE_PX as i32) / 3;
                    let glyphs = text.text(renderer, &title, tx_text, bl, TITLE_PX, TITLE_COLOR);
                    scene.extend(glyphs.into_iter().map(ZenElement::Texture));
                }

                scene.push(ZenElement::Ui(titlebar));
            }

            let loc = Point::<i32, Logical>::from((lx, ly)).to_physical_precise_round(1.0);
            let rels = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                loc,
                scale,
                1.0,
            );
            scene.extend(rels.into_iter().map(ZenElement::Window));
        }

        // Wallpaper at the very bottom of the scene.
        if let Some(wp) = wallpaper {
            scene.push(ZenElement::Texture(TextureRenderElement::from_texture_buffer(
                Point::from((0.0, 0.0)),
                wp,
                None,
                None,
                None,
                Kind::Unspecified,
            )));
        }

        // Render the scene into scene_tex — only when the scene actually changed.
        // On pure pointer motion the cached scene_tex is reused, so only the
        // cheap scanout pass (cursor + dock) below runs.
        if scene_dirty || shot {
            let mut fb = renderer.bind(&mut *scene_tex)?;
            scene_damage.render_output(renderer, &mut fb, 0, &scene, Color32F::from(CLEAR))?;
        }

        // --- Pass 2: scanout — scene fullscreen + dock (frosted) + cursor ------
        let scene_buf =
            TextureBuffer::from_texture(&*renderer, scene_tex.clone(), 1, Transform::Normal, None);

        let cursor_el = PixelShaderElement::new(
            rounded.clone(),
            Rectangle::new(Point::from((cursor.0 - ox, cursor.1 - oy)), Size::from((CURSOR_SIZE, CURSOR_SIZE))),
            None,
            1.0,
            vec![
                Uniform::new("u_color", CURSOR_COLOR),
                Uniform::new("u_radius", 2.0f32),
                Uniform::new("u_size", [CURSOR_SIZE as f32, CURSOR_SIZE as f32]),
            ],
            Kind::Unspecified,
        );

        // Front-to-back overlay.
        let mut overlay: Vec<ZenElement> = vec![ZenElement::Ui(cursor_el)];

        // Power button (top-left of the bar) + dropdown. Overlay-only, so opening
        // it or hovering an item never triggers a scene recompose.
        {
            let (bx, by, bw, bh) = power_btn_rect();
            let gw = text.measure(renderer, POWER_GLYPH, POWER_GLYPH_PX);
            let gbl = by + bh / 2 + (POWER_GLYPH_PX as i32) / 3;
            for g in text.text(renderer, POWER_GLYPH, bx + (bw - gw) / 2, gbl, POWER_GLYPH_PX, POWER_GLYPH_COLOR) {
                overlay.push(ZenElement::Texture(g));
            }
            overlay.push(ZenElement::Ui(PixelShaderElement::new(
                rounded.clone(),
                Rectangle::new(Point::from((bx, by)), Size::from((bw, bh))),
                None,
                1.0,
                vec![
                    Uniform::new("u_color", POWER_BTN_BG),
                    Uniform::new("u_radius", POWER_BTN_RADIUS),
                    Uniform::new("u_size", [bw as f32, bh as f32]),
                ],
                Kind::Unspecified,
            )));

            if menu_open {
                let clx = cursor.0 - ox;
                let cly = cursor.1 - oy;
                // Item labels (drawn first = on top of the panel/highlight).
                for (i, label) in POWER_ITEMS.iter().enumerate() {
                    let (ix, iy, _iw, ih) = power_menu_item_rect(i as i32);
                    let bl = iy + ih / 2 + (MENU_ITEM_PX as i32) / 3;
                    for g in text.text(renderer, label, ix + 12, bl, MENU_ITEM_PX, MENU_TEXT) {
                        overlay.push(ZenElement::Texture(g));
                    }
                }
                // Hover highlight (below labels, above the panel).
                for i in 0..POWER_ITEMS.len() as i32 {
                    let (ix, iy, iw, ih) = power_menu_item_rect(i);
                    if clx >= ix && clx < ix + iw && cly >= iy && cly < iy + ih {
                        overlay.push(ZenElement::Ui(PixelShaderElement::new(
                            rounded.clone(),
                            Rectangle::new(Point::from((ix, iy)), Size::from((iw, ih))),
                            None,
                            1.0,
                            vec![
                                Uniform::new("u_color", MENU_HOVER),
                                Uniform::new("u_radius", MENU_ITEM_RADIUS),
                                Uniform::new("u_size", [iw as f32, ih as f32]),
                            ],
                            Kind::Unspecified,
                        )));
                    }
                }
                // Panel background (bottom of the menu stack).
                let (mx, my, mw, mh) = power_menu_rect();
                overlay.push(ZenElement::Ui(PixelShaderElement::new(
                    rounded.clone(),
                    Rectangle::new(Point::from((mx, my)), Size::from((mw, mh))),
                    None,
                    1.0,
                    vec![
                        Uniform::new("u_color", MENU_BG),
                        Uniform::new("u_radius", MENU_RADIUS),
                        Uniform::new("u_size", [mw as f32, mh as f32]),
                    ],
                    Kind::Unspecified,
                )));
            }
        }

        // Dock icons + separators (with hover magnification).
        let cursor_lx = cursor.0 - ox;
        let cursor_ly = cursor.1 - oy;
        let hover = cursor_ly >= dock_y - 40;
        let baseline = dock_y + DOCK_H - DOCK_PAD_Y;
        for (i, app) in DOCK_APPS.iter().enumerate() {
            let (bx, _) = dock_icon_pos(w, h, i, DOCK_APPS.len());

            if app.sep_before && i > 0 {
                let sh = ICON_SIZE - 16;
                let sx = bx - ICON_GAP / 2;
                let sy = baseline - ICON_SIZE + 8;
                overlay.push(ZenElement::Ui(PixelShaderElement::new(
                    rounded.clone(),
                    Rectangle::new(Point::from((sx, sy)), Size::from((2, sh))),
                    None,
                    1.0,
                    vec![
                        Uniform::new("u_color", SEP_COLOR),
                        Uniform::new("u_radius", 1.0f32),
                        Uniform::new("u_size", [2.0f32, sh as f32]),
                    ],
                    Kind::Unspecified,
                )));
            }

            let icon_cx = bx + ICON_SIZE / 2;
            let mag = if hover {
                let dist = (cursor_lx - icon_cx).abs() as f32;
                1.0 + (MAG_MAX - 1.0) * (1.0 - (dist / MAG_RADIUS)).max(0.0)
            } else {
                1.0
            };
            let size = (ICON_SIZE as f32 * mag).round() as i32;
            let x = icon_cx - size / 2;
            let y = baseline - size;
            let radius = size as f32 * ICON_RADIUS_FRAC;
            match dock_icons.get(i) {
                Some(Some(tex)) => {
                    let src = Rectangle::<f64, Logical>::new(Point::from((0.0, 0.0)), Size::from((ICON_TEX as f64, ICON_TEX as f64)));
                    let inner = TextureRenderElement::from_texture_buffer(
                        Point::from((x as f64, y as f64)),
                        tex,
                        None,
                        Some(src),
                        Some(Size::from((size, size))),
                        Kind::Unspecified,
                    );
                    // Squircle-mask the icon so its corners are uniformly rounded
                    // (kills the square texture-edge artifact).
                    overlay.push(ZenElement::Blur(TextureShaderElement::new(
                        inner,
                        icon_mask.clone(),
                        vec![
                            Uniform::new("u_radius", radius),
                            Uniform::new("u_size", [size as f32, size as f32]),
                        ],
                    )));
                }
                _ => {
                    overlay.push(ZenElement::Ui(PixelShaderElement::new(
                        rounded.clone(),
                        Rectangle::new(Point::from((x, y)), Size::from((size, size))),
                        None,
                        1.0,
                        vec![
                            Uniform::new("u_color", app.placeholder),
                            Uniform::new("u_radius", radius),
                            Uniform::new("u_size", [size as f32, size as f32]),
                        ],
                        Kind::Unspecified,
                    )));
                }
            }
        }

        // Dock tint.
        overlay.push(ZenElement::Ui(PixelShaderElement::new(
            bordered.clone(),
            Rectangle::new(Point::from((dock_x, dock_y)), Size::from((dw, DOCK_H))),
            None,
            1.0,
            vec![
                Uniform::new("u_color", DOCK_COLOR),
                Uniform::new("u_border_color", DOCK_BORDER_COLOR),
                Uniform::new("u_border", DOCK_BORDER_W),
                Uniform::new("u_radius", DOCK_RADIUS),
                Uniform::new("u_size", [dw as f32, DOCK_H as f32]),
            ],
            Kind::Unspecified,
        )));

        // Frosted backdrop: blur the scene under the dock, rounded-masked.
        let src = Rectangle::<f64, Logical>::new(Point::from((dock_x as f64, dock_y as f64)), Size::from((dw as f64, DOCK_H as f64)));
        let inner = TextureRenderElement::from_texture_buffer(
            Point::from((dock_x as f64, dock_y as f64)),
            &scene_buf,
            None,
            Some(src),
            Some(Size::from((dw, DOCK_H))),
            Kind::Unspecified,
        );
        overlay.push(ZenElement::Blur(TextureShaderElement::new(
            inner,
            blur_mask.clone(),
            vec![
                Uniform::new("u_radius", DOCK_RADIUS),
                Uniform::new("u_size", [dw as f32, DOCK_H as f32]),
                // v_coords is normalized over the FULL scene texture, so a 1px
                // step is 1/scene_size (NOT 1/dock_size — that streaks badly).
                Uniform::new("u_texel", [BLUR_STEP / w as f32, BLUR_STEP / h as f32]),
                // src sub-rect (dock region) in full-texture coords, for the mask.
                Uniform::new("u_src_origin", [dock_x as f32 / w as f32, dock_y as f32 / h as f32]),
                Uniform::new("u_src_size", [dw as f32 / w as f32, DOCK_H as f32 / h as f32]),
            ],
        )));

        // Composed scene, fullscreen, at the bottom.
        overlay.push(ZenElement::Texture(TextureRenderElement::from_texture_buffer(
            Point::from((0.0, 0.0)),
            &scene_buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )));

        let res = compositor.render_frame::<_, ZenElement>(
            renderer,
            &overlay,
            Color32F::from(CLEAR),
            FrameFlags::DEFAULT,
        )?;
        if res.is_empty {
            return Ok(false);
        }

        // F2 screenshot: render into a readable offscreen texture. The live
        // overlay samples scene_tex for the frosted dock; keep the screenshot
        // path simpler so readback stays reliable across GLES drivers.
        if shot {
            let mut shot_elements: Vec<ZenElement> = Vec::new();
            shot_elements.push(ZenElement::Ui(PixelShaderElement::new(
                rounded.clone(),
                Rectangle::new(Point::from((cursor.0 - ox, cursor.1 - oy)), Size::from((CURSOR_SIZE, CURSOR_SIZE))),
                None,
                1.0,
                vec![
                    Uniform::new("u_color", CURSOR_COLOR),
                    Uniform::new("u_radius", 2.0f32),
                    Uniform::new("u_size", [CURSOR_SIZE as f32, CURSOR_SIZE as f32]),
                ],
                Kind::Unspecified,
            )));
            for (i, app) in DOCK_APPS.iter().enumerate() {
                let (bx, _) = dock_icon_pos(w, h, i, DOCK_APPS.len());
                if app.sep_before && i > 0 {
                    let sh = ICON_SIZE - 16;
                    let sx = bx - ICON_GAP / 2;
                    let sy = baseline - ICON_SIZE + 8;
                    shot_elements.push(ZenElement::Ui(PixelShaderElement::new(
                        rounded.clone(),
                        Rectangle::new(Point::from((sx, sy)), Size::from((2, sh))),
                        None,
                        1.0,
                        vec![
                            Uniform::new("u_color", SEP_COLOR),
                            Uniform::new("u_radius", 1.0f32),
                            Uniform::new("u_size", [2.0f32, sh as f32]),
                        ],
                        Kind::Unspecified,
                    )));
                }

                let icon_cx = bx + ICON_SIZE / 2;
                let mag = if hover {
                    let dist = (cursor_lx - icon_cx).abs() as f32;
                    1.0 + (MAG_MAX - 1.0) * (1.0 - (dist / MAG_RADIUS)).max(0.0)
                } else {
                    1.0
                };
                let size = (ICON_SIZE as f32 * mag).round() as i32;
                let x = icon_cx - size / 2;
                let y = baseline - size;
                match dock_icons.get(i) {
                    Some(Some(tex)) => {
                        let src = Rectangle::<f64, Logical>::new(Point::from((0.0, 0.0)), Size::from((ICON_TEX as f64, ICON_TEX as f64)));
                        shot_elements.push(ZenElement::Texture(TextureRenderElement::from_texture_buffer(
                            Point::from((x as f64, y as f64)),
                            tex,
                            None,
                            Some(src),
                            Some(Size::from((size, size))),
                            Kind::Unspecified,
                        )));
                    }
                    _ => {
                        shot_elements.push(ZenElement::Ui(PixelShaderElement::new(
                            rounded.clone(),
                            Rectangle::new(Point::from((x, y)), Size::from((size, size))),
                            None,
                            1.0,
                            vec![
                                Uniform::new("u_color", app.placeholder),
                                Uniform::new("u_radius", size as f32 * ICON_RADIUS_FRAC),
                                Uniform::new("u_size", [size as f32, size as f32]),
                            ],
                            Kind::Unspecified,
                        )));
                    }
                }
            }
            shot_elements.push(ZenElement::Ui(PixelShaderElement::new(
                bordered.clone(),
                Rectangle::new(Point::from((dock_x, dock_y)), Size::from((dw, DOCK_H))),
                None,
                1.0,
                vec![
                    Uniform::new("u_color", DOCK_COLOR),
                    Uniform::new("u_border_color", DOCK_BORDER_COLOR),
                    Uniform::new("u_border", DOCK_BORDER_W),
                    Uniform::new("u_radius", DOCK_RADIUS),
                    Uniform::new("u_size", [dw as f32, DOCK_H as f32]),
                ],
                Kind::Unspecified,
            )));
            shot_elements.extend(scene);

            let mut capture = || -> Result<(), Box<dyn std::error::Error>> {
                let mut shot_tex: GlesTexture =
                    renderer.create_buffer(Fourcc::Abgr8888, Size::from((w, h)))?;
                let mut tracker = OutputDamageTracker::new((w, h), 1.0, Transform::Normal);
                let mut fb = renderer.bind(&mut shot_tex)?;
                tracker.render_output(renderer, &mut fb, 0, &shot_elements, Color32F::from(CLEAR))?;
                let region = Rectangle::new(Point::from((0, 0)), Size::from((w, h)));
                let mapping = renderer.copy_framebuffer(&fb, region, Fourcc::Abgr8888)?;
                let bytes = renderer.map_texture(&mapping)?;
                image::save_buffer(
                    "/tmp/zenos-shot.png",
                    bytes,
                    w as u32,
                    h as u32,
                    image::ExtendedColorType::Rgba8,
                )?;
                Ok(())
            };
            match capture() {
                Ok(()) => tracing::info!("screenshot saved to /tmp/zenos-shot.png"),
                Err(e) => tracing::error!("screenshot failed: {e}"),
            }
        }

        compositor.queue_frame(())?;
        *pending_flip = true;
        Ok(true)
    }
}

