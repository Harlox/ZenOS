use crate::renderer::Vertex;

pub fn vertices(sw: f32, sh: f32) -> Vec<Vertex> {
    let w = 500.0;
    let h = 65.0;
    let x = (sw - w) / 2.0;
    let y = sh - h - 15.0;
    ndc_rect(x, y, w, h, sw, sh, [0.25, 0.25, 0.25, 1.0])
}

fn ndc_rect(x: f32, y: f32, w: f32, h: f32, sw: f32, sh: f32, color: [f32; 4]) -> Vec<Vertex> {
    let x1 = (x / sw) * 2.0 - 1.0;
    let y1 = 1.0 - (y / sh) * 2.0;
    let x2 = ((x + w) / sw) * 2.0 - 1.0;
    let y2 = 1.0 - ((y + h) / sh) * 2.0;
    vec![
        Vertex { position: [x1, y1], color },
        Vertex { position: [x2, y1], color },
        Vertex { position: [x1, y2], color },
        Vertex { position: [x2, y1], color },
        Vertex { position: [x2, y2], color },
        Vertex { position: [x1, y2], color },
    ]
}