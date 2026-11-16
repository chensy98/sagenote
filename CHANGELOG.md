# Changelog

All notable changes to sagenote are documented here. Versions follow
[Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-11-16

### Added

- IBM Carbon interface: light palette by default, Carbon dark for
  `Dark`/`System`-dark, square corners, 8 px grid, Blue 60 focus and Carbon
  tag colours.
- Embedded IBM Plex Sans Text/Regular/Medium/SemiBold and IBM Plex Mono
  (SIL OFL 1.1), so the UI no longer depends on egui's thin Ubuntu-Light
  default or on installed system fonts.
- Tab completion for the dotted identifier at the caret, with a compact popup,
  source-guarded responses, and configurable next/previous candidate bindings
  (Ctrl+N / Ctrl+P by default).
- Ctrl+Space documentation lookup (`inspect.getdoc`) in a non-modal, resizable
  formatted window; configurable and clearable in Settings.
- Settings dialog with theme, font sizes and Jupyter-style remappable
  keybindings; **Clear** disables a binding.
- `sadrive --docs` headless probe.
- Command-mode selection, toolbar actions and per-cell execution counts from
  the Jupyter-style keybinding work.

### Changed

- The inspector is compact: no timestamp/status line and no per-entry boxes;
  only the variable list and genuine empty/error/loading states.
- Default UI font size is 15 px and code 14 px; buttons use Plex Medium with a
  stronger Gray 40 border; headings use Plex SemiBold.
- The window clear colour and central panel use the Carbon canvas, fixing the
  black background in light mode.
- Glyphs absent from IBM Plex (`▶`, `■`, `＋`, `✕`, `●`, `⌘`, `⌫`) were replaced
  with text or supported symbols, removing “?” tofu from buttons and status
  text.

### Fixed

- `DisplayManager` objects (the `dm` commonly created by `~/.sage/init.sage`)
  are filtered from the inspector and completion.
- Completion and documentation are serialized with evaluation on the driver,
  suppressed while a cell runs, and discarded when the cell source changes.
- A completion popup owns the keyboard, so Ctrl+N selects the next candidate
  instead of creating a notebook.

## [0.1.0] - 2026-10-24

Initial prototype: cell workspace, stdio Sage driver, RaTeX/SVG/PNG rendering,
variable inspector, native error dialogs, `.sagenote` persistence, SIGINT and
watchdog process control, and headless tests.
