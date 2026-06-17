//! GLSL ES fragment shaders for the ZenOS shell (rounded rects, frosted blur,
//! icon masks, titlebars). Kept apart from `backend.rs` so the render logic
//! reads clean and shader edits don't touch Rust control flow.

/// Rounded-rectangle pixel shader (GLSL ES 100; no #version per smithay).
/// Built-in uniforms: `size` (px), `alpha`. Custom: `u_color`, `u_radius`.
/// `v_coords` is normalized [0,1] across the element.
pub const ROUNDED_SHADER: &str = r#"
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

/// Backdrop blur sampling step in px (9x9 kernel reaches ~4*BLUR_STEP px).
pub const BLUR_STEP: f32 = 3.0;

/// Texture shader for the frosted dock backdrop: gaussian-blurs the sampled
/// scene texture (9x9 kernel stepped by u_texel) and masks it to a rounded rect.
/// Must mirror smithay's builtin texture shader (`//_DEFINES_`, EXTERNAL).
pub const BLUR_MASK_SHADER: &str = r#"#version 100
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
uniform vec2 u_texel;
// The sampled src sub-rect in full-texture normalized coords, so we can map
// v_coords back to element-local 0..1 for the rounded mask.
uniform vec2 u_src_origin;
uniform vec2 u_src_size;

float sd_rounded_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}

void main() {
    vec3 acc = vec3(0.0);
    float wsum = 0.0;
    for (int yy = -4; yy <= 4; yy++) {
        for (int xx = -4; xx <= 4; xx++) {
            float fx = float(xx);
            float fy = float(yy);
            float wgt = exp(-(fx * fx + fy * fy) / 8.0);
            acc += texture2D(tex, v_coords + vec2(fx, fy) * u_texel).rgb * wgt;
            wsum += wgt;
        }
    }
    vec3 col = acc / wsum;
    vec2 local = (v_coords - u_src_origin) / u_src_size; // 0..1 over the dock
    vec2 p = local * u_size - u_size * 0.5;
    float d = sd_rounded_box(p, u_size * 0.5, u_radius);
    float cov = clamp(0.5 - d / fwidth(d), 0.0, 1.0);
    float a = cov * alpha;
    gl_FragColor = vec4(col * a, a);
}
"#;

/// Rounded mask for dock icons: samples the (premultiplied) icon and clips it to
/// a rounded square, removing the square texture edge / corner artifacts.
pub const ICON_MASK_SHADER: &str = r#"#version 100
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
    vec2 p = v_coords * u_size - u_size * 0.5;
    float d = sd_rounded_box(p, u_size * 0.5, u_radius);
    float cov = clamp(0.5 - d / fwidth(d), 0.0, 1.0);
    // c is premultiplied; scaling rgb+a by coverage keeps it premultiplied.
    gl_FragColor = c * (cov * alpha);
}
"#;

/// Rounded box with a highlight border (inner stroke) — for the macOS-style
/// dock: translucent body + a bright 1px rim.
pub const BORDERED_SHADER: &str = r#"
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
pub const TOP_ROUNDED_SHADER: &str = r#"
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
