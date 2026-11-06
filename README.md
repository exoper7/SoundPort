# SoundPort

Aplikacja działająca w zasobniku systemowym, która wysyła dźwięk systemu jednocześnie na wiele wyjść audio, np. kilka par słuchawek Bluetooth.

- **Windows**: działa (WASAPI).
- **Linux (Kubuntu/KDE)**: planowany. Kod już się do tego przygotowuje: tray przez KSNI (natywny dla KDE), dźwięk przez PipeWire.

## Jak to działa

```
 [urządzenie źródłowe] ── loopback ──┬──> kolejka ─> resampler ─> słuchawki #1
  (domyślne lub wybrane)             ├──> kolejka ─> resampler ─> słuchawki #2
                                     └──> kolejka ─> resampler ─> głośnik ...
```

1. SoundPort przechwytuje to, co gra na **urządzeniu źródłowym** (WASAPI loopback).
2. Każde zaznaczone **wyjście** dostaje własną kolejkę bez blokad i własny strumień audio.
3. **Adaptacyjny resampler** na każdym wyjściu konwertuje częstotliwość i liczbę kanałów. Kompensuje też dryf zegarów urządzeń: płynnie koryguje tempo o maks. 0,3%, co jest niesłyszalne. Dzięki temu opóźnienie nie rośnie z czasem i nie słychać trzasków.
4. Na źródle gra ciągła cisza („keep-alive”). WASAPI loopback nie dostarcza danych, gdy nic nie gra, a bez tego synchronizacja rozjeżdżałaby się przy każdej pauzie.
5. WASAPI loopback przechwytuje dźwięk przed regulacją głośności urządzenia. SoundPort odczytuje więc głośność i wyciszenie źródła ustawione w Windows i stosuje je na wszystkich wyjściach. Suwak głośności w zasobniku Windows działa wtedy tak, jak się tego oczekuje. Każde wyjście ma dodatkowo własną głośność w oknie SoundPort.
6. Po rozłączeniu słuchawek wyjście jest usuwane. Po ponownym połączeniu wraca samo, bo lista urządzeń jest sprawdzana co 2 s.

## Użycie

- **Prawy klik na ikonie**: start/stop, wybór źródła, zaznaczanie wyjść, autostart, zakończenie.
- **Lewy klik na ikonie**: okno z głośnością i opóźnieniem dla każdego wyjścia.
- Zamknięcie okna chowa je do zasobnika. Stan (włączony/wyłączony) jest pamiętany między uruchomieniami.

### Synchronizacja słuchawek

Każda para słuchawek BT ma inne opóźnienie (typowo 100–300 ms). Do wyrównania służy kolumna **Opóźnienie**: słuchawki, które grają *wcześniej*, opóźnij o kilkadziesiąt ms.

Urządzenie źródłowe gra bez opóźnienia SoundPortu, więc pozostałe wyjścia będą za nim o wartość „bufora bazowego”. Są dwa tryby pracy:

| Tryb | Konfiguracja | Synchronizacja |
|---|---|---|
| Prosty | Źródło = słuchawki A (domyślne w Windows), wyjścia = B, C… | A gra najwcześniej, B/C są opóźnione o bufor + różnicę BT |
| **Zalecany** | Zainstaluj wirtualny kabel (np. [VB-CABLE](https://vb-audio.com/Cable/)), ustaw go jako domyślne urządzenie Windows, a w SoundPort wybierz go jako źródło i zaznacz **wszystkie** słuchawki | Wszystkie słuchawki idą tą samą ścieżką, więc łatwo je wyrównać |

### Ograniczenia Bluetooth

- Jeden adapter BT ma ograniczone pasmo. Przy 3 lub więcej parach słuchawek A2DP mogą pojawić się przerwy. Pomaga drugi adapter (dongle USB) albo słuchawki z kodekiem SBC zamiast AAC/aptX.
- Gdy słuchawki przełączą się w tryb zestawu słuchawkowego (mikrofon, profil HFP), jakość spada do „telefonicznej”. W ustawieniach dźwięku Windows wyłącz dla nich urządzenie „Hands-Free”.

## Budowanie

Wymagania: [Rust](https://rustup.rs) (stable). Na Windows potrzebne są też narzędzia MSVC (Visual Studio Build Tools z „C++ build tools”).

```bash
cargo build --release          # target/release/soundport.exe (~6 MB, bez zależności)
cargo test                     # testy resamplera (w tym symulacja dryfu zegarów)
cargo run -- --show            # uruchom z otwartym oknem
cargo run -- --list-devices    # lista wyjść audio i ich identyfikatorów
cargo run -- --selftest        # 5 s: domyślne urządzenie -> wszystkie pozostałe, z poziomem buforów
```

Ustawienia i log: `%APPDATA%\SoundPort\` (Linux: `~/.config/SoundPort/`).

## Struktura

| Plik | Zawartość |
|---|---|
| `src/main.rs` | start, jedna instancja, flagi diagnostyczne |
| `src/app.rs` | ikona w zasobniku, menu, okno ustawień (egui) |
| `src/audio/mod.rs` | silnik: źródło loopback, wyjścia, ponowne łączenie |
| `src/audio/endpoint_volume.rs` | odczyt głośności i wyciszenia źródła z Windows |
| `src/audio/resampler.rs` | adaptacyjny resampler z regulatorem bufora (+ testy) |
| `src/settings.rs` | ustawienia JSON |
| `src/icon.rs` | ikona rysowana w kodzie |

## Linux (Kubuntu) — plan

Zależności są już ustawione w `Cargo.toml` dla `target_os = "linux"`: `tray-icon` z backendem `ksni` (StatusNotifierItem, natywny w Plasmie, bez GTK) oraz `cpal` z backendem `pipewire`. Do zrobienia:

1. Przechwytywanie źródła: na PipeWire zamiast WASAPI loopback trzeba nagrywać z **monitora** sinka (`<sink>.monitor`). To jedyna część silnika zależna od platformy.
2. Alternatywa bez własnego miksowania: PipeWire potrafi to natywnie (`module-combine-stream`). Backend Linux mógłby tylko zarządzać takim combine-sinkiem przez `pw-cli`/`pactl`.
3. Pakiety budowania: `sudo apt install build-essential pkg-config libpipewire-0.3-dev libclang-dev libxkbcommon-dev libgl1-mesa-dev`.
