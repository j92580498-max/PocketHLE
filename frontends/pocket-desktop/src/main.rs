//! Cross-platform desktop launcher GUI for PocketHLE.
//!
//! Targets Linux and Windows. The interface is deliberately modeled
//! after [`j2me-loader`](https://github.com/nikita36078/j2me-loader): a
//! library screen with cards for every imported game, an "Import" button
//! that pulls a `.CAB` file in via a native file dialog, a "Settings"
//! screen, and a per-game settings sheet. Selecting a card and pressing
//! "Run" opens a separate emulator viewport that displays the
//! framebuffer produced by [`pocket_core::Emulator`].

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod runner;

use std::path::PathBuf;

use anyhow::{Context, Result};

use pocket_library::{default_library_root, Library};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let library_root = default_library_root();
    log::info!("Using library root: {}", library_root.display());
    let library = Library::open(&library_root).context("opening PocketHLE library")?;

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("PocketHLE"),
        ..Default::default()
    };
    let mut library_slot = Some(library);
    eframe::run_native(
        "PocketHLE",
        native_options,
        Box::new(move |cc| {
            let lib = library_slot.take().expect("PocketLauncher built twice");
            Box::new(app::PocketLauncher::new(cc, lib)) as Box<dyn eframe::App>
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}
