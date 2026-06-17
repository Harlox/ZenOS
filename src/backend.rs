//! DRM/KMS backend: ZenOS owns the screen directly, no weston/X11.
//!
//! Multi-output: one DRM device (`Gpu`) drives one `Surface` per connected
//! connector (each its own CRTC + DrmCompositor + Output, positioned
//! left-to-right in the global Space). Connectors are (re)scanned at boot and on
//! udev "change" events, so monitors can be hot-plugged/unplugged.
//!
//! Rendering is event-driven (see `run`): the loop blocks on events and the
//! per-output flip chain is kept alive by VBlanks, so the frame rate tracks each
//! monitor's refresh with no idle busy-spin.

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
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::element::{PixelShaderElement, TextureShaderElement};
use smithay::desktop::{Space, Window};
use smithay::render_elements;
use smithay::utils::{Scale, Transform};
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::Color32F;
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

/// Background clear color (matches the old wgpu clear).
const CLEAR: [f32; 4] = [0.08, 0.08, 0.08, 1.0];
// Slightly translucent UI (fake glass; true backdrop blur is a later pass).
const BAR_COLOR: [f32; 4] = [0.16, 0.16, 0.18, 0.70];
// macOS-style dock: very transparent neutral body (blur dominates, not a milky
// tint) + a barely-there hairline rim (not a bright outline).
const DOCK_COLOR: [f32; 4] = [0.86, 0.87, 0.91, 0.12];
const DOCK_BORDER_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.18];
const DOCK_BORDER_W: f32 = 1.0;
/// Thin vertical separator between dock app groups.
const SEP_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.20];
const BAR_H: i32 = 30;
const DOCK_H: i32 = 64;
const DOCK_MARGIN: i32 = 14; // gap from the bottom of the screen
const ICON_SIZE: i32 = 50;
const ICON_GAP: i32 = 10;
const DOCK_PAD_X: i32 = 14; // dock side padding (left of first icon)
const DOCK_PAD_Y: i32 = (DOCK_H - ICON_SIZE) / 2;
/// Hover magnification (macOS-style): icon under the cursor scales up to MAG_MAX,
/// falling off over MAG_RADIUS px. Icons grow upward from the dock baseline.
const MAG_MAX: f32 = 1.45;
const MAG_RADIUS: f32 = 110.0;
/// Icon corner radius as a fraction of icon size (squircle-ish mask).
const ICON_RADIUS_FRAC: f32 = 0.23;

/// Dock width hugs its content (macOS-style), not a fixed bar.
fn dock_width(n: usize) -> i32 {
    let n = n as i32;
    if n == 0 {
        return 2 * DOCK_PAD_X;
    }
    2 * DOCK_PAD_X + n * ICON_SIZE + (n - 1) * ICON_GAP
}

/// A dock entry: the binary to spawn on click + candidate icon paths (first that
/// exists wins; none -> a colored placeholder square using `placeholder`).
struct DockApp {
    exec: &'static str,
    /// Icon PNG embedded in the binary (works regardless of CWD/install).
    icon: &'static [u8],
    placeholder: [f32; 4],
    /// Draw a group separator immediately before this icon.
    sep_before: bool,
}
const DOCK_APPS: &[DockApp] = &[
    DockApp {
        exec: "thunar",
        icon: include_bytes!("../assets/icons/Finder.png"),
        placeholder: [0.20, 0.55, 0.95, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "firefox",
        icon: include_bytes!("../assets/icons/Safari.png"),
        placeholder: [0.20, 0.55, 0.95, 0.9],
        sep_before: true,
    },
    DockApp {
        exec: "gnome-calendar",
        icon: include_bytes!("../assets/icons/Calendar.png"),
        placeholder: [0.90, 0.30, 0.25, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "gnome-text-editor",
        icon: include_bytes!("../assets/icons/Notes.png"),
        placeholder: [0.95, 0.80, 0.25, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Maps.png"),
        placeholder: [0.45, 0.75, 0.40, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "gnome-calculator",
        icon: include_bytes!("../assets/icons/Calculator.png"),
        placeholder: [0.55, 0.58, 0.66, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Settings.png"),
        placeholder: [0.55, 0.58, 0.66, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/App Store.png"),
        placeholder: [0.20, 0.55, 0.95, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Terminal.png"),
        placeholder: [0.20, 0.22, 0.28, 0.9],
        sep_before: true,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Trash Full.png"),
        placeholder: [0.55, 0.58, 0.66, 0.9],
        sep_before: true,
    },
];
/// xkb keycodes (evdev + 8). smithay's Keycode is xkb-space.
const KEY_ESC: u32 = 9; // evdev KEY_ESC 1 -> quit
const KEY_F1: u32 = 67; // evdev KEY_F1 59 -> spawn a terminal (Enter stays free)
const BAR_RADIUS: f32 = 0.0;
const DOCK_RADIUS: f32 = 20.0;
const CURSOR_SIZE: i32 = 12;
const CURSOR_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 1.0];
/// Left mouse button (evdev BTN_LEFT).
const BTN_LEFT: u32 = 0x110;

// --- macOS-style server-side decorations --------------------------------
/// Titlebar height in px. Drawn above each toplevel's surface.
const TITLEBAR_H: i32 = 28;
const TITLEBAR_COLOR: [f32; 4] = [0.86, 0.86, 0.87, 0.94];
const TITLEBAR_RADIUS: f32 = 10.0;
/// Traffic-light buttons (close/min/max), left-aligned.
const LIGHT_DIA: i32 = 13;
const LIGHT_MARGIN: i32 = 12; // left padding to the first light
const LIGHT_SPACING: i32 = 20; // distance between light left-edges
const LIGHT_CLOSE: [f32; 4] = [1.0, 0.37, 0.34, 1.0]; // #FF5F57
const LIGHT_MIN: [f32; 4] = [1.0, 0.74, 0.18, 1.0]; // #FEBC2E
const LIGHT_MAX: [f32; 4] = [0.16, 0.78, 0.25, 1.0]; // #28C840

/// Rounded-rectangle pixel shader (GLSL ES 100; no #version per smithay).
/// Built-in uniforms: `size` (px), `alpha`. Custom: `u_color`, `u_radius`.
/// `v_coords` is normalized [0,1] across the element.
const ROUNDED_SHADER: &str = r#"
#extension GL_OES_standard_derivatives : enable
// highp: the SDF is computed in pixel coords (up to ~250px from center for the
// dock); mediump (~10-bit mantissa) rounds those off and the corners go fuzzy.
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
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
    // Analytic 1px anti-alias (crisper than smoothstep over 2*fwidth).
    float cov = clamp(0.5 - d / fwidth(d), 0.0, 1.0);
    float a = u_color.a * cov * alpha;
    // smithay expects premultiplied alpha.
    gl_FragColor = vec4(u_color.rgb * a, a);
}
"#;

/// Backdrop blur factor: the wallpaper is pre-blurred at 1/N resolution on the
/// CPU, then sampled (upscaled) under the dock. Bigger = blurrier + cheaper.
const BLUR_DOWNSCALE: i32 = 4;
const BLUR_SIGMA: f32 = 10.0;

/// Texture shader: samples the (pre-blurred) wallpaper and masks it to a rounded
/// rect. Used to draw the frosted backdrop behind the dock. Must mirror
/// smithay's builtin texture shader structure (`//_DEFINES_`, EXTERNAL/NO_ALPHA).
const BLUR_MASK_SHADER: &str = r#"#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
#extension GL_OES_standard_derivatives : enable
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
uniform float u_radius;
uniform vec2 u_size;

float sd_rounded_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}

void main() {
    vec4 c = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    c = vec4(c.rgb, 1.0);
#endif
    vec2 p = v_coords * u_size - u_size * 0.5;
    float d = sd_rounded_box(p, u_size * 0.5, u_radius);
    float cov = clamp(0.5 - d / fwidth(d), 0.0, 1.0);
    // Respect the source alpha (icons have transparent areas; wallpaper is opaque).
    float a = c.a * cov * alpha;
    gl_FragColor = vec4(c.rgb * a, a);
}
"#;

/// Rounded box with a highlight border (inner stroke) — for the macOS-style
/// dock: translucent body + a bright 1px rim.
const BORDERED_SHADER: &str = r#"
#extension GL_OES_standard_derivatives : enable
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
varying vec2 v_coords;
uniform float alpha;
uniform vec4 u_color;
uniform vec4 u_border_color;
uniform float u_border;
uniform float u_radius;
uniform vec2 u_size;

float sd_rounded_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}

void main() {
    vec2 p = v_coords * u_size - u_size * 0.5;
    float d = sd_rounded_box(p, u_size * 0.5, u_radius);
    float fill = clamp(0.5 - d / fwidth(d), 0.0, 1.0);
    // Interior = shape shrunk by the border width; ring = fill - interior.
    float interior = clamp(0.5 - (d + u_border) / fwidth(d), 0.0, 1.0);
    float ring = clamp(fill - interior, 0.0, 1.0);
    vec3 rgb = mix(u_color.rgb, u_border_color.rgb, ring);
    float a = mix(u_color.a, u_border_color.a, ring) * fill * alpha;
    gl_FragColor = vec4(rgb * a, a);
}
"#;

/// Like ROUNDED_SHADER but with independent top/bottom corner radii, so a
/// titlebar can have rounded top corners and a square bottom that meets the
/// window content. `v_coords.y == 0` is the top edge.
const TOP_ROUNDED_SHADER: &str = r#"
#extension GL_OES_standard_derivatives : enable
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
varying vec2 v_coords;
uniform float alpha;
uniform vec4 u_color;
uniform float u_radius_top;
uniform float u_radius_bottom;
uniform vec2 u_size;

void main() {
    vec2 b = u_size * 0.5;
    vec2 p = v_coords * u_size - b;
    // p.y < 0.0 is the top half.
    float r = p.y < 0.0 ? u_radius_top : u_radius_bottom;
    vec2 q = abs(p) - b + r;
    float d = min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
    float cov = clamp(0.5 - d / fwidth(d), 0.0, 1.0);
    float a = u_color.a * cov * alpha;
    gl_FragColor = vec4(u_color.rgb * a, a);
}
"#;

/// The DrmCompositor type for one output: GBM allocator + GBM framebuffer
/// exporter, `()` queue user-data, DrmDeviceFd-backed GBM.
type ZenCompositor =
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

// Text styling.
const BAR_TEXT_PX: f32 = 18.0;
const BAR_TEXT_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 1.0];
const TITLE_PX: f32 = 16.0;
const TITLE_COLOR: [f32; 4] = [0.15, 0.15, 0.16, 1.0];

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
    /// Pre-blurred wallpaper at 1/BLUR_DOWNSCALE res, for the dock backdrop.
    pub wallpaper_blur: Option<TextureBuffer<GlesTexture>>,
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
    /// Texture shader: rounded-masked sampling of the blurred wallpaper.
    pub blur_mask: GlesTexProgram,
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
    /// Render every output that isn't mid-flip.
    pub fn render_all(&mut self, space: &Space<Window>, cursor: (i32, i32)) {
        let crtcs: Vec<crtc::Handle> = self.surfaces.keys().copied().collect();
        for crtc in crtcs {
            match self.render_surface(crtc, space, cursor) {
                Ok(true) => self.frames += 1,
                Ok(false) => {}
                Err(e) => tracing::error!("render surface failed: {e}"),
            }
        }
        // Once a second, report flips/s (≈ FPS of the busiest output). With
        // event-driven rendering this is 0 when idle, ~refresh under activity.
        let elapsed = self.fps_since.elapsed();
        if elapsed.as_secs() >= 1 {
            tracing::info!("{} flips/s", self.frames as f64 / elapsed.as_secs_f64());
            self.frames = 0;
            self.fps_since = std::time::Instant::now();
        }
    }

    /// Render one output: client windows + SSD titlebars below, ZenOS UI on top,
    /// over the wallpaper/clear background, then page-flip. Elements are placed
    /// in this output's local coordinates (global coords minus the output's
    /// location). Returns true if a frame was queued (had damage).
    fn render_surface(
        &mut self,
        crtc: crtc::Handle,
        space: &Space<Window>,
        cursor: (i32, i32),
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Gpu {
            renderer,
            rounded,
            rounded_top,
            bordered,
            blur_mask,
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
        let (w, h) = surface.size;
        let (ox, oy) = surface.location;

        let dw = dock_width(DOCK_APPS.len());
        let dock_x = (w - dw) / 2;
        let dock_y = h - DOCK_H - DOCK_MARGIN;
        let bar = PixelShaderElement::new(
            rounded.clone(),
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
            bordered.clone(),
            Rectangle::from_loc_and_size((dock_x, dock_y), (dw, DOCK_H)),
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
        );

        let cursor_el = PixelShaderElement::new(
            rounded.clone(),
            Rectangle::from_loc_and_size((cursor.0 - ox, cursor.1 - oy), (CURSOR_SIZE, CURSOR_SIZE)),
            None,
            1.0,
            vec![
                Uniform::new("u_color", CURSOR_COLOR),
                Uniform::new("u_radius", 2.0f32),
                Uniform::new("u_size", [CURSOR_SIZE as f32, CURSOR_SIZE as f32]),
            ],
            Kind::Unspecified,
        );

        // Front-to-back: cursor, then text (clock), UI bars, windows.
        let mut elements: Vec<ZenElement> = vec![ZenElement::Ui(cursor_el)];

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
        elements.extend(clock.into_iter().map(ZenElement::Texture));

        elements.push(ZenElement::Ui(bar));

        // Dock app icons (in front of the dock background, pushed before it).
        // macOS-style hover magnification: icons near the cursor scale up and
        // grow upward from the dock baseline. All icons get a rounded (squircle)
        // mask for a uniform look.
        let cursor_lx = cursor.0 - ox;
        let cursor_ly = cursor.1 - oy;
        let hover = cursor_ly >= dock_y - 40; // pointer over/just above the dock
        let baseline = dock_y + DOCK_H - DOCK_PAD_Y; // icon bottom edge
        for (i, app) in DOCK_APPS.iter().enumerate() {
            let (bx, _) = dock_icon_pos(w, h, i, DOCK_APPS.len());

            // Group separator (thin vertical line centered in the gap before).
            if app.sep_before && i > 0 {
                let sh = ICON_SIZE - 16;
                let sx = bx - ICON_GAP / 2;
                let sy = baseline - ICON_SIZE + 8;
                elements.push(ZenElement::Ui(PixelShaderElement::new(
                    rounded.clone(),
                    Rectangle::from_loc_and_size((sx, sy), (2, sh)),
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
                    // Real icons already have rounded/transparent art — draw
                    // them directly (no extra mask that would clip the artwork).
                    elements.push(ZenElement::Texture(TextureRenderElement::from_texture_buffer(
                        Point::from((x as f64, y as f64)),
                        tex,
                        None,
                        None,
                        Some(Size::from((size, size))),
                        Kind::Unspecified,
                    )));
                }
                _ => {
                    // Colored rounded-square placeholder when the icon is missing.
                    elements.push(ZenElement::Ui(PixelShaderElement::new(
                        rounded.clone(),
                        Rectangle::from_loc_and_size((x, y), (size, size)),
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

        elements.push(ZenElement::Ui(dock));

        // Frosted backdrop: blurred wallpaper under the dock, rounded-masked.
        // Behind the translucent dock tint, in front of windows/wallpaper.
        if let Some(blur) = &surface.wallpaper_blur {
            let bd = BLUR_DOWNSCALE as f64;
            let src = Rectangle::<f64, Logical>::from_loc_and_size(
                (dock_x as f64 / bd, dock_y as f64 / bd),
                (dw as f64 / bd, DOCK_H as f64 / bd),
            );
            let inner = TextureRenderElement::from_texture_buffer(
                Point::from((dock_x as f64, dock_y as f64)),
                blur,
                None,
                Some(src),
                Some(Size::from((dw, DOCK_H))),
                Kind::Unspecified,
            );
            let backdrop = TextureShaderElement::new(
                inner,
                blur_mask.clone(),
                vec![
                    Uniform::new("u_radius", DOCK_RADIUS),
                    Uniform::new("u_size", [dw as f32, DOCK_H as f32]),
                ],
            );
            elements.push(ZenElement::Blur(backdrop));
        }
        let scale = Scale::from(1.0);
        for window in space.elements() {
            let g = space.element_location(window).unwrap_or_default();
            let lx = g.x - ox;
            let ly = g.y - oy;
            let geo = window.geometry();

            // macOS-style SSD: titlebar above the surface + traffic lights.
            if geo.size.w > 0 {
                let tx = lx;
                let ty = ly - TITLEBAR_H;
                let titlebar = PixelShaderElement::new(
                    rounded_top.clone(),
                    Rectangle::from_loc_and_size((tx, ty), (geo.size.w, TITLEBAR_H)),
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
                    let light = PixelShaderElement::new(
                        rounded.clone(),
                        Rectangle::from_loc_and_size((lcx, light_y), (LIGHT_DIA, LIGHT_DIA)),
                        None,
                        1.0,
                        vec![
                            Uniform::new("u_color", color),
                            Uniform::new("u_radius", LIGHT_DIA as f32 / 2.0),
                            Uniform::new("u_size", [LIGHT_DIA as f32, LIGHT_DIA as f32]),
                        ],
                        Kind::Unspecified,
                    );
                    elements.push(ZenElement::Ui(light));
                }

                // Centered window title (in front of the titlebar).
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
                    let baseline = ty + TITLEBAR_H / 2 + (TITLE_PX as i32) / 3;
                    let glyphs = text.text(renderer, &title, tx_text, baseline, TITLE_PX, TITLE_COLOR);
                    elements.extend(glyphs.into_iter().map(ZenElement::Texture));
                }

                elements.push(ZenElement::Ui(titlebar));
            }

            let loc = Point::<i32, Logical>::from((lx, ly)).to_physical_precise_round(1.0);
            let rels = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                loc,
                scale,
                1.0,
            );
            elements.extend(rels.into_iter().map(ZenElement::Window));
        }

        // Wallpaper at the very bottom (drawn behind windows + UI). Opaque, so
        // it hides the CLEAR background entirely.
        if let Some(wp) = &surface.wallpaper {
            let element = TextureRenderElement::from_texture_buffer(
                Point::from((0.0, 0.0)),
                wp,
                None,
                None,
                None,
                Kind::Unspecified,
            );
            elements.push(ZenElement::Texture(element));
        }

        let res = surface.compositor.render_frame::<_, ZenElement>(
            renderer,
            &elements,
            Color32F::from(CLEAR),
            FrameFlags::DEFAULT,
        )?;
        if res.is_empty {
            return Ok(false); // no damage -> nothing to flip
        }
        surface.compositor.queue_frame(())?;
        surface.pending_flip = true;
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

    // Wake the event loop when a client sends a request, so the loop can block
    // on events (no busy-poll) yet still service clients promptly. The actual
    // dispatch_clients + flush happen in the main loop; this source only needs
    // to make `dispatch` return. Level-triggered: readiness is cleared when the
    // main loop reads the fd via dispatch_clients on the next iteration.
    let display_fd = display.backend().poll_fd().try_clone_to_owned()?;
    handle.insert_source(
        Generic::new(display_fd, Interest::READ, CalloopMode::Level),
        |_, _, _: &mut ZenState| Ok(PostAction::Continue),
    )?;

    // --- Step 2: udev (GPU discovery) ---------------------------------------
    let udev_backend = UdevBackend::new(&seat_name)?;

    // $ZENOS_GPU forces a specific DRM node (e.g. /dev/dri/card0). On hybrid
    // laptops the external HDMI/DP is often wired to the dGPU, while the iGPU
    // only sees it in degraded modes; pick the GPU the display hangs off.
    let primary = std::env::var("ZENOS_GPU")
        .ok()
        .and_then(|p| DrmNode::from_path(p).ok())
        .or_else(|| {
            primary_gpu(&seat_name)
                .ok()
                .flatten()
                .and_then(|p| DrmNode::from_path(p).ok())
        })
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

    // Open the DRM device, then scan connectors -> one Surface per output.
    let (mut gpu, drm_notifier) = open_device(&mut state.session, primary, &dev_path)?;
    scan_connectors(&mut gpu, &mut state.space, &state.display_handle)?;
    if gpu.surfaces.is_empty() {
        return Err("no connected output".into());
    }
    state.gpu = Some(gpu);

    // DRM VBlank: per-output heartbeat. Ack the finished flip on that CRTC,
    // release clients to draw their next frame (throttled to the monitor
    // refresh), then render again so any damage shows and the chain keeps going.
    handle.insert_source(drm_notifier, move |event, _, data| match event {
        DrmEvent::VBlank(crtc) => {
            if let Some(gpu) = &mut data.gpu {
                if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
                    let _ = surface.compositor.frame_submitted();
                    surface.pending_flip = false;
                }
            }
            data.send_frame_callbacks();
            data.render();
        }
        DrmEvent::Error(e) => tracing::error!("DRM error: {e:?}"),
    })?;

    // udev "change" = a monitor was (un)plugged: rescan connectors and add/remove
    // outputs, then redraw.
    handle.insert_source(udev_backend, move |event, _, data| match event {
        UdevEvent::Changed { device_id } => {
            tracing::info!("udev change {device_id}, rescanning outputs");
            let dh = data.display_handle.clone();
            if let Some(mut gpu) = data.gpu.take() {
                if let Err(e) = scan_connectors(&mut gpu, &mut data.space, &dh) {
                    tracing::error!("rescan failed: {e}");
                }
                data.gpu = Some(gpu);
            }
            data.render();
        }
        UdevEvent::Added { device_id, .. } => tracing::debug!("udev add {device_id}"),
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
            // Clamp to the union of all outputs (multi-monitor desktop spans
            // left-to-right).
            let (mut maxw, mut maxh) = (0i32, 0i32);
            if let Some(gpu) = &data.gpu {
                for s in gpu.surfaces.values() {
                    maxw = maxw.max(s.location.0 + s.size.0);
                    maxh = maxh.max(s.location.1 + s.size.1);
                }
            }
            let mut loc = data.pointer_location;
            loc.x = (loc.x + event.delta_x()).clamp(0.0, maxw as f64);
            loc.y = (loc.y + event.delta_y()).clamp(0.0, maxh as f64);
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

                // Dock launch hit-test (icons live in each output's dock).
                if button == BTN_LEFT {
                    let mut launch = None;
                    if let Some(gpu) = &data.gpu {
                        'outer: for s in gpu.surfaces.values() {
                            let (sw, sh) = s.size;
                            let (ox, oy) = s.location;
                            for (i, app) in DOCK_APPS.iter().enumerate() {
                                let (ix, iy) = dock_icon_pos(sw, sh, i, DOCK_APPS.len());
                                let r = Rectangle::from_loc_and_size(
                                    (ox + ix, oy + iy),
                                    (ICON_SIZE, ICON_SIZE),
                                );
                                if r.to_f64().contains(loc) {
                                    launch = Some(app.exec);
                                    break 'outer;
                                }
                            }
                        }
                    }
                    if let Some(exec) = launch {
                        tracing::info!("dock launch: {exec}");
                        let _ = std::process::Command::new(exec).spawn();
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
                        return;
                    }
                }

                // SSD titlebars live above the surface, outside the Space, so
                // hit-test them manually before the normal surface focus path.
                let deco = {
                    let mut found = None;
                    for window in data.space.elements() {
                        let wl = data.space.element_location(window).unwrap_or_default();
                        let gw = window.geometry().size.w;
                        if gw <= 0 {
                            continue;
                        }
                        let tb = Rectangle::from_loc_and_size(
                            (wl.x, wl.y - TITLEBAR_H),
                            (gw, TITLEBAR_H),
                        );
                        if tb.to_f64().contains(loc) {
                            found = Some((window.clone(), wl));
                            break;
                        }
                    }
                    found
                };
                if let Some((window, wl)) = deco {
                    if let Some(s) = window.toplevel().map(|t| t.wl_surface().clone()) {
                        let keyboard = data.seat.get_keyboard().unwrap();
                        keyboard.set_focus(data, Some(s), serial);
                    }
                    // First traffic light = close.
                    let close = Rectangle::from_loc_and_size(
                        (wl.x + LIGHT_MARGIN, wl.y - TITLEBAR_H + (TITLEBAR_H - LIGHT_DIA) / 2),
                        (LIGHT_DIA, LIGHT_DIA),
                    );
                    if button == BTN_LEFT && close.to_f64().contains(loc) {
                        if let Some(t) = window.toplevel() {
                            t.send_close();
                        }
                    } else if button == BTN_LEFT {
                        // Drag the titlebar to move (no modifier, macOS-style).
                        data.move_grab = Some(MoveGrab {
                            window,
                            start_ptr: loc,
                            start_win: wl,
                        });
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
                    return;
                }

                // Click on window content = focus only. Moving is done by
                // dragging the titlebar (handled above), no modifier needed.
                let under = data.space.element_under(loc).map(|(w, _)| w.clone());
                if let Some(window) = under {
                    if let Some(s) = window.toplevel().map(|t| t.wl_surface().clone()) {
                        let keyboard = data.seat.get_keyboard().unwrap();
                        keyboard.set_focus(data, Some(s), serial);
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

    // 1Hz tick so the clock redraws even when otherwise idle (minute rollover).
    handle.insert_source(
        Timer::from_duration(Duration::from_secs(1)),
        |_, _, data: &mut ZenState| {
            data.render();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        },
    )?;

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

    // Kick the first frame on every output to start their flip chains.
    state.render();

    // --- Dispatch loop -------------------------------------------------------
    // Event-driven: block until something happens (input, client request, or a
    // VBlank), then service it and try to render. Rendering itself is gated by
    // pending_flip + damage, and the VBlank handler keeps the flip chain going,
    // so the effective frame rate tracks the monitor's refresh (60/120/165Hz)
    // with no busy-spin when idle. Only poll on a timer when an auto-exit
    // deadline is set, so it can still fire with no events.
    let tick = deadline.map(|_| Duration::from_millis(200));
    tracing::info!("ZenOS compositor running");
    while state.running {
        event_loop.dispatch(tick, &mut state)?;

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

/// Open the DRM device + GBM allocator + EGL/GLES renderer and compile the
/// shaders. Outputs are added later by `scan_connectors`.
fn open_device(
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
fn scan_connectors(
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
    let wallpaper_blur = load_wallpaper_blur(&mut gpu.renderer, size.0, size.1);
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
            wallpaper_blur,
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

/// Position outputs left-to-right (stable order by CRTC) in the global Space.
fn relayout_outputs(gpu: &mut Gpu, space: &mut Space<Window>) {
    // crtc::Handle isn't Ord; order by output name for a stable left-to-right
    // layout (e.g. eDP-* then HDMI-A-*).
    let mut crtcs: Vec<crtc::Handle> = gpu.surfaces.keys().copied().collect();
    crtcs.sort_by_key(|c| gpu.surfaces[c].output.name());
    let mut x = 0;
    for crtc in crtcs {
        if let Some(s) = gpu.surfaces.get_mut(&crtc) {
            s.location = (x, 0);
            space.map_output(&s.output, (x, 0));
            x += s.size.0;
        }
    }
}

/// Top-left (output-local) px of dock icon `i` of `n`.
fn dock_icon_pos(w: i32, h: i32, i: usize, n: usize) -> (i32, i32) {
    let dw = dock_width(n);
    let dock_x = (w - dw) / 2;
    let dock_y = h - DOCK_H - DOCK_MARGIN;
    let x = dock_x + DOCK_PAD_X + i as i32 * (ICON_SIZE + ICON_GAP);
    let y = dock_y + DOCK_PAD_Y;
    (x, y)
}

/// Texture resolution for dock icons (> ICON_SIZE so magnified icons stay crisp).
const ICON_TEX: i32 = 128;

/// Decode an embedded icon PNG and upload it as a square texture.
fn load_icon(renderer: &mut GlesRenderer, bytes: &[u8]) -> Option<TextureBuffer<GlesTexture>> {
    let img = image::load_from_memory(bytes).ok()?;
    let scaled =
        img.resize_to_fill(ICON_TEX as u32, ICON_TEX as u32, image::imageops::FilterType::Lanczos3);
    let rgba = scaled.to_rgba8();
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
fn load_wallpaper(
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

/// Load + heavily blur the wallpaper at 1/BLUR_DOWNSCALE resolution, for the
/// dock's frosted backdrop. Downscaling first makes the blur cheap and stronger.
fn load_wallpaper_blur(
    renderer: &mut GlesRenderer,
    w: i32,
    h: i32,
) -> Option<TextureBuffer<GlesTexture>> {
    let path = std::env::var("ZENOS_WALLPAPER")
        .unwrap_or_else(|_| "/usr/local/share/zenos/wallpaper.png".to_string());
    let img = image::open(&path).ok()?;
    let bw = (w / BLUR_DOWNSCALE).max(1);
    let bh = (h / BLUR_DOWNSCALE).max(1);
    let small = img.resize_to_fill(bw as u32, bh as u32, image::imageops::FilterType::Triangle);
    let blurred = small.blur(BLUR_SIGMA);
    let rgba = blurred.to_rgba8();
    TextureBuffer::from_memory(
        renderer,
        rgba.as_raw(),
        Fourcc::Abgr8888,
        (bw, bh),
        false,
        1,
        Transform::Normal,
        None,
    )
    .ok()
}
