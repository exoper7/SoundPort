# SoundPort

Aplikacja w zasobniku systemowym, która wysyła dźwięk systemu **jednocześnie na wiele wyjść audio**, np. na kilka par słuchawek Bluetooth. Przydaje się do wspólnego oglądania filmu na dwie lub więcej par słuchawek bez rozdzielaczy i kabli.

- **Windows 10/11**: działa (WASAPI).
- **Linux (Kubuntu/KDE)**: planowany, patrz [plan](#linux-kubuntu--plan).

Aktualna wersja: **1.0.1**. Historia zmian jest w [CHANGELOG.md](CHANGELOG.md).

## Funkcje

- Dowolna liczba wyjść naraz: słuchawki BT, przewodowe, głośniki, monitory HDMI/DP.
- Osobna głośność i osobne opóźnienie dla każdego wyjścia, żeby wyrównać słuchawki o różnych opóźnieniach.
- Kompensacja dryfu zegarów urządzeń: opóźnienie nie rośnie z czasem i nie słychać trzasków.
- Suwak głośności i wyciszenie w Windows działają na wszystkich słuchawkach.
- Rozłączone słuchawki wracają same po ponownym połączeniu.
- Ustawienia i stan (włączony/wyłączony) są pamiętane, opcjonalny autostart z Windows.
- Jeden plik `.exe` (~6 MB), bez instalatora i bez dodatkowych bibliotek.

## Szybki start (Windows)

1. Pobierz `soundport.exe` z zakładki *Releases* albo zbuduj go samodzielnie (patrz [Budowanie](#budowanie)).
2. **Zalecane:** zainstaluj darmowy wirtualny kabel audio [VB-CABLE](https://vb-audio.com/Cable/) i ustaw *CABLE Input* jako domyślne urządzenie odtwarzania w Windows.
3. Uruchom `soundport.exe`. Przy pierwszym uruchomieniu otworzy się okno ustawień.
4. Jako **źródło** wybierz *CABLE Input* (albo „Domyślne urządzenie systemowe”), zaznacz wszystkie słuchawki jako **wyjścia** i kliknij **Uruchom**.
5. Jeśli któreś słuchawki grają wcześniej od pozostałych, zwiększ im **Opóźnienie** (patrz [Synchronizacja](#synchronizacja-słuchawek)).

## Użycie

- **Prawy klik na ikonie**: start/stop, wybór źródła, zaznaczanie wyjść, autostart, zakończenie.
- **Lewy klik na ikonie**: okno z głośnością i opóźnieniem dla każdego wyjścia.
- Zamknięcie okna chowa je do zasobnika. Program kończy się przez *Zakończ* w menu ikony.

### Synchronizacja słuchawek

Słuchawki Bluetooth opóźniają dźwięk o około 150–250 ms (zależnie od kodeka), a przewodowe praktycznie wcale. Różnicę wyrównuje się w kolumnie **Opóźnienie**: słuchawki, które grają *wcześniej*, opóźnij o odpowiednią liczbę ms. Na przykład przy parze przewodowe + BT typowa wartość dla przewodowych to 150–200 ms.

Najłatwiej ustawić to na metronomie albo nagraniu z wyraźnymi kliknięciami: załóż po jednej słuchawce z każdej pary i zmieniaj opóźnienie co 5–10 ms, aż kliknięcia zleją się w jedno. Po ponownym połączeniu słuchawek BT ich opóźnienie może się zmienić o 10–30 ms.

Urządzenie źródłowe gra bez opóźnienia SoundPortu, a wyjścia są za nim o wartość „bufora bazowego”. Dlatego są dwa tryby pracy:

| Tryb | Konfiguracja | Synchronizacja |
|---|---|---|
| Prosty | Źródło = słuchawki A (domyślne w Windows), wyjścia = B, C… | A gra najwcześniej, B/C są opóźnione o bufor i różnicę BT |
| **Zalecany** | Źródło = VB-CABLE (domyślne w Windows), wyjścia = **wszystkie** słuchawki | Wszystkie słuchawki idą tą samą ścieżką, więc łatwo je wyrównać |

Bez instalowania VB-CABLE rolę wirtualnego kabla może pełnić nieużywane wyjście, np. audio monitora HDMI/DP bez głośników.

### Ograniczenia Bluetooth

- Jeden adapter BT ma ograniczone pasmo. Przy 3 lub więcej parach słuchawek mogą pojawić się przerwy. Pomaga drugi adapter (dongle USB).
- Nie zaznaczaj urządzeń „Zestaw słuchawkowy … Hands-Free”. To tryb telefoniczny z dużo gorszą jakością dźwięku.

## Jak to działa

```
 [urządzenie źródłowe] ── loopback ──┬──> kolejka ─> resampler ─> słuchawki #1
  (domyślne lub wybrane)             ├──> kolejka ─> resampler ─> słuchawki #2
                                     └──> kolejka ─> resampler ─> głośnik ...
```

1. SoundPort przechwytuje to, co gra na **urządzeniu źródłowym** (WASAPI loopback).
2. Każde zaznaczone **wyjście** dostaje własną kolejkę bez blokad i własny strumień audio, więc wolne urządzenie nie spowalnia pozostałych.
3. **Adaptacyjny resampler** na każdym wyjściu konwertuje częstotliwość i liczbę kanałów. Kompensuje też dryf zegarów urządzeń, płynnie korygując tempo o maks. 0,3%, co jest niesłyszalne.
4. Na źródle gra ciągła cisza („keep-alive”). WASAPI loopback nie dostarcza danych, gdy nic nie gra, a bez tego synchronizacja rozjeżdżałaby się przy każdej pauzie.
5. WASAPI loopback przechwytuje dźwięk przed regulacją głośności urządzenia. SoundPort odczytuje więc głośność i wyciszenie źródła z Windows i stosuje je na wszystkich wyjściach.
6. Lista urządzeń jest sprawdzana co 2 s, więc ponownie połączone słuchawki wracają same.

## Budowanie

Wymagania: [Rust](https://rustup.rs) (stable, edycja 2024). Na Windows potrzebne są też narzędzia MSVC (Visual Studio Build Tools z „C++ build tools”).

```bash
cargo build --release          # target/release/soundport.exe
cargo test                     # testy (resampler, w tym symulacja dryfu zegarów)
cargo run -- --show            # uruchom z otwartym oknem
cargo run -- --version         # wypisz wersję
cargo run -- --list-devices    # lista wyjść audio i ich identyfikatorów
cargo run -- --selftest        # 5 s: domyślne urządzenie -> wszystkie pozostałe, z poziomem buforów
```

Ustawienia i log: `%APPDATA%\SoundPort\` (Linux: `~/.config/SoundPort/`).

### Struktura

| Plik | Zawartość |
|---|---|
| `src/main.rs` | start, wersja, jedna instancja, flagi diagnostyczne |
| `src/app.rs` | ikona w zasobniku, menu, okno ustawień (egui) |
| `src/audio/mod.rs` | silnik: źródło loopback, wyjścia, ponowne łączenie |
| `src/audio/endpoint_volume.rs` | odczyt głośności i wyciszenia źródła z Windows |
| `src/audio/resampler.rs` | adaptacyjny resampler z regulatorem bufora (+ testy) |
| `src/settings.rs` | ustawienia JSON |
| `src/icon.rs` | ikona rysowana w kodzie |

## Linux (Kubuntu) — plan

Zależności są już ustawione w `Cargo.toml` dla `target_os = "linux"`: `tray-icon` z backendem `ksni` (StatusNotifierItem, natywny w Plasmie, bez GTK) oraz `cpal` z backendem `pipewire`. Do zrobienia:

1. Przechwytywanie źródła: na PipeWire zamiast WASAPI loopback trzeba nagrywać z **monitora** sinka (`<sink>.monitor`) i odczytywać jego głośność.
2. Alternatywa bez własnego miksowania: PipeWire potrafi to natywnie (`module-combine-stream`). Backend Linux mógłby tylko zarządzać takim combine-sinkiem.
3. Pakiety do budowania: `sudo apt install build-essential pkg-config libpipewire-0.3-dev libclang-dev libxkbcommon-dev libgl1-mesa-dev`.

## Współtworzenie

Zgłoszenia błędów i pull requesty są mile widziane. Przed wysłaniem zmian:

1. Uruchom `cargo test` i `cargo clippy`.
2. Podnieś wersję w `Cargo.toml` zgodnie z [Semantic Versioning](https://semver.org/lang/pl/) i dopisz zmianę do [CHANGELOG.md](CHANGELOG.md).

## Licencja

Copyright © 2026 Grzegorz Adamski

SoundPort jest wolnym oprogramowaniem: możesz go rozpowszechniać i modyfikować na warunkach [GNU General Public License](LICENSE) w wersji 3 lub (według uznania) dowolnej późniejszej, opublikowanej przez Free Software Foundation.

Program jest rozpowszechniany bez żadnej gwarancji. Szczegóły w pliku [LICENSE](LICENSE).
