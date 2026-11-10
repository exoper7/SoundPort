//! Linux/PipeWire: wirtualne wyjście „SoundPort” i obserwacja grafu PipeWire.
//!
//! Osobny wątek z pętlą PipeWire:
//! - tworzy (na żądanie) wirtualny sink `soundport` — odpowiednik VB-CABLE wbudowany w PipeWire.
//!   Żyje tak długo jak nasze połączenie, więc po zamknięciu (lub awarii) programu znika sam.
//!   `monitor.channel-volumes=true` sprawia, że monitor uwzględnia głośność i wyciszenie
//!   ustawione w systemie, więc suwak głośności Plasmy działa na wszystkich słuchawkach;
//! - ustawia go jako domyślne wyjście systemu i przywraca poprzednie po zatrzymaniu;
//! - utrzymuje aktualną listę prawdziwych wyjść (cpal odczytuje ją tylko raz przy starcie);
//! - pilnuje, żeby żaden nasz strumień wyjściowy nie został podłączony do urządzenia
//!   źródłowego. WirePlumber przenosi strumień rozłączonych słuchawek na domyślne wyjście,
//!   którym jest właśnie źródło — bez tej blokady powstałaby pętla sprzężenia.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pipewire as pw;
use pw::types::ObjectType;

use super::Notify;

/// `node.name` wirtualnego wyjścia (identyfikator cpal: `pipewire:soundport`).
pub const VIRTUAL_NODE: &str = "soundport";
const VIRTUAL_DESCRIPTION: &str = "SoundPort";
const CONFIGURED_SINK_KEY: &str = "default.configured.audio.sink";
const DEFAULT_SINK_KEY: &str = "default.audio.sink";
/// Najdłużej tyle czekamy, aż odtwarzacze przeniosą się z wyłączanego wirtualnego wyjścia.
const DESTROY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);
/// Tyle czekamy po przeniesieniu odtwarzaczy, zanim usuniemy wirtualne wyjście. WirePlumber
/// (`linking/mpris-pause.lua`) pauzuje odtwarzacze podłączone do usuwanego wyjścia, a swoją
/// listę połączeń aktualizuje z opóźnieniem — bez tej chwili zapauzowałby już przeniesiony
/// odtwarzacz.
const SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkInfo {
    pub name: String,
    pub description: String,
}

#[derive(Default, PartialEq)]
struct Shared {
    sinks: Vec<SinkInfo>,
    default_sink: Option<String>,
    virtual_ready: bool,
    /// Wyjścia, do których jest podłączony któryś z naszych strumieni odtwarzania.
    linked_outputs: HashSet<String>,
    /// Wyjścia, z których monitora nagrywa któryś z naszych strumieni.
    captured: HashSet<String>,
    /// Wyjścia, do których gra bezpośrednio strumień innego programu.
    foreign_outputs: HashSet<String>,
    /// `node.name` wyjścia → id węzła, na którym system trzyma jego głośność. W WirePlumber 1.6
    /// słuchawki BT to filtr `bluez_output.XX` przed wewnętrznym węzłem `bluez_output.XX.1`.
    volume_ids: HashMap<String, u32>,
}

enum Command {
    /// Czy wirtualne wyjście ma istnieć (i być domyślnym wyjściem systemu).
    Virtual(bool),
    /// `node.name` urządzenia źródłowego, do którego nie wolno podłączać naszych wyjść.
    Guard(Option<String>),
    /// Usuń wyłączone wirtualne wyjście (numer wyłączenia — nieaktualne polecenia pomijamy).
    DestroyVirtual(u32),
    Quit,
}

pub struct PwGraph {
    shared: Arc<Mutex<Shared>>,
    commands: Option<pw::channel::Sender<Command>>,
    thread: Option<JoinHandle<()>>,
    /// Ostatnio wysłane polecenia — nie wysyłamy tego samego przy każdym `apply`.
    sent_virtual: bool,
    sent_guard: Option<String>,
}

impl PwGraph {
    pub fn start(notify: Notify) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = pw::channel::channel::<Command>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let thread = {
            let shared = shared.clone();
            let tx_self = tx.clone();
            std::thread::Builder::new()
                .name("soundport-pipewire".into())
                .spawn(move || {
                    if let Err(e) = run(rx, tx_self, shared, notify, ready_tx) {
                        crate::log(&format!("PipeWire: {e:#}"));
                    }
                })
                .ok()
        };
        // Czekamy na pierwszy obraz grafu, żeby lista urządzeń nie była pusta tuż po starcie.
        let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(2));
        Self {
            shared,
            commands: thread.is_some().then_some(tx),
            thread,
            sent_virtual: false,
            sent_guard: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Prawdziwe wyjścia audio (bez wirtualnego SoundPort i wewnętrznych węzłów bluez).
    pub fn sinks(&self) -> Vec<SinkInfo> {
        self.lock().sinks.clone()
    }

    /// `node.name` aktualnie domyślnego wyjścia systemu.
    pub fn default_sink(&self) -> Option<String> {
        self.lock().default_sink.clone()
    }

    /// Czy wirtualne wyjście już istnieje w grafie (można z niego nagrywać).
    pub fn virtual_ready(&self) -> bool {
        self.lock().virtual_ready
    }

    /// Czy nasz strumień odtwarzania jest faktycznie podłączony do wyjścia `name`.
    pub fn is_output_linked(&self, name: &str) -> bool {
        self.lock().linked_outputs.contains(name)
    }

    /// Czy nasz strumień przechwytywania jest faktycznie podłączony do monitora `name`.
    pub fn is_captured(&self, name: &str) -> bool {
        self.lock().captured.contains(name)
    }

    /// Czy do wyjścia `name` gra bezpośrednio inny program (np. odtwarzacz jeszcze nie
    /// przeniesiony przez WirePlumber na wirtualne wyjście).
    pub fn has_foreign_streams(&self, name: &str) -> bool {
        self.lock().foreign_outputs.contains(name)
    }

    /// Id węzła z głośnością wyjścia `name` (do `wpctl`).
    pub fn volume_id(&self, name: &str) -> Option<u32> {
        self.lock().volume_ids.get(name).copied()
    }

    pub fn set_virtual(&mut self, enabled: bool) {
        if self.sent_virtual != enabled {
            self.sent_virtual = enabled;
            self.send(Command::Virtual(enabled));
        }
    }

    pub fn set_guard(&mut self, source: Option<String>) {
        if self.sent_guard != source {
            self.sent_guard = source.clone();
            self.send(Command::Guard(source));
        }
    }

    fn send(&self, command: Command) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(command);
        }
    }
}

impl Drop for PwGraph {
    fn drop(&mut self) {
        // Pętla przed wyjściem usuwa wirtualne wyjście i przywraca domyślne urządzenie.
        self.send(Command::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// --- wątek PipeWire ---------------------------------------------------------------------------

struct NodeEntry {
    name: String,
    description: String,
    class: String,
    client: Option<u32>,
}

#[derive(Default)]
struct Local {
    nodes: HashMap<u32, NodeEntry>,
    /// Powiązane węzły audio: WirePlumber potrafi zmienić węzłowi klasę już po rejestracji
    /// (np. węzeł bluez staje się `Audio/Sink/Internal`), więc śledzimy aktualizacje.
    node_proxies: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// id połączenia → (węzeł wyjściowy, węzeł wejściowy).
    links: HashMap<u32, (u32, u32)>,
    /// id klienta → PID procesu (rozpoznajemy w ten sposób strumienie cpal tego programu).
    client_pids: HashMap<u32, u32>,
    metadata: Option<(u32, pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    configured_sink: Option<String>,
    default_sink: Option<String>,
    /// Domyślne wyjście sprzed włączenia SoundPort (surowa wartość JSON, None = brak).
    saved_default: Option<Option<String>>,
    want_virtual: bool,
    virtual_node: Option<pw::node::Node>,
    /// Wirtualne wyjście jest wyłączone, ale czeka, aż odtwarzacze się z niego przeniosą.
    /// Usunięcie urządzenia, na którym coś gra, przeglądarki (Chromium) traktują jak wyjęcie
    /// słuchawek i wstrzymują odtwarzanie — samo przeniesienie strumienia już nie.
    pending_destroy: bool,
    /// Strumienie, które grały na wirtualnym wyjściu w chwili wyłączenia. Wyjście usuwamy
    /// dopiero, gdy każdy z nich jest podłączony gdzie indziej albo zniknął — samo „brak
    /// połączeń” nie wystarcza, bo WirePlumber przy przenoszeniu najpierw rozłącza strumień,
    /// a dopiero potem podłącza go do nowego wyjścia.
    moving_streams: HashSet<u32>,
    /// Numer bieżącego wyłączenia wirtualnego wyjścia i czy zaplanowano już jego usunięcie.
    destroy_gen: u32,
    settle_scheduled: bool,
    quitting: bool,
    guard: Option<String>,
}

/// Wysyła polecenie do pętli PipeWire po upływie `delay` (pętla nie ma tu własnych timerów).
fn send_later(tx: &pw::channel::Sender<Command>, delay: std::time::Duration, command: Command) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let _ = tx.send(command);
    });
}

fn run(
    rx: pw::channel::Receiver<Command>,
    tx_self: pw::channel::Sender<Command>,
    shared: Arc<Mutex<Shared>>,
    notify: Notify,
    ready: std::sync::mpsc::Sender<()>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let local = Rc::new(RefCell::new(Local::default()));
    let our_pid = std::process::id();

    let publish = {
        let local = local.clone();
        let shared = shared.clone();
        let notify = notify.clone();
        Rc::new(move || {
            let l = local.borrow();
            let mut sinks: Vec<SinkInfo> = l
                .nodes
                .values()
                .filter(|n| n.class == "Audio/Sink" && n.name != VIRTUAL_NODE)
                .map(|n| SinkInfo {
                    name: n.name.clone(),
                    description: if n.description.is_empty() {
                        n.name.clone()
                    } else {
                        n.description.clone()
                    },
                })
                .collect();
            sinks.sort_by(|a, b| a.name.cmp(&b.name));
            let ours = |id: &u32, class: &str| {
                l.nodes.get(id).is_some_and(|n| {
                    n.class == class && n.client.and_then(|c| l.client_pids.get(&c)) == Some(&our_pid)
                })
            };
            let name_of = |id: &u32| l.nodes.get(id).map(|n| n.name.clone());
            let mut linked_outputs = HashSet::new();
            let mut captured = HashSet::new();
            let mut foreign_outputs = HashSet::new();
            for (out_node, in_node) in l.links.values() {
                if ours(out_node, "Stream/Output/Audio") {
                    linked_outputs.extend(name_of(in_node));
                } else if l.nodes.get(out_node).is_some_and(|n| n.class == "Stream/Output/Audio") {
                    foreign_outputs.extend(name_of(in_node));
                }
                if ours(in_node, "Stream/Input/Audio") {
                    captured.extend(name_of(out_node));
                }
            }
            // Wewnętrzny węzeł słuchawek BT: ta sama nazwa z `_` zamiast `:` i sufiksem `.N`.
            let internal: HashMap<String, u32> = l
                .nodes
                .iter()
                .filter(|(_, n)| n.class == "Audio/Sink/Internal")
                .filter_map(|(id, n)| Some((n.name.rsplit_once('.')?.0.to_string(), *id)))
                .collect();
            let volume_ids = l
                .nodes
                .iter()
                .filter(|(_, n)| n.class == "Audio/Sink")
                .map(|(id, n)| {
                    let id = internal.get(&n.name.replace(':', "_")).copied().unwrap_or(*id);
                    (n.name.clone(), id)
                })
                .collect();
            let next = Shared {
                sinks,
                default_sink: l.default_sink.clone(),
                virtual_ready: l.nodes.values().any(|n| n.name == VIRTUAL_NODE),
                linked_outputs,
                captured,
                foreign_outputs,
                volume_ids,
            };
            drop(l);
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            if *s != next {
                *s = next;
                drop(s);
                notify();
            }
        })
    };

    // Włącza/wyłącza wirtualne wyjście zgodnie z `want_virtual` i ustawia domyślne urządzenie.
    let sync_virtual = {
        let local = local.clone();
        let core = core.clone();
        let tx_self = tx_self.clone();
        Rc::new(move || {
            let mut guard = local.borrow_mut();
            let l = &mut *guard; // rozdzielne pożyczanie pól
            if l.want_virtual && l.virtual_node.is_none() {
                let props = pw::properties::properties! {
                    "factory.name" => "support.null-audio-sink",
                    "node.name" => VIRTUAL_NODE,
                    "node.description" => VIRTUAL_DESCRIPTION,
                    "media.class" => "Audio/Sink",
                    "audio.position" => "[ FL FR ]",
                    "monitor.channel-volumes" => "true",
                    "device.icon-name" => "audio-headphones",
                    "priority.session" => "100",
                };
                match core.create_object::<pw::node::Node>("adapter", &props) {
                    Ok(node) => l.virtual_node = Some(node),
                    Err(e) => crate::log(&format!("PipeWire: nie można utworzyć wyjścia SoundPort: {e}")),
                }
            } else if l.want_virtual {
                l.pending_destroy = false;
                l.moving_streams.clear();
            } else if l.virtual_node.is_some() && !l.pending_destroy {
                // Usunie je `destroy_virtual_if_idle`, gdy odtwarzacze się przeniosą, a gdyby
                // któryś został — i tak po `DESTROY_DELAY`.
                l.pending_destroy = true;
                l.settle_scheduled = false;
                l.destroy_gen += 1;
                send_later(&tx_self, DESTROY_DELAY, Command::DestroyVirtual(l.destroy_gen));
                let virtual_ids: HashSet<u32> = l
                    .nodes
                    .iter()
                    .filter(|(_, n)| n.name == VIRTUAL_NODE && n.class == "Audio/Sink")
                    .map(|(id, _)| *id)
                    .collect();
                l.moving_streams = l
                    .links
                    .values()
                    .filter(|(_, input)| virtual_ids.contains(input))
                    .map(|(output, _)| *output)
                    .collect();
            }

            let Some((_, metadata, _)) = &l.metadata else { return };
            if l.want_virtual {
                if l.saved_default.is_none() {
                    l.saved_default = Some(l.configured_sink.clone());
                }
                let value = format!("{{\"name\":\"{VIRTUAL_NODE}\"}}");
                if l.configured_sink.as_deref() != Some(value.as_str()) {
                    metadata.set_property(0, CONFIGURED_SINK_KEY, Some("Spa:String:JSON"), Some(&value));
                }
            } else if let Some(previous) = l.saved_default.take() {
                // Przywracamy tylko, jeśli w międzyczasie nikt nie wybrał innego wyjścia.
                if l.configured_sink.as_deref().is_some_and(|v| v.contains(&format!("\"{VIRTUAL_NODE}\""))) {
                    metadata.set_property(0, CONFIGURED_SINK_KEY, Some("Spa:String:JSON"), previous.as_deref());
                }
            }
        })
    };

    // Usuwa wyłączone wirtualne wyjście, gdy nic już na nie nie gra (albo `force`).
    let destroy_virtual_if_idle = {
        let local = local.clone();
        let tx_self = tx_self.clone();
        Rc::new(move |force: bool| {
            let mut l = local.borrow_mut();
            if !l.pending_destroy || l.want_virtual {
                return;
            }
            let virtual_ids: Vec<u32> = l
                .nodes
                .iter()
                .filter(|(_, n)| n.name == VIRTUAL_NODE && n.class == "Audio/Sink")
                .map(|(id, _)| *id)
                .collect();
            let busy = l.links.values().any(|(_, input)| virtual_ids.contains(input));
            // Każdy przenoszony strumień zniknął albo jest już podłączony do innego wyjścia.
            let moved = l.moving_streams.iter().all(|stream| {
                !l.nodes.contains_key(stream)
                    || l.links
                        .values()
                        .any(|(output, input)| output == stream && !virtual_ids.contains(input))
            });
            if !force && !busy && moved && !l.settle_scheduled {
                l.settle_scheduled = true;
                send_later(&tx_self, SETTLE_DELAY, Command::DestroyVirtual(l.destroy_gen));
            }
            if force {
                if busy || !moved {
                    crate::log("PipeWire: usuwam wirtualne wyjście, choć odtwarzacz się z niego nie przeniósł");
                }
                l.pending_destroy = false;
                l.moving_streams.clear();
                l.virtual_node = None; // zniszczenie proxy usuwa węzeł z grafu
            }
        })
    };

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let local = local.clone();
            let registry = registry.clone();
            let publish = publish.clone();
            let sync_virtual = sync_virtual.clone();
            let destroy_virtual_if_idle = destroy_virtual_if_idle.clone();
            move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    ObjectType::Client => {
                        if let Some(pid) = props.get("pipewire.sec.pid").and_then(|p| p.parse().ok()) {
                            local.borrow_mut().client_pids.insert(global.id, pid);
                        }
                    }
                    ObjectType::Node => {
                        let entry = NodeEntry {
                            name: props.get("node.name").unwrap_or_default().to_string(),
                            description: props.get("node.description").unwrap_or_default().to_string(),
                            class: props.get("media.class").unwrap_or_default().to_string(),
                            client: props.get("client.id").and_then(|c| c.parse().ok()),
                        };
                        let is_audio = entry.class.starts_with("Audio/");
                        local.borrow_mut().nodes.insert(global.id, entry);
                        if is_audio {
                            if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                                let id = global.id;
                                let listener = node
                                    .add_listener_local()
                                    .info({
                                        let local = local.clone();
                                        let publish = publish.clone();
                                        move |info| {
                                            let Some(props) = info.props() else { return };
                                            let mut l = local.borrow_mut();
                                            let Some(n) = l.nodes.get_mut(&id) else { return };
                                            if let Some(class) = props.get("media.class") {
                                                n.class = class.to_string();
                                            }
                                            if let Some(d) = props.get("node.description") {
                                                n.description = d.to_string();
                                            }
                                            drop(l);
                                            publish();
                                        }
                                    })
                                    .register();
                                local.borrow_mut().node_proxies.insert(id, (node, listener));
                            }
                        }
                        publish();
                    }
                    ObjectType::Link => {
                        let id_of = |key| props.get(key).and_then(|v| v.parse::<u32>().ok());
                        let (Some(out_node), Some(in_node)) =
                            (id_of("link.output.node"), id_of("link.input.node"))
                        else {
                            return;
                        };
                        local.borrow_mut().links.insert(global.id, (out_node, in_node));
                        let l = local.borrow();
                        let feedback = l.guard.as_deref().is_some_and(|guard| {
                            let ours = l.nodes.get(&out_node).is_some_and(|n| {
                                n.class == "Stream/Output/Audio"
                                    && n.client.and_then(|c| l.client_pids.get(&c)) == Some(&our_pid)
                            });
                            ours && l.nodes.get(&in_node).is_some_and(|n| n.name == guard)
                        });
                        drop(l);
                        if feedback {
                            crate::log("PipeWire: rozłączam strumień SoundPort skierowany na źródło (pętla)");
                            let _ = registry.destroy_global(global.id);
                        }
                        publish();
                        destroy_virtual_if_idle(false);
                    }
                    ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                        let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) else {
                            return;
                        };
                        let listener = metadata
                            .add_listener_local()
                            .property({
                                let local = local.clone();
                                let publish = publish.clone();
                                move |subject, key, _type, value| {
                                    if subject == 0 {
                                        let mut l = local.borrow_mut();
                                        match key {
                                            Some(CONFIGURED_SINK_KEY) => l.configured_sink = value.map(str::to_string),
                                            Some(DEFAULT_SINK_KEY) => l.default_sink = value.and_then(sink_name),
                                            None => {
                                                l.configured_sink = None;
                                                l.default_sink = None;
                                            }
                                            _ => return 0,
                                        }
                                        drop(l);
                                        publish();
                                    }
                                    0
                                }
                            })
                            .register();
                        local.borrow_mut().metadata = Some((global.id, metadata, listener));
                        sync_virtual();
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let local = local.clone();
            let publish = publish.clone();
            let destroy_virtual_if_idle = destroy_virtual_if_idle.clone();
            move |id| {
                let mut l = local.borrow_mut();
                l.client_pids.remove(&id);
                l.node_proxies.remove(&id);
                if l.metadata.as_ref().is_some_and(|(mid, _, _)| *mid == id) {
                    l.metadata = None;
                }
                let removed = l.nodes.remove(&id).is_some() | l.links.remove(&id).is_some();
                drop(l);
                if removed {
                    publish();
                    destroy_virtual_if_idle(false);
                }
            }
        })
        .register();

    let _commands = rx.attach(mainloop.loop_(), {
        let local = local.clone();
        let mainloop = mainloop.clone();
        let sync_virtual = sync_virtual.clone();
        let destroy_virtual_if_idle = destroy_virtual_if_idle.clone();
        move |command| match command {
            Command::Virtual(enabled) => {
                local.borrow_mut().want_virtual = enabled;
                sync_virtual();
                destroy_virtual_if_idle(false);
            }
            Command::Guard(guard) => local.borrow_mut().guard = guard,
            Command::DestroyVirtual(generation) => {
                if local.borrow().destroy_gen == generation {
                    destroy_virtual_if_idle(true);
                }
                let l = local.borrow();
                if l.quitting && !l.pending_destroy {
                    mainloop.quit();
                }
            }
            Command::Quit => {
                local.borrow_mut().want_virtual = false;
                local.borrow_mut().quitting = true;
                sync_virtual();
                destroy_virtual_if_idle(false);
                mainloop.quit();
            }
        }
    });

    // Dwie rundy: pierwsza przynosi obiekty grafu, druga właściwości metadanych „default”.
    do_roundtrip(&mainloop, &core);
    do_roundtrip(&mainloop, &core);
    let _ = ready.send(());

    mainloop.run();
    // Zamykanie: czekamy, aż odtwarzacze przeniosą się z wirtualnego wyjścia i zostanie ono
    // usunięte (`DestroyVirtual` zakończy wtedy pętlę) — najdłużej `DESTROY_DELAY`.
    if local.borrow().pending_destroy {
        mainloop.run();
        destroy_virtual_if_idle(true);
    }
    // Ostatnia runda, żeby zmiany dotarły do serwera przed rozłączeniem.
    do_roundtrip(&mainloop, &core);
    Ok(())
}

/// `{"name":"bluez_output.XX"}` → `bluez_output.XX`.
fn sink_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

fn do_roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
    let Ok(pending) = core.sync(0) else { return };
    let done = Rc::new(std::cell::Cell::new(false));
    let _listener = core
        .add_listener_local()
        .done({
            let done = done.clone();
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    // Zabezpieczenie przed zawieszeniem przy zamykaniu, gdy serwer nie odpowiada.
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let _ = timer.update_timer(Some(std::time::Duration::from_secs(1)), None);
    if !done.get() {
        mainloop.run(); // kończy się po odpowiedzi serwera albo po 1 s
    }
}

#[cfg(test)]
mod tests {
    use super::sink_name;

    #[test]
    fn parses_default_sink_json() {
        assert_eq!(sink_name(r#"{ "name": "bluez_output.C4:33" }"#).as_deref(), Some("bluez_output.C4:33"));
        assert_eq!(sink_name("nonsense"), None);
    }
}
