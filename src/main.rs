// ZenOS — Wayland compositor on DRM/KMS (smithay backend).
//
// Old winit/wgpu shell lives in src/app.rs, src/renderer/, src/ui/. It is kept
// for the geometry port (milestone 5) but is NOT declared here, so it is not
// compiled on this branch.
mod backend;
mod config;
mod drm;
mod handlers;
mod render;
mod shaders;
mod state;
mod text;

fn main() {
    tracing_subscriber::fmt().init();

    if let Err(e) = backend::run() {
        tracing::error!("ZenOS exited with error: {e}");
        std::process::exit(1);
    }
}
