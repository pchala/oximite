//! Oximite Desktop Control, Graphing & PID Tuning Application.

use std::env;
use std::sync::{Arc, Mutex};

use oximite_control::app::{apply_theme, OximiteControlApp};
use oximite_control::client::start_client_service;
use oximite_control::store::ControlStore;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = env::args().collect();
    let endpoint = match (args.get(1), args.get(2)) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) if host.contains(':') => host.clone(),
        (Some(host), None) => format!("{host}:8080"),
        (None, _) => "192.168.1.117:8080".to_string(),
    };

    let store = Arc::new(Mutex::new(ControlStore::new(&endpoint)));
    let (client, _thread) = start_client_service(store.clone());
    if args.len() > 1 {
        client.connect(&endpoint);
    }

    let app = OximiteControlApp::new(store, client);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_title("Oximite Control"),
        ..Default::default()
    };

    eframe::run_native(
        "Oximite Control",
        options,
        Box::new(|cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
