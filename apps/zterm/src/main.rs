//! zterm — ZenOS's native terminal, a Wayland client (Linux-only).
//!
//! A pty runs the shell; `vt100` turns its output into a cell grid; we paint the
//! grid into an shm buffer with a monospace font (white background, per-cell
//! colours) and forward keystrokes back to the pty. The window is a plain
//! xdg-toplevel, so ZenOS supplies the (white, rounded) decoration, move/resize
//! and dock entry — zterm only owns its content.

mod font;
mod pty;

use std::io::{Read, Write};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{channel, Event as ChannelEvent};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_keyboard::WlKeyboard, wl_pointer::WlPointer, wl_seat::WlSeat, wl_shm, wl_surface::WlSurface,
};
use wayland_client::{Connection, QueueHandle};

use font::Font;
use pty::Pty;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<State> = event_queue.handle();
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle.clone())?;

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let font = Font::load(16.0).ok_or("no monospace font found (set $ZTERM_FONT)")?;
    let (cols, rows) = (80u16, 24u16);

    // pty + parser. Read the master on a thread, funnel bytes to the loop via a
    // calloop channel so the Wayland queue and shell output share one poll.
    let pty = Pty::spawn(rows, cols)?;
    let writer = pty.writer()?;
    let mut reader = pty.reader()?;
    let (tx, rx) = channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("zterm");
    window.set_app_id("dev.zenos.zterm");
    window.set_min_size(Some((font.cell_w as u32 * 8, font.cell_h as u32 * 2)));
    window.commit();

    let pool = SlotPool::new((cols as usize * font.cell_w) * (rows as usize * font.cell_h) * 4, &shm)?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        window,
        keyboard: None,
        pointer: None,
        mods: Modifiers::default(),
        scroll_off: 0,
        qh: Some(qh.clone()),
        width: cols as u32 * font.cell_w as u32,
        height: rows as u32 * font.cell_h as u32,
        configured: false,
        exit: false,
        dirty: true,
        frame_pending: false,
        font,
        parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
        writer,
        pty,
        cols,
        rows,
    };

    // Shell output → feed parser, repaint.
    loop_handle.insert_source(rx, move |event, _, state: &mut State| match event {
        ChannelEvent::Msg(bytes) => {
            state.parser.process(&bytes);
            state.dirty = true;
            if state.configured {
                let qh = state.qh_dummy();
                state.request_draw(&qh);
            }
        }
        ChannelEvent::Closed => state.exit = true,
    })?;

    let signal = event_loop.get_signal();
    event_loop.run(None, &mut state, move |state| {
        if state.exit {
            signal.stop();
        }
    })?;
    Ok(())
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    mods: Modifiers,
    /// Current scrollback offset (0 = live bottom).
    scroll_off: usize,
    qh: Option<QueueHandle<State>>,

    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
    /// Content changed since the last paint.
    dirty: bool,
    /// A frame callback is in flight: don't paint again until it fires, so we
    /// draw at most once per vblank instead of once per pty read.
    frame_pending: bool,

    font: Font,
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    pty: Pty,
    cols: u16,
    rows: u16,
}

impl State {
    fn qh_dummy(&self) -> QueueHandle<State> {
        self.qh.clone().expect("qh set after loop init")
    }

    /// Paint only if nothing is already queued for this vblank. Coalesces a
    /// burst of pty output into a single frame.
    fn request_draw(&mut self, qh: &QueueHandle<State>) {
        if self.configured && !self.frame_pending {
            self.draw(qh);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<State>) {
        self.dirty = false;
        self.frame_pending = true;
        let (w, h) = (self.width as usize, self.height as usize);

        let stride = self.width as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(self.width as i32, self.height as i32, stride, wl_shm::Format::Argb8888)
            .expect("create shm buffer");
        // Render straight into the shm canvas (Argb8888 = native-endian u32),
        // no intermediate scratch buffer or per-pixel copy.
        let px: &mut [u32] = bytemuck::cast_slice_mut(canvas);
        render_grid(&mut self.font, &self.parser, self.rows, self.cols, px, w, h);

        let surface = self.window.wl_surface();
        surface.attach(Some(buffer.wl_buffer()), 0, 0);
        surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
        surface.frame(qh, surface.clone());
        self.window.commit();
    }

    /// Move the scrollback view. `up` rows toward history, `down` toward live.
    fn scroll(&mut self, up: usize, down: usize) {
        let off = (self.scroll_off + up).saturating_sub(down);
        self.parser.set_scrollback(off);
        // Re-read: set_scrollback clamps to the real history length.
        self.scroll_off = self.parser.screen().scrollback();
        self.dirty = true;
    }

    fn on_key(&mut self, event: KeyEvent) {
        // Typing returns to the live screen, like every terminal.
        if self.scroll_off != 0 {
            self.scroll_off = 0;
            self.parser.set_scrollback(0);
            self.dirty = true;
        }
        let m = self.mods;
        // Named keys: fixed control sequences. Shift+Tab is back-tab (CBT).
        let named: Option<Vec<u8>> = match event.keysym {
            Keysym::Return | Keysym::KP_Enter => Some(vec![b'\r']),
            Keysym::BackSpace => Some(vec![0x7f]),
            Keysym::Tab if m.shift => Some(b"\x1b[Z".to_vec()),
            Keysym::Tab => Some(vec![b'\t']),
            Keysym::Escape => Some(vec![0x1b]),
            Keysym::Left => Some(b"\x1b[D".to_vec()),
            Keysym::Right => Some(b"\x1b[C".to_vec()),
            Keysym::Up => Some(b"\x1b[A".to_vec()),
            Keysym::Down => Some(b"\x1b[B".to_vec()),
            Keysym::Home => Some(b"\x1b[H".to_vec()),
            Keysym::End => Some(b"\x1b[F".to_vec()),
            Keysym::Page_Up => Some(b"\x1b[5~".to_vec()),
            Keysym::Page_Down => Some(b"\x1b[6~".to_vec()),
            Keysym::Insert => Some(b"\x1b[2~".to_vec()),
            Keysym::Delete => Some(b"\x1b[3~".to_vec()),
            _ => None,
        };

        let mut bytes = if let Some(b) = named {
            b
        } else if m.ctrl {
            // Ctrl+key → C0 control code, derived from the keysym so it's
            // independent of how xkb folds control into utf8.
            match ctrl_code(event.keysym.raw()) {
                Some(b) => vec![b],
                None => event.utf8.map(String::into_bytes).unwrap_or_default(),
            }
        } else {
            event.utf8.map(String::into_bytes).unwrap_or_default()
        };

        // Alt acts as Meta: prefix ESC (xterm "metaSendsEscape").
        if m.alt && !bytes.is_empty() {
            bytes.insert(0, 0x1b);
        }
        if !bytes.is_empty() {
            let _ = self.writer.write_all(&bytes);
            let _ = self.writer.flush();
        }
    }
}

/// Paint the whole grid into `px` (ARGB 0xAARRGGBB), white background.
fn render_grid(
    font: &mut Font,
    parser: &vt100::Parser,
    rows: u16,
    cols: u16,
    px: &mut [u32],
    w: usize,
    h: usize,
) {
    px[..w * h].fill(0xffff_ffff);
    let screen = parser.screen();
    let (cw, ch) = (font.cell_w, font.cell_h);
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let x = col as usize * cw;
            let y = row as usize * ch;
            let bg = color(cell.bgcolor(), 0xffffff);
            if bg != 0xffffff {
                fill_rect(px, w, h, x, y, cw, ch, 0xff00_0000 | bg);
            }
            if cell.has_contents() {
                if let Some(c) = cell.contents().chars().next() {
                    let fg = color(cell.fgcolor(), 0x000000);
                    font.draw(px, w, h, x, y, c, fg);
                }
            }
        }
    }
    // Block cursor: dark fill, glyph redrawn white on top. Hidden while the
    // user is scrolled up into history.
    if !screen.hide_cursor() && screen.scrollback() == 0 {
        let (cr, cc) = screen.cursor_position();
        let x = cc as usize * cw;
        let y = cr as usize * ch;
        fill_rect(px, w, h, x, y, cw, ch, 0xff20_2020);
        if let Some(cell) = screen.cell(cr, cc) {
            if cell.has_contents() {
                if let Some(c) = cell.contents().chars().next() {
                    font.draw(px, w, h, x, y, c, 0xffffff);
                }
            }
        }
    }

    // Bottom corners rounded (top corners stay square under the SSD titlebar).
    round_bottom_corners(px, w, h, CORNER_RADIUS);
}

const CORNER_RADIUS: usize = 10;
const SCROLLBACK_LINES: usize = 10_000;
/// Rows moved per wheel notch.
const SCROLL_STEP: usize = 3;

/// Make the two bottom corners transparent outside `r`, anti-aliased. Premultiplied
/// alpha (wl_shm Argb8888), so partial-coverage pixels scale rgb by coverage too.
fn round_bottom_corners(px: &mut [u32], w: usize, h: usize, r: usize) {
    if r == 0 || w < 2 * r || h < r {
        return;
    }
    let rf = r as f32;
    for cy in 0..r {
        let yb = h - 1 - cy;
        for cx in 0..r {
            let dx = rf - cx as f32 - 0.5;
            let dy = rf - cy as f32 - 0.5;
            let cov = (rf - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            if cov >= 1.0 {
                continue;
            }
            for x in [cx, w - 1 - cx] {
                let idx = yb * w + x;
                let p = px[idx];
                let a = (cov * 255.0) as u32;
                let (pr, pg, pb) = ((p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff);
                px[idx] = (a << 24) | ((pr * a / 255) << 16) | ((pg * a / 255) << 8) | (pb * a / 255);
            }
        }
    }
}

fn fill_rect(px: &mut [u32], w: usize, h: usize, x: usize, y: usize, rw: usize, rh: usize, argb: u32) {
    for yy in y..(y + rh).min(h) {
        let base = yy * w;
        for xx in x..(x + rw).min(w) {
            px[base + xx] = argb;
        }
    }
}

/// C0 control byte for Ctrl+<key>, from the raw keysym (ASCII range). Covers
/// Ctrl-A..Z (1..26) plus @ [ \ ] ^ _ ? — None for keys with no control code.
fn ctrl_code(keysym: u32) -> Option<u8> {
    match keysym {
        0x61..=0x7a => Some(keysym as u8 - b'a' + 1), // a-z
        0x41..=0x5a => Some(keysym as u8 - b'A' + 1), // A-Z (shifted)
        0x20 | 0x40 => Some(0),                       // space, @
        0x5b => Some(27),                             // [
        0x5c => Some(28),                             // \
        0x5d => Some(29),                             // ]
        0x5e => Some(30),                             // ^
        0x5f => Some(31),                             // _
        0x3f => Some(127),                            // ?
        _ => None,
    }
}

/// Map a vt100 colour to 0xRRGGBB, `default` for `Color::Default`.
fn color(c: vt100::Color, default: u32) -> u32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => ansi256(i),
        vt100::Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    }
}

fn ansi256(i: u8) -> u32 {
    const BASE16: [u32; 16] = [
        0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
        0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    match i {
        0..=15 => BASE16[i as usize],
        16..=231 => {
            let n = i as u32 - 16;
            let comp = |x: u32| if x == 0 { 0 } else { 55 + 40 * x };
            let (r, g, b) = (comp(n / 36), comp((n / 6) % 6), comp(n % 6));
            (r << 16) | (g << 8) | b
        }
        232..=255 => {
            let l = 8 + 10 * (i as u32 - 232);
            (l << 16) | (l << 8) | l
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: i32) {}
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &WlSurface, _: u32) {
        // The buffer we attached has been shown; allow the next paint, and do it
        // now only if content changed in the meantime.
        self.frame_pending = false;
        if self.dirty {
            self.draw(qh);
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &wayland_client::protocol::wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &wayland_client::protocol::wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_output::WlOutput) {}
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        // Snap to whole cells: derive the grid from the requested pixel size and
        // size our buffer to cols*cell x rows*cell. During a drag the compositor
        // sends a configure per pixel, but the grid only changes every cell_w /
        // cell_h pixels — skip the rest so we don't realloc + reflow + repaint on
        // every sub-cell step (that was the resize lag). Compositor places the
        // slightly-smaller buffer; the window settles on cell boundaries.
        let (wo, ho) = configure.new_size;
        let req_w = wo.map(|v| v.get()).unwrap_or(self.width);
        let req_h = ho.map(|v| v.get()).unwrap_or(self.height);
        let cols = (req_w as usize / self.font.cell_w).max(1) as u16;
        let rows = (req_h as usize / self.font.cell_h).max(1) as u16;
        let unchanged = self.configured && cols == self.cols && rows == self.rows;
        self.cols = cols;
        self.rows = rows;
        self.width = cols as u32 * self.font.cell_w as u32;
        self.height = rows as u32 * self.font.cell_h as u32;
        if unchanged {
            return; // grid identical to last frame: nothing to realloc or redraw
        }
        self.parser.set_size(rows, cols);
        self.pty.resize(rows, cols);
        self.configured = true;
        self.draw(qh);
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, event: KeyEvent) {
        self.on_key(event);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, modifiers: Modifiers, _: u32) {
        self.mods = modifiers;
    }
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlPointer, events: &[PointerEvent]) {
        let mut changed = false;
        for e in events {
            if let PointerEventKind::Axis { vertical, .. } = e.kind {
                // Wayland convention: positive vertical = scroll down (toward live).
                let (up, down) = if vertical.discrete != 0 {
                    let s = vertical.discrete.unsigned_abs() as usize * SCROLL_STEP;
                    if vertical.discrete < 0 {
                        (s, 0)
                    } else {
                        (0, s)
                    }
                } else if vertical.absolute < 0.0 {
                    (1, 0)
                } else if vertical.absolute > 0.0 {
                    (0, 1)
                } else {
                    (0, 0)
                };
                if up != 0 || down != 0 {
                    self.scroll(up, down);
                    changed = true;
                }
            }
        }
        if changed {
            let qh = self.qh_dummy();
            self.request_draw(&qh);
        }
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_keyboard!(State);
delegate_pointer!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);
