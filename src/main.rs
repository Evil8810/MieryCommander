// Release builds on Windows: no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod app;
mod archive;
mod clip;
mod compare;
mod config;
mod dialogs;
mod fonts;
mod fsutil;
mod ftp;
mod remote;
mod sftp;
mod smb;
mod sync;
mod update;
mod ops;
mod openwith;
mod panel;
mod rename;
mod search;
mod viewer;
#[cfg(windows)]
mod winsys;

#[cfg(test)]
mod tests;

fn main() -> eframe::Result {
    update::cleanup_old();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MieryCommander")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_app_id("miery-commander")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                    .unwrap_or_default(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "MieryCommander",
        options,
        Box::new(|cc| Ok(Box::new(app::MieryApp::new(cc)))),
    )
}
