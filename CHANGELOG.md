# Historia zmian

Format według [Keep a Changelog](https://keepachangelog.com/pl/1.1.0/), numeracja według [Semantic Versioning](https://semver.org/lang/pl/):

- **PATCH** (1.0.**x**): poprawki błędów i drobne zmiany,
- **MINOR** (1.**x**.0): nowe funkcje zgodne wstecz,
- **MAJOR** (**x**.0.0): zmiany niezgodne wstecz, np. inny format ustawień.

Każda zmiana podnosi wersję w `Cargo.toml` i dostaje wpis poniżej.

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
