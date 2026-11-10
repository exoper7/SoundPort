//! Linux: głośność słuchawek na czas działania SoundPort.
//!
//! Dźwięk idzie przez wirtualne wyjście SoundPort (jego głośność to suwak w Plasmie), a potem
//! przez głośność samych słuchawek. Gdyby słuchawki były przyciszone przed startem, suwak
//! SoundPort nie mógłby ich podgłośnić ponad ten poziom. Dlatego na czas działania ustawiamy
//! słuchawki-wyjścia na 100% i po zatrzymaniu przywracamy poprzednią głośność — chyba że
//! w międzyczasie ktoś ją zmienił.
//!
//! Używamy `wpctl` (część WirePlumbera, zawsze obecny razem z nim).

use std::collections::HashMap;
use std::process::Command;

/// Głośność uznawana za „pełną” (wpctl zaokrągla do dwóch miejsc).
const FULL: f32 = 0.995;

#[derive(Default)]
pub struct SinkVolumes {
    /// `node.name` wyjścia → głośność sprzed podgłośnienia.
    saved: HashMap<String, f32>,
}

impl SinkVolumes {
    /// Zapamiętuje głośność wyjścia (tylko za pierwszym razem) i ustawia ją na 100%.
    pub fn raise(&mut self, name: &str, volume_id: u32) {
        let Some(current) = get(volume_id) else { return };
        self.saved.entry(name.to_string()).or_insert(current);
        if current < FULL {
            set(volume_id, 1.0);
        }
    }

    /// Przywraca głośność sprzed `raise`, jeśli nadal jest 100%.
    pub fn restore(&mut self, name: &str, volume_id: Option<u32>) {
        let Some(original) = self.saved.remove(name) else { return };
        let Some(id) = volume_id else { return };
        if get(id).is_some_and(|v| v >= FULL) && original < FULL {
            set(id, original);
        }
    }

    pub fn saved_names(&self) -> Vec<String> {
        self.saved.keys().cloned().collect()
    }

    /// Głośność wyjścia sprzed podgłośnienia albo aktualna.
    pub fn original_or_current(&self, name: &str, volume_id: u32) -> Option<f32> {
        self.saved.get(name).copied().or_else(|| get(volume_id))
    }
}

/// `wpctl get-volume ID` → „Volume: 0.57 [MUTED]”.
pub fn get(id: u32) -> Option<f32> {
    let out = Command::new("wpctl")
        .args(["get-volume", &id.to_string()])
        .output()
        .ok()?;
    parse_volume(&String::from_utf8_lossy(&out.stdout))
}

pub fn set(id: u32, volume: f32) {
    let result = Command::new("wpctl")
        .args(["set-volume", &id.to_string(), &format!("{volume:.3}")])
        .status();
    if !result.is_ok_and(|s| s.success()) {
        crate::log(&format!("Nie udało się ustawić głośności węzła {id} (wpctl)"));
    }
}

fn parse_volume(text: &str) -> Option<f32> {
    text.split_whitespace()
        .skip_while(|w| *w != "Volume:")
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::parse_volume;

    #[test]
    fn parses_wpctl_output() {
        assert_eq!(parse_volume("Volume: 0.57\n"), Some(0.57));
        assert_eq!(parse_volume("Volume: 1.00 [MUTED]\n"), Some(1.0));
        assert_eq!(parse_volume(""), None);
    }
}
