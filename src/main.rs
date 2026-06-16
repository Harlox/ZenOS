use std::sync::Arc;
use std::num::NonZeroU32;
use winit::application::ApplicationHandler;
use winit::event::{WindowEvent, MouseButton, ElementState};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

// Couleurs style macOS dark
const COLOR_BAR: u32      = 0x00323232;
const COLOR_DOCK: u32     = 0x00454545;
const COLOR_SETTINGS: u32 = 0x00282828;
const COLOR_ICON: u32     = 0x00606060;
const COLOR_TEXT: u32     = 0x00FFFFFF;
const COLOR_SECTION: u32  = 0x00383838;

struct Surface {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
}

struct App {
    bar: Option<Surface>,
    dock: Option<Surface>,
    settings: Option<Surface>,
    settings_open: bool,
    font: fontdue::Font,
}

impl App {
    fn new() -> Self {
        let font_data = include_bytes!("/usr/share/fonts/TTF/DejaVuSans.ttf");
        let font = fontdue::Font::from_bytes(font_data as &[u8], fontdue::FontSettings::default()).unwrap();
        Self {
            bar: None,
            dock: None,
            settings: None,
            settings_open: false,
            font,
        }
    }
}

fn draw_rounded_rect(buffer: &mut [u32], width: u32, height: u32, color: u32, radius: u32) {
    for y in 0..height {
        for x in 0..width {
            let in_corner =
                (x < radius && y < radius && dist(x, y, radius, radius) > radius) ||
                (x >= width - radius && y < radius && dist(x, y, width - radius - 1, radius) > radius) ||
                (x < radius && y >= height - radius && dist(x, y, radius, height - radius - 1) > radius) ||
                (x >= width - radius && y >= height - radius && dist(x, y, width - radius - 1, height - radius - 1) > radius);
            buffer[(y * width + x) as usize] = if in_corner { 0x00000000 } else { color };
        }
    }
}

fn draw_circle(buffer: &mut [u32], width: u32, cx: u32, cy: u32, radius: u32, color: u32) {
    let r = radius as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                let px = cx as i32 + dx;
                let py = cy as i32 + dy;
                if px >= 0 && py >= 0 {
                    buffer[(py as u32 * width + px as u32) as usize] = color;
                }
            }
        }
    }
}

fn draw_text(buffer: &mut [u32], buf_width: u32, buf_height: u32, font: &fontdue::Font, text: &str, x: u32, y: u32, size: f32, color: u32) {
    let mut cursor_x = x as i32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        for (i, alpha) in bitmap.iter().enumerate() {
            let bx = cursor_x + (i % metrics.width) as i32;
            let by = y as i32 + (i / metrics.width) as i32;
            if bx >= 0 && by >= 0 && bx < buf_width as i32 && by < buf_height as i32 && *alpha > 0 {
                let a = *alpha as u32;
                let r = ((color >> 16) & 0xFF) * a / 255;
                let g = ((color >> 8) & 0xFF) * a / 255;
                let b = (color & 0xFF) * a / 255;
                buffer[(by as u32 * buf_width + bx as u32) as usize] = (r << 16) | (g << 8) | b;
            }
        }
        cursor_x += metrics.advance_width as i32;
    }
}

fn dist(x: u32, y: u32, cx: u32, cy: u32) -> u32 {
    let dx = (x as i32 - cx as i32).pow(2);
    let dy = (y as i32 - cy as i32).pow(2);
    ((dx + dy) as f64).sqrt() as u32
}

fn make_surface(event_loop: &ActiveEventLoop, title: &str, x: i32, y: i32, w: u32, h: u32) -> Surface {
    let attr = Window::default_attributes()
        .with_title(title)
        .with_inner_size(winit::dpi::LogicalSize::new(w as f64, h as f64))
        .with_position(winit::dpi::LogicalPosition::new(x as f64, y as f64))
        .with_decorations(false);
    let window = Arc::new(event_loop.create_window(attr).unwrap());
    let context = softbuffer::Context::new(window.clone()).unwrap();
    let surface = softbuffer::Surface::new(&context, window.clone()).unwrap();
    Surface { window, surface }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.bar = Some(make_surface(event_loop, "bar", 0, 0, 1280, 28));
        self.dock = Some(make_surface(event_loop, "dock", 340, 730, 600, 70));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(settings) = &self.settings {
                    if settings.window.id() == id {
                        self.settings = None;
                        self.settings_open = false;
                        return;
                    }
                }
                event_loop.exit();
            }

            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if let Some(bar) = &self.bar {
                    if bar.window.id() == id && !self.settings_open {
                        self.settings_open = true;
                        let s = make_surface(event_loop, "settings", 400, 40, 320, 500);
                        s.window.request_redraw();
                        self.settings = Some(s);
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                // Bar
                if let Some(bar) = &mut self.bar {
                    if bar.window.id() == id {
                        let size = bar.window.inner_size();
                        let w = size.width;
                        let h = size.height;
                        bar.surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).unwrap();
                        let mut buffer = bar.surface.buffer_mut().unwrap();
                        for pixel in buffer.iter_mut() { *pixel = COLOR_BAR; }
                        // Texte "Mon DE" à gauche
                        draw_text(&mut buffer, w, h, &self.font, "Mon DE", 12, 7, 14.0, COLOR_TEXT);
                        // Heure au centre
                        draw_text(&mut buffer, w, h, &self.font, "12:00", 600, 7, 14.0, COLOR_TEXT);
                        // Settings icon à droite
                        draw_text(&mut buffer, w, h, &self.font, "Settings", 1160, 7, 14.0, COLOR_TEXT);
                        buffer.present().unwrap();
                    }
                }

                // Dock
                if let Some(dock) = &mut self.dock {
                    if dock.window.id() == id {
                        let size = dock.window.inner_size();
                        let w = size.width;
                        let h = size.height;
                        dock.surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).unwrap();
                        let mut buffer = dock.surface.buffer_mut().unwrap();
                        draw_rounded_rect(&mut buffer, w, h, COLOR_DOCK, 18);
                        // Icônes — cercles colorés
                        let icons = [
                            0x00FF5F57, // rouge
                            0x00FFBD2E, // jaune
                            0x0028CA41, // vert
                            0x005AC8FA, // bleu
                            0x00BF5AF2, // violet
                            0x00FF9F0A, // orange
                        ];
                        for (i, color) in icons.iter().enumerate() {
                            let cx = 60 + i as u32 * 90;
                            draw_circle(&mut buffer, w, cx, 35, 22, *color);
                        }
                        buffer.present().unwrap();
                    }
                }

                // Settings
                if let Some(settings) = &mut self.settings {
                    if settings.window.id() == id {
                        let size = settings.window.inner_size();
                        let w = size.width;
                        let h = size.height;
                        settings.surface.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).unwrap();
                        let mut buffer = settings.surface.buffer_mut().unwrap();
                        draw_rounded_rect(&mut buffer, w, h, COLOR_SETTINGS, 16);

                        // Titre
                        draw_text(&mut buffer, w, h, &self.font, "Settings", 110, 20, 18.0, COLOR_TEXT);

                        // Sections
                        let sections = ["Wi-Fi", "Bluetooth", "Volume", "Brightness", "Appearance"];
                        for (i, name) in sections.iter().enumerate() {
                            let sy = 70 + i as u32 * 70;
                            // Fond section
                            for py in sy..sy+55 {
                                for px in 16..w-16 {
                                    if buffer[(py * w + px) as usize] != 0 {
                                        buffer[(py * w + px) as usize] = COLOR_SECTION;
                                    }
                                }
                            }
                            draw_text(&mut buffer, w, h, &self.font, name, 30, sy + 18, 14.0, COLOR_TEXT);
                        }
                        buffer.present().unwrap();
                    }
                }

                if let Some(bar) = &self.bar { bar.window.request_redraw(); }
                if let Some(dock) = &self.dock { dock.window.request_redraw(); }
                if let Some(s) = &self.settings { s.window.request_redraw(); }
            }
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    let mut app = App::new();
    event_loop.run_app(&mut app).unwrap();
}