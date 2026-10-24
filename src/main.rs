//! eframe entry point for the desktop app (M2 shell, M3 rendering, M4 inspector).
//!
//! wgpu is explicit: eframe's default renderer is glow, and this project needs
//! wgpu (Metal on Apple Silicon, Vulkan/GL on Linux).  Use `WGPU_BACKEND=gl`
//! over SSH/llvmpipe.  Verify the adapter once with `RUST_LOG=wgpu=info`.

mod app;
mod render;
mod text;
mod ui;

use eframe::egui;

fn main() -> eframe::Result {
    let _ = env_logger::try_init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("sagenote")
            .with_inner_size([1120.0, 820.0])
            .with_min_inner_size([760.0, 520.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "sagenote",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)?))),
    )
}
