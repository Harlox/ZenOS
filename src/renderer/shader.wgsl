struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) size: vec2<f32>,
    @location(4) radius: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) radius: f32,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    out.uv = in.uv;
    out.size = in.size;
    out.radius = in.radius;
    return out;
}

// Signed distance to a rounded box centered at origin.
// p: point, b: half-extents, r: corner radius.
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let local = (in.uv - 0.5) * in.size;          // pixel coord, centered
    let d = sd_rounded_box(local, in.size * 0.5, in.radius);
    let aa = fwidth(d);                            // antialias edge width
    let alpha = 1.0 - smoothstep(-aa, aa, d);      // smooth coverage
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
