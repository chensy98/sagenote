# Changelog

All notable changes to sagenote are documented here. Versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-11-23

### Added

- VSCode-Jupyter cell chrome: no box, a `[n]` execution-count gutter, a 2 px
  Blue 60 left accent for the selected/running cell, and a small floating
  toolbar (Run / + / ↑ / ↓ / ×) that appears in a 20 px strip above the editor
  only while the cell is selected.
- Completion candidates now carry a kind (`function`, `class`, `module`,
  `variable`, `keyword`, `unknown`) probed in the live namespace; the popup
  draws a colour-coded icon column so the names left-align. Candidate names
  are clean — the rlcompleter `(` suffix on callables is stripped — and
  accepting a function/class with auto-close on inserts `name()` with the
  caret between the parentheses.
- Callables in the completion list also show their **signature** on the same
  line, dimmed after the name, so `pri` → `print(value, ..., sep=' ', end='\n')`.
  Long signatures are truncated to fit the popup, with the full list on hover.
- Docs-follow mode: Ctrl+Space over an open completion popup shows the
  selected candidate's documentation in a panel anchored to the right of the
  candidate list, following the selection (Ctrl+N/Ctrl+P).
- Documentation lookup is caret-aware: with the caret inside an argument
  list, Ctrl+Space resolves the innermost enclosing call's callee (e.g.
  `plot(x, ␣)` → docs for `plot`).
- Auto-close brackets and quotes: typing an opener inserts the pair, typing
  a closer over the same closer skips over it, Backspace between an empty
  pair deletes both, and a selection is wrapped. A Settings checkbox
  (default on) toggles it.
- A dependency policy is now recorded in `docs/DESIGN.md`: official Sage is
  the sole Python-side dependency and the live namespace is the only source
  of completion/documentation; LSP is excluded as policy.

### Changed

- Shift+Enter now moves keyboard focus into the next cell's editor (VSCode
  style) instead of leaving the caret in the old cell; at the last cell a new
  cell is appended and focused, so repeated Shift+Enter trails the notebook.
  Alt+Enter similarly lands in the inserted cell's editor.
- The completion popup's left edge is now anchored at the start of the
  completed token instead of following the caret.
- The menu bar no longer carries Run All / Interrupt quick buttons; the
  toolbar below is their single home (the Notebook menu items stay).
- The `sagenote` label was removed from the start of the menu bar; the
  native window title (`sagenote — name *`) is the single home for the
  program name and notebook title.
- Rendered outputs no longer show a mimetype tag (`text/latex`,
  `image/svg+xml`, …): the transport mimetype is protocol plumbing, not
  something the reader needs. A label still appears for a mimetype this
  client cannot render, where it says what the raw payload is.
- The settings-dialog font-size sliders now show a visible hairline rail with
  Blue 60 progress up to a ~16 px circular thumb, instead of an invisible
  rail and a ~26 px handle.

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
