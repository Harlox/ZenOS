//! Output placement + dock icon geometry. Pure layout math, kept out of the
//! render passes so positioning bugs map here.

use smithay::desktop::{Space, Window};
use smithay::reexports::drm::control::crtc;

use crate::config::*;
use crate::render::Gpu;

/// Position outputs left-to-right (stable order by CRTC) in the global Space.
pub fn relayout_outputs(gpu: &mut Gpu, space: &mut Space<Window>) {
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
pub fn dock_icon_pos(w: i32, h: i32, i: usize, n: usize) -> (i32, i32) {
    let dw = dock_width(n);
    let dock_x = (w - dw) / 2;
    let dock_y = h - DOCK_H - DOCK_MARGIN;
    let x = dock_x + DOCK_PAD_X + i as i32 * (ICON_SIZE + ICON_GAP);
    let y = dock_y + DOCK_PAD_Y;
    (x, y)
}

