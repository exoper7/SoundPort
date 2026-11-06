//! Ustawienia zapisywane w %APPDATA%\SoundPort\settings.json (Windows)
//! lub ~/.config/SoundPort/settings.json (Linux).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audio::{OutputRoute, RoutingConfig};

pub const MIN_BASE_LATENCY_MS: u32 = 20;
pub const MAX_BASE_LATENCY_MS: u32 = 500;
pub const MAX_DELAY_MS: u32 = 2000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputSettings {
    /// Ostatnio widziana nazwa — żeby pokazać zapamiętane, ale rozłączone słuchawki.
    pub name: String,
    pub enabled: bool,
    pub volume: f32,
    pub delay_ms: u32,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: false,
            volume: 1.0,
            delay_ms: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// None = domyślne urządzenie systemowe.
    pub source_device: Option<String>,
    pub base_latency_ms: u32,
    /// Czy routing był włączony przy zamknięciu — wznawiany po starcie.
    pub was_running: bool,
    pub outputs: BTreeMap<String, OutputSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source_device: None,
            base_latency_ms: 80,
            was_running: false,
            outputs: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("SoundPort")
    }

    fn path() -> PathBuf {
        Self::dir().join("settings.json")
    }

    pub fn exists() -> bool {
        Self::path().exists()
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                crate::log(&format!("Uszkodzony plik ustawień, używam domyślnych: {e}"));
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(Self::dir())?;
            let tmp = Self::path().with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
            std::fs::rename(&tmp, Self::path())?;
            Ok(())
        })();
        if let Err(e) = result {
            crate::log(&format!("Nie udało się zapisać ustawień: {e:#}"));
        }
    }

    pub fn output_mut(&mut self, id: &str, name: &str) -> &mut OutputSettings {
        let out = self.outputs.entry(id.to_string()).or_default();
        if !name.is_empty() {
            out.name = name.to_string();
        }
        out
    }

    pub fn routing(&self) -> RoutingConfig {
        RoutingConfig {
            source: self.source_device.clone(),
            base_latency_ms: self
                .base_latency_ms
                .clamp(MIN_BASE_LATENCY_MS, MAX_BASE_LATENCY_MS),
            outputs: self
                .outputs
                .iter()
                .filter(|(_, o)| o.enabled)
                .map(|(id, o)| OutputRoute {
                    device_id: id.clone(),
                    volume: o.volume.clamp(0.0, 1.0),
                    delay_ms: o.delay_ms.min(MAX_DELAY_MS),
                })
                .collect(),
        }
    }
}
