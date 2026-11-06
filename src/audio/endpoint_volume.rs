//! Głośność i wyciszenie urządzenia źródłowego ustawione w systemie.
//!
//! WASAPI loopback przechwytuje dźwięk *przed* głośnością urządzenia, więc suwak
//! głośności Windows (lub VB-CABLE) nie wpływałby na słuchawki. Tu odczytujemy
//! głośność źródła i podajemy ją wyjściom jako mnożnik.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

pub struct VolumeWatch {
    gain: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VolumeWatch {
    /// `device_id` w formacie cpal („wasapi:{…}.{…}”).
    pub fn start(device_id: &str) -> Self {
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = platform::spawn(device_id, gain.clone(), stop.clone());
        Self { gain, stop, thread }
    }

    /// Mnożnik 0..=1 aktualizowany na bieżąco (bity f32).
    pub fn gain(&self) -> Arc<AtomicU32> {
        self.gain.clone()
    }
}

impl Drop for VolumeWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Głośność w dB -> mnożnik amplitudy. Suwak na 0 lub wyciszenie = cisza.
fn gain_from(level_db: f32, scalar: f32, muted: bool) -> f32 {
    if muted || scalar <= 0.0 {
        0.0
    } else {
        10f32.powf(level_db / 20.0).clamp(0.0, 1.0)
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::core::HSTRING;

    /// Odczyt jest tani, a 25 ms wystarcza, by suwak działał „na żywo”.
    const POLL: Duration = Duration::from_millis(25);

    pub fn spawn(
        device_id: &str,
        gain: Arc<AtomicU32>,
        stop: Arc<AtomicBool>,
    ) -> Option<JoinHandle<()>> {
        let endpoint = device_id.strip_prefix("wasapi:")?.to_string();
        std::thread::Builder::new()
            .name("soundport-volume".into())
            .spawn(move || {
                // SAFETY: inicjalizacja COM dla tego wątku, sparowana z CoUninitialize.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let result = (|| -> windows::core::Result<()> {
                    let volume: IAudioEndpointVolume = unsafe {
                        let enumerator: IMMDeviceEnumerator =
                            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                        enumerator
                            .GetDevice(&HSTRING::from(endpoint))?
                            .Activate(CLSCTX_ALL, None)?
                    };
                    while !stop.load(Ordering::Relaxed) {
                        // SAFETY: zwykłe odczyty z interfejsu COM utworzonego w tym wątku.
                        let (db, scalar, muted) = unsafe {
                            (
                                volume.GetMasterVolumeLevel()?,
                                volume.GetMasterVolumeLevelScalar()?,
                                volume.GetMute()?.as_bool(),
                            )
                        };
                        gain.store(super::gain_from(db, scalar, muted).to_bits(), Ordering::Relaxed);
                        std::thread::sleep(POLL);
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    // Urządzenie zniknęło lub nie ma regulacji głośności — gramy z pełną głośnością.
                    gain.store(1.0f32.to_bits(), Ordering::Relaxed);
                    crate::log(&format!("Nie można odczytać głośności źródła: {e}"));
                }
                if com.is_ok() {
                    unsafe { CoUninitialize() };
                }
            })
            .ok()
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::thread::JoinHandle;

    // TODO(Linux): monitor sinka PipeWire też omija głośność sinka — odczytać ją przez pipewire.
    pub fn spawn(_: &str, _: Arc<AtomicU32>, _: Arc<AtomicBool>) -> Option<JoinHandle<()>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::gain_from;

    #[test]
    fn maps_volume_to_gain() {
        assert_eq!(gain_from(0.0, 1.0, false), 1.0);
        assert!((gain_from(-6.0, 0.7, false) - 0.501).abs() < 0.01);
        assert_eq!(gain_from(-3.0, 0.9, true), 0.0);
        assert_eq!(gain_from(-65.0, 0.0, false), 0.0);
    }
}
