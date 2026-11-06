//! Adaptacyjny resampler po stronie wyjścia.
//!
//! Każde urządzenie wyjściowe (np. słuchawki BT) ma własny zegar, który minimalnie
//! różni się od zegara źródła. Bez korekty bufor powoli by się przepełniał albo
//! opróżniał (trzaski / rosnące opóźnienie). Resampler:
//! - konwertuje częstotliwość próbkowania i liczbę kanałów źródła na format wyjścia,
//! - utrzymuje zadany poziom bufora (opóźnienie), płynnie zmieniając współczynnik
//!   resamplingu o ułamek procenta (niesłyszalne),
//! - przy dużej rozbieżności (np. zmiana opóźnienia przez użytkownika) robi twardy resync.

use std::collections::VecDeque;

/// Maksymalna korekta tempa (0.3%) — niesłyszalna zmiana wysokości dźwięku.
const MAX_CORRECTION: f64 = 0.003;
/// Wzmocnienie regulatora: korekta = błąd[s] * GAIN.
const CORRECTION_GAIN: f64 = 0.1;
/// Wygładzanie pomiaru zapełnienia bufora (EMA na każde wywołanie callbacku).
const FILL_SMOOTHING: f64 = 0.02;
/// Nadmiar bufora ponad cel, po którym robimy twardy resync.
const RESYNC_EXCESS_SECONDS: f64 = 0.25;
/// Zmiana celu większa niż ta powoduje natychmiastowe przestawienie (zamiast powolnej korekty).
const RETARGET_THRESHOLD_SECONDS: f64 = 0.02;

pub struct Resampler {
    src_channels: usize,
    dst_channels: usize,
    src_rate: f64,
    base_step: f64,
    /// Próbki źródła (przeplatane), jeszcze nieodtworzone.
    buffer: VecDeque<f32>,
    /// Mapowanie kanał wyjścia -> kanał źródła (None = cisza).
    channel_map: Vec<Option<usize>>,
    downmix_to_mono: bool,
    /// Pozycja odczytu (w ramkach źródła, z częścią ułamkową) względem początku bufora.
    pos: f64,
    fill_avg: f64,
    priming: bool,
    target_frames: usize,
}

impl Resampler {
    pub fn new(src_rate: u32, src_channels: u16, dst_rate: u32, dst_channels: u16) -> Self {
        let src_channels = src_channels.max(1) as usize;
        let dst_channels = dst_channels.max(1) as usize;
        let channel_map = (0..dst_channels)
            .map(|d| {
                if src_channels == 1 {
                    Some(0) // mono -> wszystkie kanały
                } else if d < src_channels {
                    Some(d) // L->L, R->R, ...
                } else {
                    None // np. stereo -> 5.1: tylne kanały cicho
                }
            })
            .collect();
        Self {
            src_channels,
            dst_channels,
            src_rate: src_rate as f64,
            base_step: src_rate as f64 / dst_rate as f64,
            // 4 s zapasu — bez alokacji w wątku audio w normalnej pracy.
            buffer: VecDeque::with_capacity(src_rate as usize * src_channels * 4),
            channel_map,
            downmix_to_mono: dst_channels == 1 && src_channels > 1,
            pos: 0.0,
            fill_avg: 0.0,
            priming: true,
            target_frames: 0,
        }
    }

    /// Ile ramek źródła czeka w buforze.
    pub fn buffered_frames(&self) -> usize {
        self.buffer.len() / self.src_channels
    }

    pub fn buffered_ms(&self) -> u32 {
        (self.buffered_frames() as f64 * 1000.0 / self.src_rate) as u32
    }

    /// Dokłada przeplatane próbki źródła.
    pub fn push(&mut self, samples: &[f32]) {
        self.buffer.extend(samples.iter().copied());
    }

    /// Wypełnia `out` (przeplatane próbki wyjścia).
    pub fn process(&mut self, out: &mut [f32], volume: f32, latency_ms: u32) {
        let src_ch = self.src_channels;
        let dst_ch = self.dst_channels;
        let target = ((latency_ms as f64) * self.src_rate / 1000.0).round() as usize;
        let target = target.max(1);
        self.retarget(target);

        let fill = self.buffered_frames();
        if self.priming {
            if fill < target {
                out.fill(0.0);
                return;
            }
            self.priming = false;
            self.pos = 0.0;
            self.fill_avg = fill as f64;
        }

        let resync_limit = target + (RESYNC_EXCESS_SECONDS * self.src_rate) as usize;
        if fill > resync_limit {
            self.drop_frames(fill - target);
            self.fill_avg = target as f64;
            self.pos = 0.0;
        }

        let fill = self.buffered_frames();
        self.fill_avg += (fill as f64 - self.fill_avg) * FILL_SMOOTHING;
        let error_seconds = (self.fill_avg - target as f64) / self.src_rate;
        // Za dużo w buforze -> czytamy minimalnie szybciej, za mało -> wolniej.
        let correction = (error_seconds * CORRECTION_GAIN).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        let step = self.base_step * (1.0 + correction);

        let out_frames = out.len() / dst_ch;
        let mut written = 0usize;
        for frame in 0..out_frames {
            let i = self.pos as usize;
            if i + 1 >= fill {
                // Niedobór danych: reszta ciszą, ponowne napełnienie bufora do celu.
                self.priming = true;
                break;
            }
            let frac = (self.pos - i as f64) as f32;
            let a = i * src_ch;
            let b = a + src_ch;
            let o = frame * dst_ch;
            if self.downmix_to_mono {
                let mut sum = 0.0;
                for c in 0..src_ch {
                    let (sa, sb) = (self.buffer[a + c], self.buffer[b + c]);
                    sum += sa + (sb - sa) * frac;
                }
                out[o] = sum / src_ch as f32 * volume;
            } else {
                for (d, map) in self.channel_map.iter().enumerate() {
                    out[o + d] = match *map {
                        Some(c) => {
                            let (sa, sb) = (self.buffer[a + c], self.buffer[b + c]);
                            (sa + (sb - sa) * frac) * volume
                        }
                        None => 0.0,
                    };
                }
            }
            self.pos += step;
            written = frame + 1;
        }
        out[written * dst_ch..].fill(0.0);

        let consumed = (self.pos as usize).min(self.buffered_frames());
        self.drop_frames(consumed);
        self.pos -= consumed as f64;
        if self.priming {
            self.pos = 0.0;
        }
    }

    /// Natychmiastowa reakcja na dużą zmianę opóźnienia (zmiana ustawień przez użytkownika).
    fn retarget(&mut self, target: usize) {
        if self.target_frames == target {
            return;
        }
        let previous = self.target_frames;
        self.target_frames = target;
        let threshold = (RETARGET_THRESHOLD_SECONDS * self.src_rate) as usize;
        if previous == 0 || target.abs_diff(previous) <= threshold {
            return;
        }
        let fill = self.buffered_frames();
        if fill > target {
            self.drop_frames(fill - target);
            self.pos = 0.0;
        } else {
            self.priming = true;
        }
        self.fill_avg = self.buffered_frames() as f64;
    }

    fn drop_frames(&mut self, frames: usize) {
        let samples = (frames * self.src_channels).min(self.buffer.len());
        self.buffer.drain(..samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, channels: u16, frames: usize, start: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * channels as usize);
        for f in start..start + frames {
            let s = (f as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin();
            for _ in 0..channels {
                v.push(s);
            }
        }
        v
    }

    #[test]
    fn outputs_silence_until_primed() {
        let mut r = Resampler::new(48_000, 2, 48_000, 2);
        r.push(&sine(48_000, 2, 480, 0)); // 10 ms
        let mut out = vec![1.0; 960];
        r.process(&mut out, 1.0, 50);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn passes_audio_at_equal_rates() {
        let mut r = Resampler::new(48_000, 2, 48_000, 2);
        r.push(&sine(48_000, 2, 4_800, 0)); // 100 ms
        let mut out = vec![0.0; 960];
        r.process(&mut out, 1.0, 50);
        assert!(out.iter().any(|&s| s.abs() > 0.5));
    }

    #[test]
    fn converts_mono_to_stereo_and_applies_volume() {
        let mut r = Resampler::new(48_000, 1, 48_000, 2);
        r.push(&vec![1.0; 4_800]);
        let mut out = vec![0.0; 200];
        r.process(&mut out, 0.5, 20);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }

    /// Symulacja dryfu zegarów: źródło 48 kHz, wyjście deklaruje 44.1 kHz,
    /// ale w praktyce pobiera 0.05% szybciej. Bufor ma zostać przy celu.
    #[test]
    fn tracks_target_latency_under_clock_drift() {
        let src_rate = 48_000;
        let mut r = Resampler::new(src_rate, 2, 44_100, 2);
        let latency_ms = 80;
        let mut produced = 0usize;
        let mut consumed_out_frames = 0f64;
        let out_rate = 44_100.0 * 1.0005;
        let mut out = vec![0.0; 441 * 2];
        for tick in 0..6_000 {
            // 10 ms źródła na tick
            r.push(&sine(src_rate, 2, 480, produced));
            produced += 480;
            // wyjście pobiera wg własnego (szybszego) zegara
            consumed_out_frames += out_rate / 100.0;
            while consumed_out_frames >= 441.0 {
                r.process(&mut out, 1.0, latency_ms);
                consumed_out_frames -= 441.0;
            }
            if tick > 3_000 {
                let fill_ms = r.buffered_frames() as f64 * 1000.0 / src_rate as f64;
                assert!(
                    (fill_ms - latency_ms as f64).abs() < 25.0,
                    "bufor odjechał: {fill_ms:.1} ms przy celu {latency_ms} ms (tick {tick})"
                );
            }
        }
    }

    #[test]
    fn large_latency_decrease_resyncs_immediately() {
        let mut r = Resampler::new(48_000, 2, 48_000, 2);
        r.push(&sine(48_000, 2, 48_000, 0)); // 1 s
        let mut out = vec![0.0; 960];
        r.process(&mut out, 1.0, 900);
        r.process(&mut out, 1.0, 100);
        let fill_ms = r.buffered_frames() as f64 * 1000.0 / 48_000.0;
        assert!(fill_ms <= 100.0, "fill {fill_ms}");
    }
}
