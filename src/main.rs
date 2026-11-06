// W wersji release bez okna konsoli na Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod icon;
mod settings;

use std::io::Write;

use eframe::egui;

/// Prosty log do pliku obok ustawień (soundport.log).
pub fn log(message: &str) {
    eprintln!("{message}");
    let dir = settings::Settings::dir();
    let path = dir.join("soundport.log");
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let _ = writeln!(f, "[{ts}] {message}");
    }
}

fn selftest() -> anyhow::Result<()> {
    let mut engine = audio::Engine::new(std::sync::Arc::new(|| {}));
    let devices = engine.devices();
    let config = audio::RoutingConfig {
        source: None,
        base_latency_ms: 80,
        outputs: devices
            .iter()
            .filter(|d| !d.is_default)
            .map(|d| audio::OutputRoute { device_id: d.id.clone(), volume: 1.0, delay_ms: 0 })
            .collect(),
    };
    engine.apply(&config, &devices);
    for e in engine.take_errors() {
        println!("BŁĄD: {e}");
    }
    println!("źródło: {:?}", engine.active_source());
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let stats: Vec<String> = devices
            .iter()
            .filter(|d| !d.is_default)
            .map(|d| format!("{}: {:?} ms", d.name, engine.output_buffer_ms(&d.id)))
            .collect();
        println!("głośność źródła: {:?} | {}", engine.source_gain(), stats.join(" | "));
    }
    engine.stop();
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Diagnostyka: `soundport --list-devices` (w wersji debug wypisuje na konsolę).
    if std::env::args().any(|a| a == "--list-devices") {
        let engine = audio::Engine::new(std::sync::Arc::new(|| {}));
        for d in engine.devices() {
            println!("{} {}\n    {}", if d.is_default { "*" } else { " " }, d.name, d.id);
        }
        return Ok(());
    }
    // Diagnostyka: przez 5 s kieruje dźwięk z domyślnego urządzenia na wszystkie pozostałe.
    if std::env::args().any(|a| a == "--selftest") {
        return selftest();
    }

    let instance = single_instance::SingleInstance::new("soundport-tray-app")?;
    if !instance.is_single() {
        log("SoundPort już działa — kończę.");
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SoundPort")
            .with_inner_size([760.0, 460.0])
            .with_min_inner_size([600.0, 320.0])
            .with_icon(app::window_icon())
            // Startujemy schowani w zasobniku; okno otwiera kliknięcie ikony.
            .with_visible(false),
        ..Default::default()
    };

    eframe::run_native(
        "SoundPort",
        options,
        Box::new(|cc| Ok(Box::new(app::SoundPortApp::new(cc)?))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
