// W wersji release bez okna konsoli na Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
#[cfg(target_os = "linux")]
mod desktop;
mod icon;
mod settings;

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use eframe::egui;

/// Wersja aplikacji z Cargo.toml (podnoszona przy każdej zmianie, patrz CHANGELOG.md).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    // Windows: domyślne urządzenie jest źródłem. Linux: źródłem jest wirtualne wyjście
    // SoundPort, więc grają wszystkie prawdziwe wyjścia.
    // `--selftest <tekst>` ogranicza wyjścia do tych, których nazwa lub id zawiera tekst.
    let filter = std::env::args().skip_while(|a| a != "--selftest").nth(1).unwrap_or_default();
    let outputs: Vec<_> = devices
        .iter()
        .filter(|d| cfg!(target_os = "linux") || !d.is_default)
        .filter(|d| d.name.contains(&filter) || d.id.contains(&filter))
        .collect();
    let config = audio::RoutingConfig {
        source: None,
        base_latency_ms: 80,
        outputs: outputs
            .iter()
            .map(|d| audio::OutputRoute { device_id: d.id.clone(), volume: 1.0, delay_ms: 0 })
            .collect(),
    };
    for _ in 0..10 {
        // Ponowne `apply` jak w aplikacji: na Linuksie wirtualne wyjście pojawia się po chwili.
        engine.apply(&config, &devices);
        for e in engine.take_errors() {
            println!("BŁĄD: {e}");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let stats: Vec<String> = outputs
            .iter()
            .map(|d| format!("{}: {:?} ms", d.name, engine.output_buffer_ms(&d.id)))
            .collect();
        println!(
            "źródło: {:?} | głośność źródła: {:?} | {}",
            engine.active_source(),
            engine.source_gain(),
            stats.join(" | ")
        );
    }
    engine.stop();
    Ok(())
}

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("SoundPort {VERSION}");
        return Ok(());
    }
    // Diagnostyka: `soundport --list-devices` (w wersji debug wypisuje na konsolę).
    if std::env::args().any(|a| a == "--list-devices") {
        let engine = audio::Engine::new(std::sync::Arc::new(|| {}));
        for d in engine.devices() {
            println!("{} {}\n    {}", if d.is_default { "*" } else { " " }, d.name, d.id);
        }
        return Ok(());
    }
    // Diagnostyka: przez 5 s kieruje dźwięk ze źródła na wszystkie pozostałe wyjścia
    // (opcjonalnie tylko pasujące do `--selftest <tekst>`).
    if std::env::args().any(|a| a == "--selftest") {
        return selftest();
    }

    let instance = single_instance::SingleInstance::new("soundport-tray-app")?;
    if !instance.is_single() {
        log("SoundPort już działa — kończę.");
        return Ok(());
    }
    log(&format!("SoundPort {VERSION} — start"));
    #[cfg(target_os = "linux")]
    desktop::install();

    let waker = app::Waker::default();
    let app = Rc::new(RefCell::new(app::SoundPortApp::new(waker.clone())?));

    // Windows: jedno okno przez cały czas działania, startuje schowane w zasobniku.
    #[cfg(not(target_os = "linux"))]
    run_window(&app, false)?;

    // Linux: na Wayland okna nie da się schować, więc zamykamy je naprawdę i działamy
    // dalej w zasobniku, a okno otwieramy na nowo po kliknięciu ikony.
    #[cfg(target_os = "linux")]
    {
        let mut show = app.borrow_mut().take_show_on_start();
        while !app.borrow().is_quitting() {
            if show {
                run_window(&app, true)?;
            }
            show = false;
            while !show && !app.borrow().is_quitting() {
                let timeout = app.borrow().idle_timeout();
                waker.wait(timeout);
                let mut a = app.borrow_mut();
                a.tick(None);
                show = a.take_window_request();
            }
        }
    }

    app.borrow_mut().shutdown();
    Ok(())
}

/// Otwiera okno ustawień i wraca, gdy zostanie zamknięte (eframe ponownie używa pętli zdarzeń).
fn run_window(app: &Rc<RefCell<app::SoundPortApp>>, visible: bool) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("SoundPort {VERSION}"))
            .with_app_id("soundport")
            .with_inner_size([760.0, 460.0])
            .with_min_inner_size([600.0, 320.0])
            .with_icon(app::window_icon())
            .with_visible(visible),
        ..Default::default()
    };
    let app = app.clone();
    eframe::run_native(
        "SoundPort",
        options,
        Box::new(move |cc| Ok(Box::new(app::Window::new(app, cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
