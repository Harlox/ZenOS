use winit::event_loop::EventLoop;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId, CursorIcon};
use std::sync::Arc;

use crate::renderer::Renderer;

pub struct App {
    renderer: Option<Renderer>,
}

impl Default for App {
    fn default() -> Self {
        Self { renderer: None }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attr = Window::default_attributes()
            .with_title("ZenOS")
            .with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)))
            .with_cursor(winit::window::Cursor::Icon(CursorIcon::Default));

        let window = Arc::new(event_loop.create_window(attr).unwrap());

        // Cacher le curseur
        window.set_cursor_visible(false);

        self.renderer = Some(Renderer::new(window));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.physical_key == winit::keyboard::PhysicalKey::Code(
                    winit::keyboard::KeyCode::Escape
                ) {
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.render();
                }
                if let Some(renderer) = &self.renderer {
                    renderer.window().request_redraw();
                }
            }
            _ => {}
        }
    }
}

pub fn run() {
    let event_loop = EventLoop::new().unwrap();
    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}