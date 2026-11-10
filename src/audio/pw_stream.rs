//! Linux: strumień odtwarzania PipeWire dla jednego wyjścia (zamiast strumienia cpal).
//!
//! cpal dokleja każdemu strumieniowi `node.group = cpal-<pid>`, przez co PipeWire spina
//! wszystkie wyjścia w jedną grupę taktowaną zegarem jednych słuchawek. Drugie słuchawki
//! Bluetooth chodzą wtedy jako „follower” cudzego zegara, a przy niestabilnym łączu potrafią
//! utknąć w ciszy, choć graf pokazuje, że grają. Własny strumień nie ma `node.group`, więc
//! każde słuchawki taktuje ich własny zegar — dryf wyrównuje adaptacyjny resampler silnika.
//!
//! `node.dont-reconnect` sprawia też, że WirePlumber nie przeniesie strumienia rozłączonych
//! słuchawek na inne (domyślne) wyjście.

use std::thread::JoinHandle;

use anyhow::{Context as _, Result, anyhow};
use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamRc, StreamState};

pub struct PwOutput {
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PwOutput {
    /// `target` — `node.name` wyjścia. `fill` dostaje bufor przeplecionych próbek f32
    /// (`channels` kanałów, `rate` Hz) do wypełnienia; `on_error` — strumień przestał działać.
    pub fn open<F, E>(target: &str, description: &str, rate: u32, channels: u32, fill: F, on_error: E) -> Result<Self>
    where
        F: FnMut(&mut [f32]) + Send + 'static,
        E: Fn() + Send + 'static,
    {
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();
        let target = target.to_string();
        let description = description.to_string();
        let thread = std::thread::Builder::new()
            .name("soundport-out".into())
            .spawn(move || {
                let result = run(&target, &description, rate, channels, fill, on_error, quit_rx, &init_tx);
                if let Err(e) = result {
                    let _ = init_tx.send(Err(e));
                }
            })
            .context("wątek strumienia")?;
        match init_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { quit: Some(quit_tx), thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("PipeWire nie odpowiada")),
        }
    }
}

impl Drop for PwOutput {
    fn drop(&mut self) {
        if let Some(tx) = self.quit.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct UserData<F, E> {
    fill: F,
    on_error: E,
    channels: usize,
    failed: bool,
}

#[allow(clippy::too_many_arguments)]
fn run<F, E>(
    target: &str,
    description: &str,
    rate: u32,
    channels: u32,
    fill: F,
    on_error: E,
    quit_rx: pw::channel::Receiver<()>,
    init_tx: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()>
where
    F: FnMut(&mut [f32]) + Send + 'static,
    E: Fn() + Send + 'static,
{
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let props = pw::properties::properties! {
        "media.type" => "Audio",
        "media.category" => "Playback",
        "media.role" => "Music",
        "application.name" => "SoundPort",
        "application.icon-name" => "audio-headphones",
        "node.name" => "soundport-output",
        "node.description" => format!("SoundPort → {description}"),
        "target.object" => target,
        "node.dont-reconnect" => "true",
    };
    let stream = StreamRc::new(core.clone(), "soundport-output", props)?;

    let data = UserData { fill, on_error, channels: channels as usize, failed: false };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_stream, data, _old, new| {
            let fatal = matches!(new, StreamState::Error(_) | StreamState::Unconnected);
            if fatal && !data.failed {
                data.failed = true;
                (data.on_error)();
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(buf) = datas.first_mut() else { return };
            let stride = std::mem::size_of::<f32>() * data.channels;
            let capacity = buf.as_raw().maxsize as usize / stride;
            let frames = if requested == 0 { capacity } else { requested.min(capacity) };
            let Some(bytes) = buf.data() else { return };
            let bytes = &mut bytes[..frames * stride];
            // Bufor PipeWire jest wyrównany do 4 bajtów, więc można go traktować jako f32.
            let (prefix, samples, _) = unsafe { bytes.align_to_mut::<f32>() };
            if prefix.is_empty() {
                (data.fill)(samples);
            } else {
                bytes.fill(0);
            }
            let chunk = buf.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .register()?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(channels);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow!("format: {e:?}"))?
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("format"))?];
    stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = init_tx.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}
