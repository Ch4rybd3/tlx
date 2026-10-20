//! TLX — Timeline eXplorer: a cross-platform forensic timeline / CSV viewer.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod data;
mod dialogs;
mod evtx;
mod filter;
mod format;
mod grid;
mod session;
mod timefmt;
mod view;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TLX — Timeline eXplorer")
            .with_app_id("tlx")
            .with_inner_size([1400.0, 850.0])
            .with_min_inner_size([640.0, 400.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("TLX", options, Box::new(|cc| Ok(Box::new(app::App::new(cc, files)))))
}
