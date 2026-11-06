//! Aplikacja: ikona w zasobniku + menu kontekstowe + (domyślnie ukryte) okno ustawień.
//!
//! Wszystko działa w jednym wątku głównym pętli `eframe`. Zdarzenia z zasobnika, menu
//! i wątków audio budzą pętlę przez `request_repaint`, a `App::logic` obsługuje je także
//! wtedy, gdy okno jest ukryte.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use auto_launch::{AutoLaunch, AutoLaunchBuilder, WindowsEnableMode};
use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::audio::{DeviceInfo, Engine};
use crate::icon::{self, IconState};
use crate::settings::{MAX_BASE_LATENCY_MS, MAX_DELAY_MS, MIN_BASE_LATENCY_MS, Settings};

/// Jak często sprawdzamy listę urządzeń (podłączenie/rozłączenie słuchawek BT).
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Zapis ustawień po chwili bezczynności (przeciąganie suwaków nie pisze na dysk co klatkę).
const SAVE_DELAY: Duration = Duration::from_millis(800);

enum UiEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
}

#[derive(Clone)]
enum Action {
    ToggleRun,
    SetSource(Option<String>),
    ToggleOutput { id: String, name: String },
    ShowWindow,
    ToggleAutostart,
    Quit,
}

pub struct SoundPortApp {
    engine: Engine,
    settings: Settings,
    devices: Vec<DeviceInfo>,
    events: Receiver<UiEvent>,
    poll_requested: Arc<AtomicBool>,
    last_poll: Instant,
    save_due: Option<Instant>,
    last_error: Option<String>,
    autostart: Option<AutoLaunch>,
    autostart_enabled: bool,
    tray: TrayIcon,
    menu_actions: HashMap<MenuId, Action>,
    tray_signature: String,
    quitting: bool,
    show_on_start: bool,
}

impl SoundPortApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        let ctx = cc.egui_ctx.clone();

        // Zdarzenia zasobnika/menu przychodzą z innych wątków — kolejkujemy i budzimy pętlę.
        let (tx, events) = channel::<UiEvent>();
        install_tray_handlers(tx, ctx.clone());

        // Wątki audio (np. rozłączenie słuchawek) proszą o natychmiastowe odświeżenie.
        let poll_requested = Arc::new(AtomicBool::new(true));
        let notify = {
            let flag = poll_requested.clone();
            let ctx = ctx.clone();
            Arc::new(move || {
                flag.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            })
        };

        // Okresowe budzenie pętli, także gdy okno jest ukryte.
        {
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("soundport-poll".into())
                .spawn(move || loop {
                    std::thread::sleep(POLL_INTERVAL);
                    ctx.request_repaint();
                })?;
        }

        let tray = TrayIconBuilder::new()
            .with_tooltip("SoundPort")
            .with_icon(tray_icon(IconState::Idle))
            .with_menu(Box::new(Menu::new()))
            .with_menu_on_left_click(false)
            .build()?;

        let autostart = std::env::current_exe().ok().and_then(|exe| {
            AutoLaunchBuilder::new()
                .set_app_name("SoundPort")
                .set_app_path(&exe.to_string_lossy())
                .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
                .build()
                .ok()
        });
        let autostart_enabled = autostart
            .as_ref()
            .and_then(|a| a.is_enabled().ok())
            .unwrap_or(false);

        let engine = Engine::new(notify);
        let first_run = !Settings::exists();
        let settings = Settings::load();
        let devices = engine.devices();

        let mut app = Self {
            engine,
            settings,
            devices,
            events,
            poll_requested,
            last_poll: Instant::now(),
            save_due: None,
            last_error: None,
            autostart,
            autostart_enabled,
            tray,
            menu_actions: HashMap::new(),
            tray_signature: String::new(),
            quitting: false,
            show_on_start: first_run || std::env::args().any(|a| a == "--show"),
        };
        if first_run {
            app.settings.save();
        }
        app.remember_device_names();
        if app.settings.was_running {
            app.start();
        }
        app.refresh_tray();
        Ok(app)
    }

    // --- sterowanie routingiem -----------------------------------------------------------------

    fn start(&mut self) {
        self.engine.apply(&self.settings.routing(), &self.devices);
        self.settings.was_running = true;
        self.collect_errors();
        self.schedule_save();
    }

    fn stop(&mut self) {
        self.engine.stop();
        self.settings.was_running = false;
        self.schedule_save();
    }

    /// Po każdej zmianie ustawień: zapis + (jeśli działa) natychmiastowe zastosowanie.
    fn settings_changed(&mut self) {
        if self.engine.is_running() {
            self.engine.apply(&self.settings.routing(), &self.devices);
            self.collect_errors();
        }
        self.schedule_save();
    }

    fn poll_devices(&mut self) {
        self.devices = self.engine.devices();
        self.remember_device_names();
        if self.engine.is_running() {
            self.engine.apply(&self.settings.routing(), &self.devices);
            self.collect_errors();
        }
        self.last_poll = Instant::now();
    }

    fn remember_device_names(&mut self) {
        for d in &self.devices {
            if let Some(o) = self.settings.outputs.get_mut(&d.id) {
                if o.name != d.name {
                    o.name = d.name.clone();
                }
            }
        }
    }

    fn collect_errors(&mut self) {
        for e in self.engine.take_errors() {
            crate::log(&e);
            self.last_error = Some(e);
        }
    }

    fn schedule_save(&mut self) {
        self.save_due = Some(Instant::now() + SAVE_DELAY);
    }

    fn effective_source(&self) -> Option<String> {
        self.engine.active_source().map(str::to_string).or_else(|| {
            self.settings
                .source_device
                .clone()
                .filter(|id| self.devices.iter().any(|d| &d.id == id))
                .or_else(|| self.devices.iter().find(|d| d.is_default).map(|d| d.id.clone()))
        })
    }

    fn run_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::ToggleRun => {
                if self.engine.is_running() {
                    self.stop();
                } else {
                    self.start();
                }
            }
            Action::SetSource(id) => {
                self.settings.source_device = id;
                self.settings_changed();
            }
            Action::ToggleOutput { id, name } => {
                let out = self.settings.output_mut(&id, &name);
                out.enabled = !out.enabled;
                self.settings_changed();
            }
            Action::ShowWindow => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Action::ToggleAutostart => self.toggle_autostart(!self.autostart_enabled),
            Action::Quit => {
                self.engine.stop(); // was_running zostaje — wznowimy po następnym starcie
                self.settings.save();
                self.save_due = None;
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn toggle_autostart(&mut self, enable: bool) {
        let Some(auto) = &self.autostart else { return };
        let result = if enable { auto.enable() } else { auto.disable() };
        match result {
            Ok(()) => self.autostart_enabled = enable,
            Err(e) => {
                let msg = format!("Nie udało się zmienić autostartu: {e}");
                crate::log(&msg);
                self.last_error = Some(msg);
            }
        }
    }

    // --- zasobnik ---------------------------------------------------------------------------------

    /// Przebudowuje menu i ikonę tylko, gdy coś się faktycznie zmieniło.
    fn refresh_tray(&mut self) {
        let running = self.engine.is_running();
        let source = self.effective_source();
        let signature = format!(
            "{running}|{:?}|{source:?}|{:?}|{:?}|{}|{}",
            self.settings.source_device,
            self.devices,
            self.settings
                .outputs
                .iter()
                .map(|(id, o)| (id, o.enabled, self.engine.is_output_active(id)))
                .collect::<Vec<_>>(),
            self.autostart_enabled,
            self.autostart.is_some(),
        );
        if signature == self.tray_signature {
            return;
        }
        self.tray_signature = signature;

        let status = self.status_text();
        let _ = self.tray.set_tooltip(Some(format!("SoundPort — {status}")));
        let _ = self.tray.set_icon(Some(tray_icon(if running {
            IconState::Active
        } else {
            IconState::Idle
        })));

        let mut actions = HashMap::new();
        let menu = Menu::new();
        let add_action =|actions: &mut HashMap<MenuId, Action>, id: &MenuId, a: Action| {
            actions.insert(id.clone(), a);
        };

        let _ = menu.append(&MenuItem::new(&status, false, None));
        let toggle = MenuItem::new(if running { "Zatrzymaj" } else { "Uruchom" }, true, None);
        add_action(&mut actions, toggle.id(), Action::ToggleRun);
        let _ = menu.append(&toggle);
        let _ = menu.append(&PredefinedMenuItem::separator());

        // Źródło
        let source_menu = Submenu::new("Źródło dźwięku", true);
        let default_name = self
            .devices
            .iter()
            .find(|d| d.is_default)
            .map(|d| format!("Domyślne urządzenie ({})", d.name))
            .unwrap_or_else(|| "Domyślne urządzenie".into());
        let item = CheckMenuItem::new(
            escape_mnemonic(&default_name),
            true,
            self.settings.source_device.is_none(),
            None,
        );
        add_action(&mut actions, item.id(), Action::SetSource(None));
        let _ = source_menu.append(&item);
        let _ = source_menu.append(&PredefinedMenuItem::separator());
        for d in &self.devices {
            let item = CheckMenuItem::new(
                escape_mnemonic(&d.name),
                true,
                self.settings.source_device.as_deref() == Some(d.id.as_str()),
                None,
            );
            add_action(&mut actions, item.id(), Action::SetSource(Some(d.id.clone())));
            let _ = source_menu.append(&item);
        }
        let _ = menu.append(&source_menu);

        // Wyjścia
        let outputs_menu = Submenu::new("Wyjścia", true);
        for d in &self.devices {
            let is_source = source.as_deref() == Some(d.id.as_str());
            let enabled = self.settings.outputs.get(&d.id).is_some_and(|o| o.enabled);
            let label = if is_source {
                format!("{}  (źródło)", d.name)
            } else {
                d.name.clone()
            };
            let item = CheckMenuItem::new(escape_mnemonic(&label), !is_source, enabled && !is_source, None);
            add_action(
                &mut actions,
                item.id(),
                Action::ToggleOutput { id: d.id.clone(), name: d.name.clone() },
            );
            let _ = outputs_menu.append(&item);
        }
        // Zapamiętane, ale aktualnie rozłączone (np. wyłączone słuchawki BT).
        for (id, o) in &self.settings.outputs {
            if o.enabled && !self.devices.iter().any(|d| &d.id == id) {
                let label = format!("{}  (rozłączone)", if o.name.is_empty() { id } else { &o.name });
                let item = CheckMenuItem::new(escape_mnemonic(&label), true, true, None);
                add_action(
                    &mut actions,
                    item.id(),
                    Action::ToggleOutput { id: id.clone(), name: o.name.clone() },
                );
                let _ = outputs_menu.append(&item);
            }
        }
        if self.devices.is_empty() {
            let _ = outputs_menu.append(&MenuItem::new("Brak urządzeń", false, None));
        }
        let _ = menu.append(&outputs_menu);

        let settings_item = MenuItem::new("Głośność i opóźnienia…", true, None);
        add_action(&mut actions, settings_item.id(), Action::ShowWindow);
        let _ = menu.append(&settings_item);
        let _ = menu.append(&PredefinedMenuItem::separator());

        if self.autostart.is_some() {
            let item = CheckMenuItem::new("Uruchamiaj z systemem", true, self.autostart_enabled, None);
            add_action(&mut actions, item.id(), Action::ToggleAutostart);
            let _ = menu.append(&item);
        }
        let quit = MenuItem::new("Zakończ", true, None);
        add_action(&mut actions, quit.id(), Action::Quit);
        let _ = menu.append(&quit);

        self.tray.set_menu(Some(Box::new(menu)));
        self.menu_actions = actions;
    }

    fn status_text(&self) -> String {
        if !self.engine.is_running() {
            return "zatrzymany".into();
        }
        let n = self.engine.active_output_count();
        format!("aktywny, {n} {}", plural_outputs(n))
    }

    // --- okno ustawień ----------------------------------------------------------------------------

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let ctx = ui.ctx().clone();

        ui.horizontal(|ui| {
            ui.heading("SoundPort");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = self.engine.is_running();
                if ui.button(if running { "Zatrzymaj" } else { "Uruchom" }).clicked() {
                    self.run_action(&ctx, Action::ToggleRun);
                }
                let (color, text) = if running {
                    (egui::Color32::from_rgb(0x2e, 0xcc, 0x71), self.status_text())
                } else {
                    (ui.visuals().weak_text_color(), self.status_text())
                };
                ui.colored_label(color, text);
            });
        });
        ui.separator();

        // Źródło
        let source = self.effective_source();
        egui::Grid::new("general").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Źródło dźwięku:");
            let current = match &self.settings.source_device {
                None => "Domyślne urządzenie systemowe".to_string(),
                Some(id) => self
                    .devices
                    .iter()
                    .find(|d| &d.id == id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "(niedostępne — używam domyślnego)".into()),
            };
            egui::ComboBox::from_id_salt("source")
                .selected_text(current)
                .width(320.0)
                .show_ui(ui, |ui| {
                    let mut selected = self.settings.source_device.clone();
                    ui.selectable_value(&mut selected, None, "Domyślne urządzenie systemowe");
                    for d in &self.devices {
                        ui.selectable_value(&mut selected, Some(d.id.clone()), &d.name);
                    }
                    if selected != self.settings.source_device {
                        self.settings.source_device = selected;
                        changed = true;
                    }
                });
            ui.end_row();

            ui.label("Bufor bazowy:");
            changed |= ui
                .add(
                    egui::Slider::new(
                        &mut self.settings.base_latency_ms,
                        MIN_BASE_LATENCY_MS..=MAX_BASE_LATENCY_MS,
                    )
                    .suffix(" ms"),
                )
                .on_hover_text(
                    "Zapas danych na każdym wyjściu. Mniej = mniejsze opóźnienie, \
                     więcej = odporność na zakłócenia Bluetooth.",
                )
                .changed();
            ui.end_row();
        });

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Dźwięk ze źródła trafia na zaznaczone wyjścia. Źródło gra bez dodatkowego opóźnienia, \
                 więc dla najlepszej synchronizacji ustaw jako źródło wirtualny kabel audio \
                 (np. VB-CABLE) i zaznacz wszystkie słuchawki.",
            )
            .small()
            .weak(),
        );
        ui.separator();
        ui.strong("Wyjścia");

        // Lista: podłączone urządzenia + zapamiętane rozłączone.
        let mut rows: Vec<(String, String, bool)> =
            self.devices.iter().map(|d| (d.id.clone(), d.name.clone(), true)).collect();
        for (id, o) in &self.settings.outputs {
            if o.enabled && !self.devices.iter().any(|d| &d.id == id) {
                rows.push((id.clone(), o.name.clone(), false));
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("outputs")
                .num_columns(4)
                .striped(true)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.weak("Urządzenie");
                    ui.weak("Stan");
                    ui.weak("Głośność");
                    ui.weak("Opóźnienie");
                    ui.end_row();

                    for (id, name, connected) in rows {
                        let is_source = source.as_deref() == Some(id.as_str());
                        let out = self.settings.output_mut(&id, &name);
                        ui.add_enabled_ui(!is_source, |ui| {
                            changed |= ui.checkbox(&mut out.enabled, &name).changed();
                        });
                        let state = if is_source {
                            "źródło".to_string()
                        } else if !connected {
                            "rozłączone".to_string()
                        } else if let Some(ms) = self.engine.output_buffer_ms(&id) {
                            format!("gra · {ms} ms")
                        } else if out.enabled && self.engine.is_running() {
                            "łączenie…".to_string()
                        } else {
                            "—".to_string()
                        };
                        ui.weak(state);
                        ui.add_enabled_ui(out.enabled && !is_source, |ui| {
                            let mut percent = (out.volume * 100.0).round();
                            if ui
                                .add(
                                    egui::Slider::new(&mut percent, 0.0..=100.0)
                                        .fixed_decimals(0)
                                        .suffix(" %"),
                                )
                                .changed()
                            {
                                out.volume = percent / 100.0;
                                changed = true;
                            }
                        });
                        ui.add_enabled_ui(out.enabled && !is_source, |ui| {
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut out.delay_ms)
                                        .range(0..=MAX_DELAY_MS)
                                        .speed(1.0)
                                        .suffix(" ms"),
                                )
                                .on_hover_text(
                                    "Opóźnij to wyjście, jeśli gra wcześniej niż pozostałe \
                                     (przeciągnij lub kliknij dwukrotnie, by wpisać).",
                                )
                                .changed();
                        });
                        ui.end_row();
                    }
                });
        });

        ui.separator();
        if self.autostart.is_some() {
            let mut enabled = self.autostart_enabled;
            if ui.checkbox(&mut enabled, "Uruchamiaj z systemem").changed() {
                self.toggle_autostart(enabled);
            }
        }
        if let Some(err) = self.last_error.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, err);
                if ui.small_button("OK").clicked() {
                    self.last_error = None;
                }
            });
        }

        if changed {
            self.settings_changed();
        }
    }
}

impl eframe::App for SoundPortApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.show_on_start) {
            self.run_action(ctx, Action::ShowWindow);
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::Menu(e) => {
                    if let Some(action) = self.menu_actions.get(e.id()).cloned() {
                        self.run_action(ctx, action);
                    }
                }
                UiEvent::Tray(TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }) => self.run_action(ctx, Action::ShowWindow),
                UiEvent::Tray(_) => {}
            }
        }

        if self.poll_requested.swap(false, Ordering::Relaxed)
            || self.last_poll.elapsed() >= POLL_INTERVAL
        {
            self.poll_devices();
        }

        if self.save_due.is_some_and(|t| Instant::now() >= t) {
            self.settings.save();
            self.save_due = None;
        }
        if let Some(t) = self.save_due {
            ctx.request_repaint_after(t.saturating_duration_since(Instant::now()));
        }

        self.refresh_tray();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Zamknięcie okna chowa je do zasobnika; aplikację kończy "Zakończ" w menu.
        if ui.ctx().input(|i| i.viewport().close_requested()) && !self.quitting {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        egui::CentralPanel::default().show(ui, |ui| self.settings_ui(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.engine.stop();
        if self.save_due.is_some() {
            self.settings.save();
        }
    }
}

fn install_tray_handlers(tx: Sender<UiEvent>, ctx: egui::Context) {
    let tray_tx = tx.clone();
    let tray_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = tray_tx.send(UiEvent::Tray(e));
        tray_ctx.request_repaint();
    }));
    let tx = std::sync::Mutex::new(tx);
    MenuEvent::set_event_handler(Some(move |e| {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(UiEvent::Menu(e));
        }
        ctx.request_repaint();
    }));
}

fn tray_icon(state: IconState) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(icon::rgba(state), icon::SIZE, icon::SIZE).expect("valid icon")
}

pub fn window_icon() -> egui::IconData {
    egui::IconData {
        rgba: icon::rgba(IconState::Active),
        width: icon::SIZE,
        height: icon::SIZE,
    }
}

/// `&` w nazwie urządzenia byłby w menu Windows potraktowany jako skrót klawiszowy.
fn escape_mnemonic(s: &str) -> String {
    s.replace('&', "&&")
}

fn plural_outputs(n: usize) -> &'static str {
    match (n % 10, n % 100) {
        _ if n == 1 => "wyjście",
        (2..=4, r) if !(12..=14).contains(&r) => "wyjścia",
        _ => "wyjść",
    }
}

#[cfg(test)]
mod tests {
    use super::plural_outputs;

    #[test]
    fn polish_plurals() {
        assert_eq!(plural_outputs(0), "wyjść");
        assert_eq!(plural_outputs(1), "wyjście");
        assert_eq!(plural_outputs(3), "wyjścia");
        assert_eq!(plural_outputs(5), "wyjść");
        assert_eq!(plural_outputs(12), "wyjść");
        assert_eq!(plural_outputs(22), "wyjścia");
    }
}
