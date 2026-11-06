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

mod resampler;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{DeviceId, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};

use resampler::Resampler;

/// Pojemność kolejki między źródłem a wyjściem (w sekundach dźwięku).
const QUEUE_SECONDS: usize = 1;
/// Po nieudanym otwarciu urządzenia nie próbujemy ponownie częściej niż co tyle.
const RETRY_AFTER: Duration = Duration::from_secs(5);
const STREAM_TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));

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
}

impl Engine {
    pub fn new(notify: Notify) -> Self {
        Self {
            host: cpal::default_host(),
            notify,
            running: false,
            source: None,
            outputs: HashMap::new(),
            failed_at: HashMap::new(),
            errors: Vec::new(),
        }
    }

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
    }

    /// Uruchamia routing lub doprowadza go do zadanej konfiguracji.
    /// Idempotentne — wywoływane po każdej zmianie ustawień i cyklicznie (reconnect słuchawek).
    pub fn apply(&mut self, config: &RoutingConfig, devices: &[DeviceInfo]) {
        self.running = true;
        let is_present = |id: &str| devices.iter().any(|d| d.id == id);

        // 1. Źródło: wybrane, a jeśli niedostępne — domyślne.
        let source_id = config
            .source
            .as_deref()
            .filter(|id| is_present(id))
            .map(str::to_string)
            .or_else(|| devices.iter().find(|d| d.is_default).map(|d| d.id.clone()));

        let source_ok = self.source.as_ref().is_some_and(|s| {
            Some(&s.device_id) == source_id.as_ref() && !s.faulted.load(Ordering::Relaxed)
        });
        if !source_ok {
            // Zmiana źródła = inny format próbek, więc wyjścia budujemy od nowa.
            self.outputs.clear();
            self.source = None;
            if let Some(id) = source_id.as_deref() {
                if self.may_try(id) {
                    match Source::open(&self.host, id, self.notify.clone()) {
                        Ok(source) => {
                            self.failed_at.remove(id);
                            self.source = Some(source);
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

        self.outputs.retain(|id, out| {
            wanted.contains_key(id.as_str()) && !out.control.faulted.load(Ordering::Relaxed)
        });

        for (id, route) in &wanted {
            let latency_ms = config.base_latency_ms + route.delay_ms;
            if let Some(out) = self.outputs.get(*id) {
                out.control.set(route.volume, latency_ms);
                continue;
            }
            if !is_present(id) || !self.may_try(id) {
                continue;
            }
            let source = self.source.as_ref().expect("source checked above");
            match Output::open(&self.host, id, source, route.volume, latency_ms, self.notify.clone()) {
                Ok(out) => {
                    self.failed_at.remove(*id);
                    self.outputs.insert(id.to_string(), out);
                }
                Err(e) => self.report_failure(id, devices, "wyjścia", e),
            }
        }

        self.source
            .as_ref()
            .expect("source checked above")
            .set_sinks(&mut self.outputs);
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

fn find_device(host: &cpal::Host, id: &str) -> Result<cpal::Device> {
    let device_id = DeviceId::from_str(id).map_err(|e| anyhow!("{e}"))?;
    host.device_by_id(&device_id)
        .ok_or_else(|| anyhow!("urządzenie niedostępne"))
}

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
    sample_rate: u32,
    channels: u16,
    sinks: Arc<Mutex<Vec<Sink>>>,
    faulted: Arc<AtomicBool>,
    _capture: cpal::Stream,
    /// Strumień ciszy na urządzeniu źródłowym: WASAPI loopback nie dostarcza danych, gdy
    /// nic nie gra — ciągła cisza utrzymuje stały przepływ i synchronizację wyjść.
    _keep_alive: Option<cpal::Stream>,
}

impl Source {
    fn open(host: &cpal::Host, id: &str, notify: Notify) -> Result<Self> {
        let device = find_device(host, id)?;
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

        let keep_alive = build_silence(&device, &config, supported.sample_format()).ok();
        if let Some(s) = &keep_alive {
            let _ = s.play();
        }

        // Na WASAPI strumień wejściowy na urządzeniu wyjściowym = tryb loopback.
        let capture = match supported.sample_format() {
            SampleFormat::F32 => build_capture::<f32>(&device, &config, &sinks, &faulted, notify),
            SampleFormat::I16 => build_capture::<i16>(&device, &config, &sinks, &faulted, notify),
            SampleFormat::I32 => build_capture::<i32>(&device, &config, &sinks, &faulted, notify),
            SampleFormat::U16 => build_capture::<u16>(&device, &config, &sinks, &faulted, notify),
            other => return Err(anyhow!("nieobsługiwany format próbek {other:?}")),
        }?;
        capture.play().context("start przechwytywania")?;

        Ok(Self {
            device_id: id.to_string(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            sinks,
            faulted,
            _capture: capture,
            _keep_alive: keep_alive,
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
    _stream: cpal::Stream,
}

impl Output {
    fn open(
        host: &cpal::Host,
        id: &str,
        source: &Source,
        volume: f32,
        latency_ms: u32,
        notify: Notify,
    ) -> Result<Self> {
        let device = find_device(host, id)?;
        let supported = device
            .default_output_config()
            .context("brak formatu urządzenia")?;
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let control = Arc::new(OutputControl {
            volume_bits: AtomicU32::new(0),
            latency_ms: AtomicU32::new(0),
            faulted: AtomicBool::new(false),
            buffered_ms: AtomicU32::new(0),
        });
        control.set(volume, latency_ms);

        let capacity = source.sample_rate as usize * source.channels as usize * QUEUE_SECONDS;
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
        let resampler = Resampler::new(
            source.sample_rate,
            source.channels,
            config.sample_rate,
            config.channels,
        );

        let args = (&device, &config, consumer, resampler, control.clone(), notify);
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
            _stream: stream,
        })
    }
}

type OutputArgs<'a> = (
    &'a cpal::Device,
    &'a StreamConfig,
    rtrb::Consumer<f32>,
    Resampler,
    Arc<OutputControl>,
    Notify,
);

fn build_output<T>(
    (device, config, mut consumer, mut resampler, control, notify): OutputArgs<'_>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let error_control = control.clone();
    let mut scratch: Vec<f32> = Vec::with_capacity(16_384);
    let stream = device.build_output_stream::<T, _, _>(
        *config,
        move |data: &mut [T], _| {
            // Przenieś wszystko, co przyszło ze źródła, do bufora resamplera.
            let available = consumer.slots();
            if available > 0 {
                if let Ok(chunk) = consumer.read_chunk(available) {
                    let (a, b) = chunk.as_slices();
                    resampler.push(a);
                    resampler.push(b);
                    chunk.commit_all();
                }
            }
            let volume = f32::from_bits(control.volume_bits.load(Ordering::Relaxed));
            let latency_ms = control.latency_ms.load(Ordering::Relaxed);
            scratch.resize(data.len(), 0.0);
            resampler.process(&mut scratch, volume, latency_ms);
            control
                .buffered_ms
                .store(resampler.buffered_ms(), Ordering::Relaxed);
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
