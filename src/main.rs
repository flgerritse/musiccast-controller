//! Controller for MusicCast-compatible network speakers. Entry point: wires the
//! background worker to the egui UI and runs the native window.

mod api;
mod discovery;
mod icon;
mod model;
mod stream;
mod ui;
mod worker;

use std::sync::{Arc, Mutex};

fn main() -> eframe::Result<()> {
    // --screenshot <file.png>: draw ~120 frames without taking focus, save, exit.
    let mut screenshot: Option<std::path::PathBuf> = None;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--screenshot" && i + 1 < args.len() {
            screenshot = Some(std::path::PathBuf::from(&args[i + 1]));
            i += 1;
        }
        // --make-icon <file.png>: write the 1024 px app icon and exit.
        if args[i] == "--make-icon" && i + 1 < args.len() {
            let path = std::path::PathBuf::from(&args[i + 1]);
            let rgba = icon::rgba(1024);
            let img = image::RgbaImage::from_raw(1024, 1024, rgba).expect("icon size");
            if let Err(e) = img.save(&path) {
                eprintln!("icon save failed: {e}");
                std::process::exit(1);
            }
            println!("wrote {}", path.display());
            return Ok(());
        }
        i += 1;
    }
    let window_icon = eframe::egui::IconData {
        rgba: icon::rgba(256),
        width: 256,
        height: 256,
    };

    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let job_tx = worker::spawn(ev_tx);
    let shared = Arc::new(Mutex::new(ui::Shared::new(job_tx)));

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([920.0, 600.0])
        .with_min_inner_size([480.0, 420.0])
        .with_title("Controller for MusicCast")
        .with_icon(window_icon);
    if screenshot.is_some() {
        viewport = viewport.with_active(false);
        #[cfg(target_os = "windows")]
        {
            viewport = viewport.with_window_level(eframe::egui::WindowLevel::AlwaysOnBottom);
        }
    }

    let mut native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    if screenshot.is_some() {
        native.event_loop_builder = Some(Box::new(|_builder| {
            #[cfg(target_os = "macos")]
            {
                use winit::platform::macos::EventLoopBuilderExtMacOS;
                _builder.with_activate_ignoring_other_apps(false);
            }
        }));
    }

    let app = ui::App::new(shared, ev_rx, screenshot);
    eframe::run_native(
        "network-speaker-control",
        native,
        Box::new(|cc| {
            ui::install_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}