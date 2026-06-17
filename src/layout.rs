//! Output placement + dock icon geometry. Pure layout math, kept out of the
//! render passes so positioning bugs map here.

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
use smithay::input::keyboard::{keysyms, FilterResult, Keysym};
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
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Offscreen};
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

use crate::config::*;
use crate::shaders::*;
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

