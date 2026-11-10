//! Linux: plik `.desktop` i ikona w `~/.local/share`.
//!
//! Na Wayland kompozytor (KWin) nie używa ikony ustawionej przez okno, tylko szuka pliku
//! `<app_id>.desktop` i bierze z niego ikonę — bez tego pasek zadań pokazuje ikonę domyślną.
//! Przy okazji SoundPort pojawia się w menu aplikacji.

use std::path::Path;

use crate::icon::{self, IconState};

/// `app_id` okna — musi się zgadzać z nazwą pliku `.desktop`.
pub const APP_ID: &str = "soundport";
const ICON_SIZE: u32 = 128;

/// Zapisuje (lub aktualizuje) ikonę i plik `.desktop`. Błędy tylko logujemy.
pub fn install() {
    if let Err(e) = try_install() {
        crate::log(&format!("Nie udało się zapisać pliku .desktop: {e:#}"));
    }
}

fn try_install() -> anyhow::Result<()> {
    let data = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("brak katalogu danych"))?;
    let exe = std::env::current_exe()?;

    let icon_path = data
        .join("icons/hicolor")
        .join(format!("{ICON_SIZE}x{ICON_SIZE}/apps/{APP_ID}.png"));
    write_if_changed(&icon_path, &icon_png()?)?;

    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=SoundPort\n\
         Comment=Dźwięk na kilka par słuchawek naraz\n\
         Exec=\"{exe}\" --show\n\
         Icon={icon}\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;Utility;\n\
         StartupWMClass={APP_ID}\n",
        exe = exe.display(),
        icon = icon_path.display(),
    );
    write_if_changed(&data.join(format!("applications/{APP_ID}.desktop")), desktop.as_bytes())
}

fn icon_png() -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, ICON_SIZE, ICON_SIZE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&icon::rgba_sized(IconState::Active, ICON_SIZE))?;
    writer.finish()?;
    Ok(out)
}

fn write_if_changed(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}
