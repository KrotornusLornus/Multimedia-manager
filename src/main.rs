mod app;
mod config;
mod db;
mod domain;
mod player;
mod scan;
mod thumbs;
mod ytdlp;

use eframe::egui;
use eframe::wgpu;

fn preferred_wgpu_backends() -> wgpu::Backends {
    if let Some(backends) = wgpu::Backends::from_env() {
        return backends;
    }

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(descriptor);
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    if adapters.is_empty() {
        wgpu::Backends::PRIMARY | wgpu::Backends::GL
    } else {
        wgpu::Backends::VULKAN
    }
}

fn main() -> eframe::Result {
    let db_path = config::data_dir().join("library.db");
    let db = db::Db::open(&db_path).unwrap_or_else(|error| {
        eprintln!("Could not open the database: {error}");
        std::process::exit(1);
    });

    let config = config::Config::from_meta(&db.all_meta().unwrap_or_default());

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/gallery.png"))
        .expect("could not load the app icon");

    let mut wgpu_options = eframe::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = preferred_wgpu_backends();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("gallery")
            .with_icon(icon)
            .with_inner_size([1080.0, 700.0])
            .with_min_inner_size([640.0, 420.0]),
        wgpu_options,
        ..Default::default()
    };

    eframe::run_native(
        "Gallery",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, config, db_path, db)))),
    )
}
