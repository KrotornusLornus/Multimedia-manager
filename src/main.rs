mod app;
mod config;
mod db;
mod domain;
mod player;
mod scan;
mod thumbs;
mod ytdlp;

use eframe::egui;

fn main() -> eframe::Result {
    let db_path = config::data_dir().join("library.db");
    let db = db::Db::open(&db_path).unwrap_or_else(|error| {
        eprintln!("No se pudo abrir la base de datos: {error}");
        std::process::exit(1);
    });

    let config = config::Config::from_meta(&db.all_meta().unwrap_or_default());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("video-player")
            .with_inner_size([1080.0, 700.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Multimedia-manager",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, config, db_path, db)))),
    )
}
