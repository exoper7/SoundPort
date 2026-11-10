# Historia zmian

Format według [Keep a Changelog](https://keepachangelog.com/pl/1.1.0/), numeracja według [Semantic Versioning](https://semver.org/lang/pl/):

- **PATCH** (1.0.**x**): poprawki błędów i drobne zmiany,
- **MINOR** (1.**x**.0): nowe funkcje zgodne wstecz,
- **MAJOR** (**x**.0.0): zmiany niezgodne wstecz, np. inny format ustawień.

Każda zmiana podnosi wersję w `Cargo.toml` i dostaje wpis poniżej.

## [1.1.0] — 2026-11-10

### Dodane
- Wersja dla Linuksa (PipeWire, testowana na Ubuntu 26.04 z KDE Plasma i dwiema parami słuchawek Bluetooth).
  - SoundPort tworzy na czas działania wirtualne wyjście „SoundPort” (wbudowany odpowiednik VB-CABLE), ustawia je jako domyślne w systemie i po zatrzymaniu przywraca poprzednie.
  - Głośność i wyciszenie wirtualnego wyjścia ustawione w systemie działają na wszystkich słuchawkach.
  - Lista wyjść jest aktualna na bieżąco i nie zawiera wewnętrznych węzłów Bluetooth ani strumieni aplikacji.
  - Ochrona przed pętlą sprzężenia: strumień rozłączonych słuchawek nie zostanie przeniesiony na źródło.
  - Na czas działania słuchawki-wyjścia mają głośność 100%, a głośnością steruje wirtualne wyjście SoundPort. Wirtualne wyjście startuje z głośnością poprzedniego domyślnego urządzenia, a po zatrzymaniu słuchawki wracają do swojej głośności, jeśli nikt jej nie zmienił.
  - Strumień, którego WirePlumber nie podłączył (albo który stracił połączenie przy chwilowym zerwaniu Bluetooth), jest po 3 s otwierany ponownie.
  - Każde wyjście ma własny strumień PipeWire taktowany zegarem swoich słuchawek (strumienie cpal były spięte w jedną grupę z jednym zegarem, przez co drugie słuchawki Bluetooth potrafiły zamilknąć). WirePlumber nie przenosi strumienia rozłączonych słuchawek na inne wyjście.
  - Przy zatrzymaniu wirtualne wyjście znika dopiero po przeniesieniu odtwarzaczy na poprzednie wyjście, więc przeglądarki nie wstrzymują odtwarzania. Słuchawki przy uruchamianiu i zatrzymywaniu nie grają na chwilę na pełnej głośności.
  - Zamknięcie okna krzyżykiem zamyka okno, a program działa dalej w zasobniku (na Wayland okna nie da się ukryć).
  - Plik `.desktop` i ikona w `~/.local/share`, dzięki czemu pasek zadań na Wayland pokazuje ikonę SoundPort.
  - Ikona w zasobniku przez StatusNotifierItem (natywnie w Plasmie).
- `--selftest <tekst>` ogranicza test do wyjść, których nazwa lub identyfikator zawiera podany tekst.

## [1.0.1] — 2026-11-06

### Naprawione
- Głośność i wyciszenie urządzenia źródłowego ustawione w Windows (np. VB-CABLE) działają teraz na wszystkich wyjściach. Wcześniej WASAPI loopback przechwytywał dźwięk sprzed regulacji głośności, więc suwak nie zmieniał głośności słuchawek.

### Dodane
- Numer wersji w tytule okna, w oknie ustawień, w podpowiedzi ikony i w logu.
- Flaga `--version`.
- Licencja GPL-3.0-or-later.

## [1.0.0] — 2026-11-06

Pierwsza wersja.

- Ikona w zasobniku z menu: start/stop, źródło, wyjścia, autostart.
- Rozsyłanie dźwięku z urządzenia źródłowego (WASAPI loopback) na wiele wyjść jednocześnie.
- Adaptacyjny resampler na każdym wyjściu: konwersja formatu i kompensacja dryfu zegarów.
- Głośność i opóźnienie osobno dla każdego wyjścia.
- Automatyczne ponowne łączenie słuchawek, zapamiętywanie ustawień i stanu.
