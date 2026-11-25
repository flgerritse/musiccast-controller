//! Controller for MusicCast-compatible network speakers. Entry point: wires the
//! background worker to the egui UI and runs the native window.

mod api;
mod discovery;
mod icon;
mod model;
mod stream;
mod ui;
mod worker;

use std::sync::{Arc, Mutex, OnceLock};

fn main() -> eframe::Result<()> {
    // --screenshot <file.png>: draw ~120 frames without taking focus, save, exit.
    // --demo: show made-up speakers instead of searching the network (for screenshots).
    let mut screenshot: Option<std::path::PathBuf> = None;
    let mut demo = false;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--screenshot" if i + 1 < args.len() => {
                screenshot = Some(std::path::PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--demo" => demo = true,
            // --make-icon <file.png>: write the 1024 px app icon and exit.
            "--make-icon" if i + 1 < args.len() => {
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
            _ => {}
        }
        i += 1;
    }
    let window_icon = eframe::egui::IconData { rgba: icon::rgba(256), width: 256, height: 256 };

    // The worker wakes the UI through this context once it exists.
    let ctx_cell = Arc::new(OnceLock::new());
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let job_tx = if demo {
        // Nothing listens: demo mode never touches the network.
        std::sync::mpsc::channel().0
    } else {
        worker::spawn(worker::EventTx::new(ev_tx, ctx_cell.clone()))
    };
    let shared = Arc::new(Mutex::new(ui::Shared::new(job_tx)));
    if demo {
        ui::seed_demo(&mut shared.lock().unwrap());
    }

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1040.0, 760.0])
        .with_min_inner_size([420.0, 460.0])
        .with_title("Controller for MusicCast")
        .with_drag_and_drop(true)
        .with_icon(window_icon);
    if screenshot.is_some() {
        viewport = viewport.with_active(false);
        #[cfg(target_os = "windows")]
        {
            viewport = viewport.with_window_level(eframe::egui::WindowLevel::AlwaysOnBottom);
        }
    }

    let mut native = eframe::NativeOptions { viewport, ..Default::default() };
    if screenshot.is_some() {
        native.event_loop_builder = Some(Box::new(|_builder| {
            #[cfg(target_os = "macos")]
            {
                use winit::platform::macos::EventLoopBuilderExtMacOS;
                _builder.with_activate_ignoring_other_apps(false);
            }
        }));
    }

    let app = ui::App::new(shared, ev_rx, screenshot, demo);
    eframe::run_native(
        "network-speaker-control",
        native,
        Box::new(move |cc| {
            ui::install_style(&cc.egui_ctx);
            let _ = ctx_cell.set(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
}
