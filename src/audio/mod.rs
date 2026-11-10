//! Silnik audio: przechwytuje dźwięk z urządzenia źródłowego (WASAPI loopback na Windows,
//! monitor sinka na PipeWire) i rozsyła go równolegle na wiele urządzeń wyjściowych.
//!
//! Przepływ danych:
//! ```text
//!   [loopback źródła] --f32--> rtrb SPSC ──> [Resampler + strumień wyjścia #1]
//!                       └────> rtrb SPSC ──> [Resampler + strumień wyjścia #2] ...
//! ```
//! Wątki audio nie blokują się nawzajem (kolejki lock-free); główny wątek tylko
//! zestawia/rozbiera strumienie i przekazuje parametry przez atomiki.

mod endpoint_volume;
#[cfg(target_os = "linux")]
mod pw_graph;
#[cfg(target_os = "linux")]
mod pw_stream;
mod resampler;
#[cfg(target_os = "linux")]
mod sink_volume;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{DeviceId, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};

use endpoint_volume::VolumeWatch;
use resampler::Resampler;

/// Pojemność kolejki między źródłem a wyjściem (w sekundach dźwięku).
const QUEUE_SECONDS: usize = 1;
/// Po nieudanym otwarciu urządzenia nie próbujemy ponownie częściej niż co tyle.
const RETRY_AFTER: Duration = Duration::from_secs(5);
const STREAM_TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));
/// Linux: strumień, który po tylu sekundach nadal nie jest podłączony w grafie PipeWire
/// (WirePlumber nie zestawił połączenia albo słuchawki na chwilę się rozłączyły),
/// otwieramy od nowa.
#[cfg(target_os = "linux")]
const LINK_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputRoute {
    pub device_id: String,
    /// 0..=1
    pub volume: f32,
    /// Dodatkowe opóźnienie do wyrównania słuchawek względem siebie.
    pub delay_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutingConfig {
    /// None = domyślne urządzenie systemowe.
    pub source: Option<String>,
    pub base_latency_ms: u32,
    pub outputs: Vec<OutputRoute>,
}

/// Wywoływane z wątków audio, gdy coś wymaga uwagi głównego wątku (np. rozłączenie słuchawek).
pub type Notify = Arc<dyn Fn() + Send + Sync>;

pub struct Engine {
    host: cpal::Host,
    notify: Notify,
    running: bool,
    source: Option<Source>,
    outputs: HashMap<String, Output>,
    failed_at: HashMap<String, Instant>,
    errors: Vec<String>,
    /// Czy `host` odświeżono w bieżącym `apply` (cpal czyta listę urządzeń PipeWire tylko raz).
    host_fresh: bool,
    #[cfg(target_os = "linux")]
    graph: pw_graph::PwGraph,
    #[cfg(target_os = "linux")]
    volumes: sink_volume::SinkVolumes,
    /// Linux: domyślne wyjście sprzed włączenia wirtualnego — od niego wirtualne
    /// wyjście przejmuje głośność, żeby po przełączeniu nie zrobiło się nagle głośno.
    #[cfg(target_os = "linux")]
    previous_default: Option<String>,
    /// Linux: słuchawki czekające na podgłośnienie do 100% (patrz `raise_pending_volumes`).
    #[cfg(target_os = "linux")]
    pending_raise: std::collections::HashSet<String>,
}

/// Identyfikator cpal wirtualnego wyjścia SoundPort (Linux).
#[cfg(target_os = "linux")]
const VIRTUAL_SOURCE_ID: &str = "pipewire:soundport";

impl Engine {
    pub fn new(notify: Notify) -> Self {
        Self {
            host: cpal::default_host(),
            #[cfg(target_os = "linux")]
            graph: pw_graph::PwGraph::start(notify.clone()),
            #[cfg(target_os = "linux")]
            volumes: sink_volume::SinkVolumes::default(),
            #[cfg(target_os = "linux")]
            previous_default: None,
            #[cfg(target_os = "linux")]
            pending_raise: Default::default(),
            notify,
            running: false,
            source: None,
            outputs: HashMap::new(),
            failed_at: HashMap::new(),
            errors: Vec::new(),
            host_fresh: false,
        }
    }

    /// Nazwa opcji „źródło domyślne” (`source = None`) w menu i oknie ustawień.
    pub fn default_source_label(&self, devices: &[DeviceInfo]) -> String {
        if cfg!(target_os = "linux") {
            "Wirtualne wyjście SoundPort (zalecane)".into()
        } else {
            devices
                .iter()
                .find(|d| d.is_default)
                .map(|d| format!("Domyślne urządzenie ({})", d.name))
                .unwrap_or_else(|| "Domyślne urządzenie".into())
        }
    }

    /// Linux: lista z obserwatora grafu PipeWire — tylko prawdziwe wyjścia, zawsze aktualna.
    #[cfg(target_os = "linux")]
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let default = self.graph.default_sink();
        let mut list: Vec<DeviceInfo> = self
            .graph
            .sinks()
            .into_iter()
            .map(|s| DeviceInfo {
                is_default: default.as_deref() == Some(s.name.as_str()),
                id: format!("pipewire:{}", s.name),
                name: s.description,
            })
            .collect();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        list
    }

    #[cfg(not(target_os = "linux"))]
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let default_id = self
            .host
            .default_output_device()
            .and_then(|d| d.id().ok())
            .map(|id| id.to_string());
        let Ok(devices) = self.host.output_devices() else {
            return Vec::new();
        };
        let mut list: Vec<DeviceInfo> = devices
            .filter_map(|d| {
                let id = d.id().ok()?.to_string();
                let name = d
                    .description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| id.clone());
                Some(DeviceInfo {
                    is_default: default_id.as_deref() == Some(id.as_str()),
                    id,
                    name,
                })
            })
            .collect();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        list
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Faktycznie przechwytywane urządzenie.
    pub fn active_source(&self) -> Option<&str> {
        self.source.as_ref().map(|s| s.device_id.as_str())
    }

    /// Głośność źródła ustawiona w systemie (mnożnik 0..=1), stosowana na wszystkich wyjściach.
    pub fn source_gain(&self) -> Option<f32> {
        self.source
            .as_ref()
            .map(|s| f32::from_bits(s.volume.gain().load(Ordering::Relaxed)))
    }

    pub fn is_output_active(&self, device_id: &str) -> bool {
        self.outputs
            .get(device_id)
            .is_some_and(|o| !o.control.faulted.load(Ordering::Relaxed))
    }

    /// Aktualne zapełnienie bufora wyjścia (≈ opóźnienie dodawane przez SoundPort).
    pub fn output_buffer_ms(&self, device_id: &str) -> Option<u32> {
        self.outputs
            .get(device_id)
            .filter(|o| !o.control.faulted.load(Ordering::Relaxed))
            .map(|o| o.control.buffered_ms.load(Ordering::Relaxed))
    }

    pub fn active_output_count(&self) -> usize {
        self.outputs
            .values()
            .filter(|o| !o.control.faulted.load(Ordering::Relaxed))
            .count()
    }

    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.outputs.clear();
        self.source = None;
        self.failed_at.clear();
        #[cfg(target_os = "linux")]
        {
            // Najpierw przyciszamy słuchawki, dopiero potem usuwamy wirtualne wyjście — po jego
            // zniknięciu odtwarzacze wracają prosto na słuchawki i nie mogą trafić na 100%.
            for name in self.volumes.saved_names() {
                let id = self.graph.volume_id(&name);
                self.volumes.restore(&name, id);
            }
            self.graph.set_guard(None);
            self.graph.set_virtual(false);
            self.previous_default = None;
            self.pending_raise.clear();
        }
    }

    /// Linux: czy strumień po czasie ochronnym nadal nie jest podłączony w grafie.
    #[cfg(target_os = "linux")]
    fn is_unlinked(&self, device_id: &str, opened_at: Instant, capture: bool) -> bool {
        let Some(name) = device_id.strip_prefix("pipewire:") else {
            return false;
        };
        let linked = if capture {
            self.graph.is_captured(name)
        } else {
            self.graph.is_output_linked(name)
        };
        let unlinked = !linked && opened_at.elapsed() >= LINK_GRACE;
        if unlinked {
            crate::log(&format!("Strumień {device_id} nie jest podłączony — otwieram ponownie"));
        }
        unlinked
    }

    #[cfg(not(target_os = "linux"))]
    fn is_unlinked(&self, _device_id: &str, _opened_at: Instant, _capture: bool) -> bool {
        false
    }

    /// Linux, tryb z wirtualnym wyjściem: słuchawki pójdą na 100%, głośnością steruje SoundPort.
    #[cfg(target_os = "linux")]
    fn after_output_opened(&mut self, device_id: &str) {
        let virtual_source = self.source.as_ref().is_some_and(|s| s.device_id == VIRTUAL_SOURCE_ID);
        if let (true, Some(name)) = (virtual_source, device_id.strip_prefix("pipewire:")) {
            self.pending_raise.insert(name.to_string());
        }
    }

    /// Podgłaśnia słuchawki dopiero, gdy nie gra do nich bezpośrednio żaden inny program.
    /// WirePlumber przenosi odtwarzacze na nowe domyślne wyjście z opóźnieniem — gdybyśmy
    /// podgłośnili wcześniej, odtwarzacz zagrałby przez chwilę na pełnej głośności.
    #[cfg(target_os = "linux")]
    fn raise_pending_volumes(&mut self) {
        let ready = self.graph.default_sink().as_deref() == Some(pw_graph::VIRTUAL_NODE);
        if !ready {
            return;
        }
        let names: Vec<String> = self
            .pending_raise
            .iter()
            .filter(|name| self.outputs.contains_key(&format!("pipewire:{name}")))
            .filter(|name| !self.graph.has_foreign_streams(name))
            .cloned()
            .collect();
        for name in names {
            self.pending_raise.remove(&name);
            if let Some(id) = self.graph.volume_id(&name) {
                self.volumes.raise(&name, id);
            }
        }
    }

    /// Linux: wyjście odznaczone przez użytkownika — oddajemy mu jego głośność.
    #[cfg(target_os = "linux")]
    fn after_output_removed(&mut self, device_id: &str) {
        if let Some(name) = device_id.strip_prefix("pipewire:") {
            self.pending_raise.remove(name);
            let id = self.graph.volume_id(name);
            self.volumes.restore(name, id);
        }
    }

    /// Linux: nowo otwarte wirtualne wyjście przejmuje głośność poprzedniego domyślnego.
    #[cfg(target_os = "linux")]
    fn after_source_opened(&mut self, device_id: &str) {
        if device_id != VIRTUAL_SOURCE_ID {
            return;
        }
        let Some(previous) = self.previous_default.clone() else { return };
        let (Some(prev_id), Some(own_id)) = (
            self.graph.volume_id(&previous),
            self.graph.volume_id(pw_graph::VIRTUAL_NODE),
        ) else {
            return;
        };
        if let Some(volume) = self.volumes.original_or_current(&previous, prev_id) {
            sink_volume::set(own_id, volume);
        }
    }

    /// Źródło do przechwytywania. Windows: wybrane, a gdy niedostępne — domyślne urządzenie.
    #[cfg(not(target_os = "linux"))]
    fn resolve_source(&mut self, config: &RoutingConfig, devices: &[DeviceInfo]) -> Option<String> {
        config
            .source
            .as_deref()
            .filter(|id| devices.iter().any(|d| d.id == *id))
            .map(str::to_string)
            .or_else(|| devices.iter().find(|d| d.is_default).map(|d| d.id.clone()))
    }

    /// Linux: wybrane prawdziwe wyjście, a domyślnie (lub gdy niedostępne) wirtualne wyjście
    /// SoundPort, tworzone na czas działania i ustawiane jako domyślne wyjście systemu.
    #[cfg(target_os = "linux")]
    fn resolve_source(&mut self, config: &RoutingConfig, devices: &[DeviceInfo]) -> Option<String> {
        let chosen = config
            .source
            .as_deref()
            .filter(|id| devices.iter().any(|d| d.id == *id))
            .map(str::to_string);
        if chosen.is_none() && self.previous_default.is_none() {
            self.previous_default = self
                .graph
                .default_sink()
                .filter(|name| name != pw_graph::VIRTUAL_NODE);
        }
        self.graph.set_virtual(chosen.is_none());
        let source = chosen.or_else(|| {
            // Węzeł pojawia się chwilę po utworzeniu; obserwator grafu wywoła wtedy `notify`.
            self.graph.virtual_ready().then(|| VIRTUAL_SOURCE_ID.to_string())
        });
        self.graph.set_guard(
            source
                .as_deref()
                .and_then(|id| id.strip_prefix("pipewire:"))
                .map(str::to_string),
        );
        source
    }

    /// Urządzenie cpal o danym id. Na PipeWire cpal zna tylko urządzenia sprzed utworzenia
    /// hosta (a ponownie połączone słuchawki mają nowy numer węzła), więc przed pierwszym
    /// otwarciem w danym `apply` tworzymy hosta od nowa.
    fn open_device(&mut self, id: &str) -> Result<cpal::Device> {
        if cfg!(target_os = "linux") && !self.host_fresh {
            self.host = cpal::default_host();
            self.host_fresh = true;
        }
        let device_id = DeviceId::from_str(id).map_err(|e| anyhow!("{e}"))?;
        self.host
            .device_by_id(&device_id)
            .ok_or_else(|| anyhow!("urządzenie niedostępne"))
    }

    /// Uruchamia routing lub doprowadza go do zadanej konfiguracji.
    /// Idempotentne — wywoływane po każdej zmianie ustawień i cyklicznie (reconnect słuchawek).
    pub fn apply(&mut self, config: &RoutingConfig, devices: &[DeviceInfo]) {
        self.running = true;
        self.host_fresh = false;
        let is_present = |id: &str| devices.iter().any(|d| d.id == id);

        // 1. Źródło: wybrane, a jeśli niedostępne — domyślne.
        let source_id = self.resolve_source(config, devices);

        let source_ok = self.source.as_ref().is_some_and(|s| {
            Some(&s.device_id) == source_id.as_ref()
                && !s.faulted.load(Ordering::Relaxed)
                && !self.is_unlinked(&s.device_id, s.opened_at, true)
        });
        if !source_ok {
            // Zmiana źródła = inny format próbek, więc wyjścia budujemy od nowa.
            self.outputs.clear();
            self.source = None;
            if let Some(id) = source_id.as_deref() {
                if self.may_try(id) {
                    match self
                        .open_device(id)
                        .and_then(|device| Source::open(&device, id, self.notify.clone()))
                    {
                        Ok(source) => {
                            self.failed_at.remove(id);
                            self.source = Some(source);
                            #[cfg(target_os = "linux")]
                            self.after_source_opened(id);
                        }
                        Err(e) => self.report_failure(id, devices, "źródła", e),
                    }
                }
            }
        }
        let Some(source) = self.source.as_ref() else {
            return;
        };

        // 2. Wyjścia: usuń zbędne i zepsute, dodaj brakujące, zaktualizuj parametry.
        let wanted: HashMap<&str, &OutputRoute> = config
            .outputs
            .iter()
            .filter(|o| o.device_id != source.device_id) // nie odtwarzamy źródła na nim samym
            .map(|o| (o.device_id.as_str(), o))
            .collect();

        // Na Linuksie usuwamy też wyjścia rozłączonych słuchawek: cpal nie zawsze zgłasza błąd,
        // a WirePlumber przeniósłby taki strumień na domyślne wyjście.
        let drop_missing = cfg!(target_os = "linux");
        let unlinked: Vec<String> = self
            .outputs
            .iter()
            .filter(|(id, out)| self.is_unlinked(id, out.opened_at, false))
            .map(|(id, _)| id.clone())
            .collect();
        let unwanted: Vec<String> = self
            .outputs
            .keys()
            .filter(|id| !wanted.contains_key(id.as_str()))
            .cloned()
            .collect();
        self.outputs.retain(|id, out| {
            wanted.contains_key(id.as_str())
                && !out.control.faulted.load(Ordering::Relaxed)
                && (!drop_missing || is_present(id))
                && !unlinked.contains(id)
        });
        #[cfg(target_os = "linux")]
        for id in &unwanted {
            self.after_output_removed(id);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = unwanted;

        for (id, route) in &wanted {
            let latency_ms = config.base_latency_ms + route.delay_ms;
            if let Some(out) = self.outputs.get(*id) {
                out.control.set(route.volume, latency_ms);
                continue;
            }
            if !is_present(id) || !self.may_try(id) {
                continue;
            }
            #[cfg(not(target_os = "linux"))]
            let opened = self.open_device(id).and_then(|device| {
                let source = self.source.as_ref().expect("source checked above");
                Output::open(&device, source, route.volume, latency_ms, self.notify.clone())
            });
            #[cfg(target_os = "linux")]
            let opened = {
                let source = self.source.as_ref().expect("source checked above");
                let description = devices.iter().find(|d| d.id == *id).map_or(*id, |d| d.name.as_str());
                Output::open(id, description, source, route.volume, latency_ms, self.notify.clone())
            };
            match opened {
                Ok(out) => {
                    self.failed_at.remove(*id);
                    self.outputs.insert(id.to_string(), out);
                    #[cfg(target_os = "linux")]
                    self.after_output_opened(id);
                }
                Err(e) => self.report_failure(id, devices, "wyjścia", e),
            }
        }

        self.source
            .as_ref()
            .expect("source checked above")
            .set_sinks(&mut self.outputs);
        #[cfg(target_os = "linux")]
        self.raise_pending_volumes();
    }

    fn may_try(&self, id: &str) -> bool {
        self.failed_at
            .get(id)
            .is_none_or(|t| t.elapsed() >= RETRY_AFTER)
    }

    fn report_failure(&mut self, id: &str, devices: &[DeviceInfo], what: &str, e: anyhow::Error) {
        let first_failure = !self.failed_at.contains_key(id);
        self.failed_at.insert(id.to_string(), Instant::now());
        if first_failure {
            let name = devices
                .iter()
                .find(|d| d.id == id)
                .map_or(id, |d| d.name.as_str());
            self.errors.push(format!("Nie można otworzyć {what} „{name}”: {e:#}"));
        }
    }
}

// ---------------------------------------------------------------------------------------------

/// Czy błąd strumienia oznacza, że strumień przestał działać (np. rozłączone słuchawki).
fn is_fatal(err: &cpal::Error) -> bool {
    !matches!(
        err.kind(),
        cpal::ErrorKind::RealtimeDenied | cpal::ErrorKind::DeviceChanged
    )
}

struct Sink {
    device_id: String,
    producer: rtrb::Producer<f32>,
}

struct Source {
    device_id: String,
    opened_at: Instant,
    sample_rate: u32,
    channels: u16,
    sinks: Arc<Mutex<Vec<Sink>>>,
    faulted: Arc<AtomicBool>,
    _capture: cpal::Stream,
    /// Strumień ciszy na urządzeniu źródłowym: WASAPI loopback nie dostarcza danych, gdy
    /// nic nie gra — ciągła cisza utrzymuje stały przepływ i synchronizację wyjść.
    /// Monitor sinka w PipeWire daje ciszę sam, więc na Linuksie to zbędne.
    #[cfg(windows)]
    _keep_alive: Option<cpal::Stream>,
    /// Głośność źródła z Windows — loopback jej nie uwzględnia, więc stosują ją wyjścia.
    volume: VolumeWatch,
}

impl Source {
    fn open(device: &cpal::Device, id: &str, notify: Notify) -> Result<Self> {
        let supported = device
            .default_output_config()
            .context("brak formatu urządzenia")?;
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let sinks: Arc<Mutex<Vec<Sink>>> = Arc::new(Mutex::new(Vec::new()));
        let faulted = Arc::new(AtomicBool::new(false));

        #[cfg(windows)]
        let keep_alive = build_silence(device, &config, supported.sample_format()).ok();
        #[cfg(windows)]
        if let Some(s) = &keep_alive {
            let _ = s.play();
        }

        // Strumień wejściowy na urządzeniu wyjściowym = WASAPI loopback / monitor sinka PipeWire.
        let capture = match supported.sample_format() {
            SampleFormat::F32 => build_capture::<f32>(device, &config, &sinks, &faulted, notify),
            SampleFormat::I16 => build_capture::<i16>(device, &config, &sinks, &faulted, notify),
            SampleFormat::I32 => build_capture::<i32>(device, &config, &sinks, &faulted, notify),
            SampleFormat::U16 => build_capture::<u16>(device, &config, &sinks, &faulted, notify),
            other => return Err(anyhow!("nieobsługiwany format próbek {other:?}")),
        }?;
        capture.play().context("start przechwytywania")?;

        Ok(Self {
            device_id: id.to_string(),
            opened_at: Instant::now(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            sinks,
            faulted,
            _capture: capture,
            #[cfg(windows)]
            _keep_alive: keep_alive,
            volume: VolumeWatch::start(id),
        })
    }

    /// Podmienia zbiór odbiorców: zachowuje istniejące kolejki, dodaje producentów nowych wyjść.
    fn set_sinks(&self, outputs: &mut HashMap<String, Output>) {
        let has_pending = outputs.values().any(|o| o.pending_producer.is_some());
        let mut sinks = self.sinks.lock().unwrap_or_else(|e| e.into_inner());
        if !has_pending && sinks.len() == outputs.len() {
            return; // nic się nie zmieniło — nie zatrzymujemy wątku przechwytywania
        }
        // Porzucona kolejka = strumień wyjścia już zniknął (także przy ponownym połączeniu tego samego id).
        sinks.retain(|s| outputs.contains_key(&s.device_id) && !s.producer.is_abandoned());
        for (id, out) in outputs.iter_mut() {
            if let Some(producer) = out.pending_producer.take() {
                sinks.push(Sink {
                    device_id: id.clone(),
                    producer,
                });
            }
        }
    }
}

fn build_capture<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    sinks: &Arc<Mutex<Vec<Sink>>>,
    faulted: &Arc<AtomicBool>,
    notify: Notify,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let sinks = sinks.clone();
    let faulted = faulted.clone();
    let mut scratch: Vec<f32> = Vec::with_capacity(16_384);
    let stream = device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _| {
            scratch.clear();
            scratch.extend(data.iter().map(|&s| f32::from_sample(s)));
            // Lock trzymany przez główny wątek tylko na mikrosekundy (zmiana listy wyjść).
            let Ok(mut sinks) = sinks.lock() else { return };
            for sink in sinks.iter_mut() {
                // Wysyłamy tylko pełne ramki; gdy wyjście nie nadąża — nadmiar odrzucamy.
                let n = (sink.producer.slots() / channels * channels).min(scratch.len());
                if n == 0 {
                    continue;
                }
                if let Ok(chunk) = sink.producer.write_chunk_uninit(n) {
                    chunk.fill_from_iter(scratch[..n].iter().copied());
                }
            }
        },
        move |err| {
            if is_fatal(&err) {
                faulted.store(true, Ordering::Relaxed);
                notify();
            }
        },
        STREAM_TIMEOUT,
    )?;
    Ok(stream)
}

#[cfg(windows)]
fn build_silence(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
) -> Result<cpal::Stream> {
    // Bufor jest wstępnie wypełniony ciszą przez cpal, więc callback nic nie robi.
    let stream = device.build_output_stream_raw(
        *config,
        format,
        |_data, _| {},
        |_err| {},
        STREAM_TIMEOUT,
    )?;
    Ok(stream)
}

// ---------------------------------------------------------------------------------------------

/// Parametry wyjścia współdzielone z wątkiem audio.
struct OutputControl {
    volume_bits: AtomicU32,
    latency_ms: AtomicU32,
    faulted: AtomicBool,
    /// Aktualny poziom bufora (diagnostyka/UI).
    buffered_ms: AtomicU32,
}

impl OutputControl {
    fn set(&self, volume: f32, latency_ms: u32) {
        self.volume_bits
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.latency_ms.store(latency_ms, Ordering::Relaxed);
    }
}

struct Output {
    control: Arc<OutputControl>,
    /// Producent kolejki — przekazywany do źródła przy najbliższym `set_sinks`.
    pending_producer: Option<rtrb::Producer<f32>>,
    opened_at: Instant,
    #[cfg(not(target_os = "linux"))]
    _stream: cpal::Stream,
    #[cfg(target_os = "linux")]
    _stream: pw_stream::PwOutput,
}

impl Output {
    fn new_control(volume: f32, latency_ms: u32) -> Arc<OutputControl> {
        let control = Arc::new(OutputControl {
            volume_bits: AtomicU32::new(0),
            latency_ms: AtomicU32::new(0),
            faulted: AtomicBool::new(false),
            buffered_ms: AtomicU32::new(0),
        });
        control.set(volume, latency_ms);
        control
    }

    fn new_queue(source: &Source) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
        let capacity = source.sample_rate as usize * source.channels as usize * QUEUE_SECONDS;
        rtrb::RingBuffer::<f32>::new(capacity)
    }

    /// Windows: strumień cpal w natywnym formacie urządzenia.
    #[cfg(not(target_os = "linux"))]
    fn open(
        device: &cpal::Device,
        source: &Source,
        volume: f32,
        latency_ms: u32,
        notify: Notify,
    ) -> Result<Self> {
        let supported = device
            .default_output_config()
            .context("brak formatu urządzenia")?;
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let control = Self::new_control(volume, latency_ms);
        let (producer, consumer) = Self::new_queue(source);
        let processor = OutputProcessor::new(
            consumer,
            Resampler::new(source.sample_rate, source.channels, config.sample_rate, config.channels),
            control.clone(),
            source.volume.gain(),
            config.channels,
        );
        let args = (device, &config, processor, control.clone(), notify);
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_output::<f32>(args),
            SampleFormat::I16 => build_output::<i16>(args),
            SampleFormat::I32 => build_output::<i32>(args),
            SampleFormat::U16 => build_output::<u16>(args),
            other => return Err(anyhow!("nieobsługiwany format próbek {other:?}")),
        }?;
        stream.play().context("start odtwarzania")?;

        Ok(Self {
            control,
            pending_producer: Some(producer),
            opened_at: Instant::now(),
            _stream: stream,
        })
    }

    /// Linux: własny strumień PipeWire w formacie źródła (konwersję do formatu słuchawek
    /// robi PipeWire, a resampler wyrównuje tylko dryf zegarów).
    #[cfg(target_os = "linux")]
    fn open(
        id: &str,
        description: &str,
        source: &Source,
        volume: f32,
        latency_ms: u32,
        notify: Notify,
    ) -> Result<Self> {
        let target = id.strip_prefix("pipewire:").unwrap_or(id);
        let control = Self::new_control(volume, latency_ms);
        let (producer, consumer) = Self::new_queue(source);
        let mut processor = OutputProcessor::new(
            consumer,
            Resampler::new(source.sample_rate, source.channels, source.sample_rate, source.channels),
            control.clone(),
            source.volume.gain(),
            source.channels,
        );
        let error_control = control.clone();
        let stream = pw_stream::PwOutput::open(
            target,
            description,
            source.sample_rate,
            u32::from(source.channels),
            move |buf| processor.fill(buf),
            move || {
                error_control.faulted.store(true, Ordering::Relaxed);
                notify();
            },
        )?;
        Ok(Self {
            control,
            pending_producer: Some(producer),
            opened_at: Instant::now(),
            _stream: stream,
        })
    }
}

/// Wypełnianie bufora wyjścia (wątek audio): kolejka → resampler → głośność.
struct OutputProcessor {
    consumer: rtrb::Consumer<f32>,
    resampler: Resampler,
    control: Arc<OutputControl>,
    source_gain: Arc<AtomicU32>,
    channels: usize,
    gain: f32,
}

impl OutputProcessor {
    fn new(
        consumer: rtrb::Consumer<f32>,
        resampler: Resampler,
        control: Arc<OutputControl>,
        source_gain: Arc<AtomicU32>,
        channels: u16,
    ) -> Self {
        Self {
            consumer,
            resampler,
            control,
            source_gain,
            channels: channels as usize,
            gain: f32::NAN,
        }
    }

    fn fill(&mut self, out: &mut [f32]) {
        // Przenieś wszystko, co przyszło ze źródła, do bufora resamplera.
        let available = self.consumer.slots();
        if available > 0 {
            if let Ok(chunk) = self.consumer.read_chunk(available) {
                let (a, b) = chunk.as_slices();
                self.resampler.push(a);
                self.resampler.push(b);
                chunk.commit_all();
            }
        }
        let target = f32::from_bits(self.control.volume_bits.load(Ordering::Relaxed))
            * f32::from_bits(self.source_gain.load(Ordering::Relaxed));
        let latency_ms = self.control.latency_ms.load(Ordering::Relaxed);
        self.resampler.process(out, 1.0, latency_ms);
        // Zmiana głośności rozłożona na cały bufor — bez trzasków przy ruchu suwaka.
        let start = if self.gain.is_nan() { target } else { self.gain };
        let frames = (out.len() / self.channels).max(1);
        for (i, frame) in out.chunks_mut(self.channels).enumerate() {
            let g = start + (target - start) * (i + 1) as f32 / frames as f32;
            frame.iter_mut().for_each(|s| *s *= g);
        }
        self.gain = target;
        self.control
            .buffered_ms
            .store(self.resampler.buffered_ms(), Ordering::Relaxed);
    }
}

#[cfg(not(target_os = "linux"))]
type OutputArgs<'a> = (
    &'a cpal::Device,
    &'a StreamConfig,
    OutputProcessor,
    Arc<OutputControl>,
    Notify,
);

#[cfg(not(target_os = "linux"))]
fn build_output<T>(
    (device, config, mut processor, error_control, notify): OutputArgs<'_>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::with_capacity(16_384);
    let stream = device.build_output_stream::<T, _, _>(
        *config,
        move |data: &mut [T], _| {
            scratch.resize(data.len(), 0.0);
            processor.fill(&mut scratch);
            for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                *dst = T::from_sample(src);
            }
        },
        move |err| {
            if is_fatal(&err) {
                error_control.faulted.store(true, Ordering::Relaxed);
                notify();
            }
        },
        STREAM_TIMEOUT,
    )?;
    Ok(stream)
}
