use crate::renderer::Vertex;

pub fn vertices(sw: f32, sh: f32) -> Vec<Vertex> {
    let w = 500.0;
    let h = 65.0;
    let x = (sw - w) / 2.0;
    let y = sh - h - 15.0;
    ndc_rect(x, y, w, h, sw, sh, [0.25, 0.25, 0.25, 1.0], 16.0)
}

fn ndc_rect(x: f32, y: f32, w: f32, h: f32, sw: f32, sh: f32, color: [f32; 4], radius: f32) -> Vec<Vertex> {
    let x1 = (x / sw) * 2.0 - 1.0;
    let y1 = 1.0 - (y / sh) * 2.0;
    let x2 = ((x + w) / sw) * 2.0 - 1.0;
    let y2 = 1.0 - ((y + h) / sh) * 2.0;
    let size = [w, h];
    let v = |position: [f32; 2], uv: [f32; 2]| Vertex {
        position,
        color,
        uv,
        size,
        radius,
        _pad: 0.0,
    };
    vec![
        v([x1, y1], [0.0, 0.0]),
        v([x2, y1], [1.0, 0.0]),
        v([x1, y2], [0.0, 1.0]),
        v([x2, y1], [1.0, 0.0]),
        v([x2, y2], [1.0, 1.0]),
        v([x1, y2], [0.0, 1.0]),
    ]
}
