# sagenote

A Rust-native SageMath notebook in the spirit of wxMaxima: every pixel is
`egui`, every calculation comes from a `sage` already installed on the user's
machine.  There is no WebView, no JavaScript, no WASM frontend, and no bundled
SageMath.

## Documentation

- [docs/USER_GUIDE.md](docs/USER_GUIDE.md) — how to use the app
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — layering, module boundaries, threading
- [docs/PROTOCOL.md](docs/PROTOCOL.md) — wire protocol, driver design, discovery
- [docs/UI.md](docs/UI.md) — editor, workspace, inspector, native dialogs
- [docs/RENDERING.md](docs/RENDERING.md) — math pipeline, SVG/PNG, texture cache
- [docs/NOTEBOOK_FORMAT.md](docs/NOTEBOOK_FORMAT.md) — .sagenote JSON schema
- [docs/PLATFORMS.md](docs/PLATFORMS.md) — Linux/macOS differences, building
- [docs/TESTING.md](docs/TESTING.md) — test structure, coverage
- [docs/DESIGN.md](docs/DESIGN.md) — index and non-negotiables
- [CHANGELOG.md](CHANGELOG.md) — release notes

## Features

- **Cell workspace** — scrollable, syntax-highlighted Sage cells with
  Run / Run All / Run Above, move, insert, delete
- **IBM Carbon UI** — light by default, Carbon dark available; embedded IBM
  Plex Sans/Mono, square corners, hairline borders, Blue 60 focus
- **Tab completion** — driver-side namespace/attribute completion with a
  compact, keyboard-navigable popup (Ctrl+N/Ctrl+P)
- **Inline documentation** — Ctrl+Space opens a formatted docstring window
  (inspect.getdoc; no WebView)
- **Math rendering** — native RaTeX (Rust KaTeX) for `text/latex`,
  usvg/resvg for SVG, and the image crate for PNG
- **Variable inspector** — compact, resizable right panel listing user
  variables with type and bounded repr, auto-refreshing every 2 s while visible
- **Native error dialogs** — zenity on Linux, AppKit on macOS; custom
  Retry / Browse / Keep-editing buttons for recovery
- **Notebook persistence** — atomic `.sagenote` saves with dirty tracking,
  close confirmation, and stale-output marking on load
- **Robust process control** — SIGINT interrupt, watchdog restart, and
  guaranteed cleanup on close (no orphaned Python)

## Quick start

```sh
cargo test                    # backend, codecs, fake driver, headless UI, math/SVG/PNG
cargo run --release           # the eframe/wgpu app
cargo run --bin sadrive       # headless: spawn the driver, evaluate one cell, print frames
cargo run --bin sadrive -- -e 'a = 6*7' -e 'a/2'
```

`sadrive` discovers Sage in this order (first *validated* hit wins):

1. `SAGE_BIN`
2. `~/.config/sagenote/config.toml` (written by Browse or `sadrive --sage`):
   ```toml
   sage = "/absolute/path/to/sage"
   ```
3. `PATH`
4. macOS `/Applications/SageMath*.app` / `~/Applications/SageMath*.app`,
   resolving `Contents/Frameworks/Sage.framework/Versions/{Current,*}/local/bin/sage`
   (versioned directories are globbed; highest version wins even when
   `Current` is a dangling symlink)
5. The error dialog's **Browse for sage…** button, which accepts any
   executable, validates it with `--version`, and saves the config entry.

Every candidate is validated by **executing `<candidate> --version`** — never by
`which` or a file test.  That distinguishes "missing" from "broken install" and
yields the version shown in the status bar.  If everything fails, the five
attempts and their individual reasons are printed by `sadrive` and shown in the
GUI's native error dialog.

On macOS, a Finder launch inherits only `/usr/bin:/bin:/usr/sbin:/sbin`.  When
`PATH` discovery fails, `sadrive` re-derives it once with
`$SHELL -lic 'echo "$PATH"'`, retries, and splices the resolved PATH into the
driver child's environment.  The absolute Sage path it actually launched is
printed and will be shown in the status bar.

### Run the protocol without Sage

`tests/fixtures/fake_driver.py` speaks the same NDJSON protocol and needs only
`python3`:

```sh
cargo run --bin sadrive -- --fake-driver -e latex -e raise -e '1+1' --introspect --docs
cargo run --bin sadrive -- --fake-driver -e hang --interrupt-after 0.5 -e '1+1'
cargo run --bin sadrive -- --fake-driver -e ignore-sigint --interrupt-after 0.5 --interrupt-timeout 1
cargo run --bin sadrive -- --fake-driver -e die
```

## Cell workspace

`cargo run --release` starts the desktop app (the package's `default-run` is
`sagenote`).  It uses **eframe 0.31.1 + egui 0.31.1** with
`default-features = false, features = ["wgpu", "default_fonts", "x11", "wayland"]`.
The pin is deliberate: eframe 0.37 moved to a different `App::ui`/panel API,
while 0.31 matches the `update()` + `TopBottomPanel` design this project is
built around.  On Apple Silicon wgpu resolves to Metal; on Linux it uses
Vulkan by default.  Under SSH/llvmpipe:

```sh
WGPU_BACKEND=gl cargo run --release
```

- Vertical scroll area with a stable `id_salt`; each cell is wrapped in
  `ui.push_id(cell.id, …)` and its editor has an explicit stable id.  Adding,
  deleting or moving a cell therefore does not reset the focused editor's
  cursor.
- Rust-native editor: `TextEdit::multiline` + a syntect layouter.  The current
  `egui_code_editor` release pins egui 0.36 and ships its own highlighter instead
  of accepting a caller-supplied `syntect::SyntaxSet`, so the fallback is used.
  Shift+Enter is consumed before the editor sees it; bare Enter inserts a
  newline.  Tab asks the driver for prefix/attribute completions: one candidate
  is inserted, several open a compact, keyboard-navigable popup.  Ctrl+Space
  opens a formatted docstring window for the name at the caret.
- Per-cell state inline: idle / queued / running / ok + elapsed / error /
  interrupted.
- Buttons: Run, insert below, move up, move down, delete.  A bottom
  "＋ Add cell" appends.  Menus: **Run All**, **Run Above**.  Run All and Run
  Above skip empty cells.  At most one cell is on the driver's stack; the
  supervisor queues the rest, and the UI shows them as queued.
- Outputs live in a Gray 10 Carbon panel below the editor.  The text pipeline
  renders plain text, stderr (warning colour), and full red tracebacks; the math
  pipeline adds RaTeX math, SVG and PNG (below).  HTML/3D show the
  scope-limit placeholder.  Saved outputs are dirty-checked and greyed with a
  **stale** tag after load; a backend restart/death also stales every output,
  because the session's globals are gone.
- Moving/deleting a cell mid-computation is safe: events are routed by
  `(CellId, RunId)`, and output for a deleted/superseded cell is discarded.  On
  source edit, any in-flight run is marked superseded for routing and the old
  output is tagged stale.

### Files, dirty state, shortcuts

- `.sagenote` new/open/save/save-as.  rfd is target-scoped: xdg-portal on Linux
  (no GTK), AppKit panels on macOS.
- Atomic save; title bar shows `sagenote — name *` while dirty.
- Window close, **Quit**, ⌘/Ctrl+Q and ⌘/Ctrl+W go through one unsaved-changes
  modal (Save / Discard / Cancel).  Save failure keeps the notebook open.
- Global shortcuts use `egui::Modifiers::COMMAND` (⌘ on macOS, Ctrl
  elsewhere): Ctrl/⌘+N new · +O open · +S save · +Shift+S save as · +.
  interrupt · Ctrl/⌘+I inspector.  Editor bindings are Shift+Enter evaluate,
  Tab complete, Ctrl+Enter run in place, Alt+Enter run and insert below.
  ⌘Q/⌘W are intercepted explicitly because egui does not handle them;
  otherwise macOS could not quit from the keyboard.

### Startup and recovery

Sage discovery and the 90 s ready handshake run on a Tokio multi-threaded
runtime's blocking pool (no `#[tokio::main]`; winit owns the main thread).  The
window shows a spinner and "Starting Sage…" while it runs.  The backend's
supervisor receives an opaque wake callback that calls
`egui::Context::request_repaint`, so output appears without mouse movement.
Discovery failures and driver exit/restart errors present a **native error
dialog** (rfd on Linux, AppKit on macOS) with Retry / Browse for sage… / Keep
editing.  Discovery attempts are included in its description.  The in-window
"Sage is not running" banner remains the fallback, and the native dialog returns
Keep editing when no desktop message provider is available.  The next start is
a restart of the existing supervisor rather than a fresh discovery.

## Math and images

All rendering lives behind one wrapper, `src/render.rs`; the backend and
`.sagenote` model remain windowing-free.

### Math (`text/latex`)

- RaTeX `ratex-types`, `ratex-parser`, `ratex-layout` and
  `ratex-katex-fonts` are pinned to **0.1.15**.  `ratex-parser` is required in
  addition to the two crates named in the original design: `layout()` consumes
  `ParseNode`s.
- `parse → layout → validate_layout → to_display_list`, then the DisplayList is
  rasterized with **tiny-skia** into premultiplied RGBA and uploaded through
  `ColorImage::from_rgba_premultiplied`.  The KaTeX TTFs are embedded; glyph
  outlines come from `ttf-parser`.  Painting uses tiny-skia's `SimpleSRGB`
  blend space so the bytes egui receives are sRGB, not linear.
- Glyphs missing from KaTeX (CJK, emoji, unusual symbols) fall back to a system
  face found by `fontdb`.  `RATEX_UNICODE_FONT`, when set, is loaded into that
  database first.  If no face has the outline, the whole formula falls back.
- **Never blank:** a parse/layout panic, resource-limit error, unsupported
  command, missing glyph, or >2048 px texture shows a red `unrendered` tag and
  the LaTeX source in monospace.  The tag's hover text carries the reason.
- The display style comes from the driver's `display` field: block uses
  `MathStyle::Display` at 19 pt, inline uses `MathStyle::Text` at 15 pt and
  `horizontal_wrapped`.
- Cache key is `(latex hash, style, color, px_size * pixels_per_point)`;
  `Renderer` keeps an LRU of 64 textures.  Cell delete and new/open clear the
  cache, dropping the texture handles.  Rendered images are fitted to the
  available cell width without changing the cache.

### SVG and PNG

- `image/svg+xml` → `usvg::Tree::from_str` with a shared
  `fontdb::Database::load_system_fonts()` database (mandatory for matplotlib
  labels), then `resvg::render` into a `tiny_skia::Pixmap`.  Natural SVG size
  is used; rasterization scale follows `pixels_per_point` and is capped at 2048
  px per side.
- `image/png` is base64-decoded (the driver's `_image_bundle` format), decoded
  with the `image` crate, converted to RGBA8, downscaled to 2048 px if needed,
  and uploaded as unmultiplied sRGB.  Display width is capped at 720 pt.
- Both are tinted grey and tagged **stale** after load.  Inline images use
  `horizontal_wrapped`; block images are placed below the MIME label.
- `text/plain`, stderr and tracebacks are monospaced.  ANSI CSI/OSC sequences
  are stripped at display time by `src/text.rs`; the saved output keeps the
  original bytes.
- `text/html` remains an explicit "not supported in this prototype" label; the
  driver already returns the "interactive 3D plot not supported" text for
  `Graphics3d`.

## Variable inspector

- A resizable **Variables** side panel on the right, visible by default.  Toggle
  it with **View → Inspector** or Ctrl/⌘+I; close with its ✕.  The panel is
  220–420 px wide and remembers its width for the session through egui.
- The app sends one `introspect` request per refresh.  The driver answers with
  `set(_user_ns) - _baseline` and a bounded `reprlib.Repr` (60-char strings, 20
  list/dict items, depth 3, 4000-char cap).  Only the user's names are shown;
  Sage's own globals are excluded by the driver's baseline, and
  `DisplayManager` objects (the `dm` from `init.sage`) are filtered out.
- One request in flight at a time.  A manual ↻ is available; auto-refresh runs
  every 2 s **only while the panel is visible**, and is suppressed while any cell
  is queued/running, including a backend eval that is still active after its
  cell was edited or deleted.  Responses with a stale `IntrospectId` are ignored.
- The panel is deliberately concise: no permanent status line, no timestamp and
  no per-entry boxes.  Each variable is one compact block (name + type, then
  the repr) separated by spacing; starting/loading/error/empty are shown only
  when there is no list to show.
- Backend start/restart/death clears the list; opening another notebook does
  **not**, because the inspector describes the live Sage session, not the file.
- Ctrl/⌘+I is consumed before the editor, so it cannot become italic text.

## Native error dialogs

- Errors use `rfd::MessageDialog`: AppKit on macOS; on Linux the rfd xdg
  backend uses **zenity** for message boxes.  Install `zenity` for native
  dialogs on a minimal Linux desktop.
- Recovery errors expose **Retry**, **Browse for sage…**, and **Keep editing**
  custom buttons.  The discovery attempts and reasons are in the dialog
  description.  Informational errors (open/save failures) are a single OK.
  Browse validates the chosen executable in the background discovery task, so
  a broken binary cannot freeze the UI for the 90 s validation timeout.
- The in-window "Sage is not running" banner remains the fallback if no native
  message provider is available; the driver log records the error either way.
  The native dialog intentionally has no fourth Quit button; the window close,
  File → Quit, and Ctrl/⌘+Q still work after Keep editing.
- This is a blocking native modal on the UI thread, like the file pickers.

## Protocol

The driver is `sage -python -u <driver.py>` (with a compatibility fallback; see
below).  Frames are one JSON object per line, Jupyter-shaped:

```text
{"type":"ready","sage_version":"10.10"}
{"type":"start","id":"<uuid>","run":1}
{"type":"stream","name":"stdout","text":"..."}
{"type":"result","id":"<uuid>","run":1,
 "bundle":{"text/latex":"..."},"display":"block"}
{"type":"error","id":"<uuid>","run":1,"traceback":"..."}
{"type":"introspect","id":1,"vars":[{"name":"a","type":"Integer","value":"42"}]}
{"type":"complete","id":"<uuid>","offset":7,"items":["print(","println("]}
{"type":"doc","id":"<uuid>","offset":4,"name":"plot","kind":"function",
 "signature":"plot(*args, **kwds)","text":"..."}
{"type":"end","id":"<uuid>","run":1}
{"type":"pong"}          # auxiliary answer to ping, never cell output
```

Stdin is a stream of `{"op":"eval"|"introspect"|"complete"|"doc"|"set_display"|"ping", ...}`
objects.  `bundle` is always a mimetype map; `image/png` is base64, SVG/latex/
plain are UTF-8.  `display` is the driver's `block`/`inline` decision, never
inferred from the LaTeX string.  `complete` and `doc` carry the cell `source`
and the caret `offset` (characters); the driver extracts the dotted identifier
there.

`sage/driver.py` is included in the binary with `include_str!` and written at
startup to `std::env::temp_dir()/sagenote-<pid>/driver.py` (not
`$XDG_RUNTIME_DIR`).  `SAGE_DRIVER_OVERRIDE` or `--driver` uses a repo copy
instead, for iteration.

### Why not drive the interactive Sage REPL?

The interactive REPL is IPython-based.  Over a pipe it emits a banner,
`In [n]:` prompt echo, ANSI colour and `...` continuation prompts; stdout is
block-buffered when not a tty; `%display latex` is IPython-magic global state
the user can turn off; and pasting multi-line blocks needs continuation
handling.  A JSON line cannot contain a raw newline, so newline-delimited JSON
has no delimiter collision and user `print()` output cannot corrupt framing.
The driver probes `latex()`, then Sage's rich output containers, then `repr()`,
and caps every payload at ~1 MB before it crosses the pipe.

`===SAGE_OUTPUT_BEGIN_<session_nonce>===` is retained only as a documented debug
fallback for a hypothetical raw-REPL path; payloads would be base64-encoded.
It is **not used** by the JSON transport.

## Process control

Both targets are Unix.

- Spawn with `Command::process_group(0)`; `sage` is a shell script wrapping
  Python, so the spawned pid is the group leader, not the interpreter.
- Interrupt with `libc::kill(-pgid, SIGINT)`.  Never `Child::kill()` for an
  interrupt: that is SIGKILL and silently destroys the session.
- Restart = SIGTERM the group → wait with a timeout → SIGKILL if needed →
  respawn → ready handshake → preamble (`set_display`; the driver has already
  snapshotted the post-`sage.all` baseline and re-sourced
  `~/.sage/init.sage`).  Start and restart share
  `spawn_and_handshake`, so the preamble cannot be forgotten.
- Watchdog: if no `end` arrives within `interrupt_timeout` (default 5 s) after
  SIGINT, SIGKILL the group, restart, and mark in-flight/queued cells
  `Interrupted`.
- `Drop` sends SIGTERM, waits `shutdown_grace` (default 2 s), then SIGKILLs and
  waits, so closing the app cannot orphan a Python interpreter holding the
  notebook's data.

### Known limitation

SIGINT on this stdio prototype interrupts the entire `sage -python` process
group, not an individual cell.  Sage evaluation is single-threaded and the
driver's `run()` is the only thing on the stack, so an interrupt can only abort
whatever is currently executing; cells queued behind it are dropped by the
client and marked `Interrupted`.  Per-cell cancellation arrives with the Jupyter
kernel backend (`interrupt_request`), which is why `interrupt()` already takes
`InterruptScope::{Cell, Kernel}`.  Both scopes currently mean "SIGINT the
group".

### Sage 10.10 `-python` compatibility

The primary launch path is exactly the specified
`sage -python -u driver.py`.  Some newer Sage builds (the `sage.cli` rewrite in
10.10, for example) reject `-python`.  If that launch exits before `ready`,
`backend/driver.rs` re-executes the driver through Sage's own Python with
`sage -c 'os.execv(sys.executable, [...])'`.  That preserves every environment
variable the Sage wrapper sets up (important inside the macOS .app framework),
and the launch is cached for restarts.  This is a fallback, not a replacement;
no Sage code is bundled.

## Notebook format

`.sagenote` is JSON with `format_version: 1`:

```json
{"format_version":1,
 "meta":{"created":"2026-10-24T00:00:00Z","sage_version":"10.10"},
 "cells":[{"id":"<uuid>","source":"a = 6*7\na",
           "outputs":[{"mime":"text/latex","data":"42","display":"block","stale":false}],
           "state":"ok"}]}
```

Saves write a temp file in the same directory, fsync, then `fs::rename`
(atomic within one filesystem on both targets).  On load every saved output is
marked `stale` — the globals that produced it do not exist in a fresh session —
and rendered greyed with a tag rather than silently re-run.  `IpynbCodec` exists
as a trait hook only.

## Platform support

- **Linux (x11 + Wayland)** — the supported platform.  Everything in this
  README is tested here, on real Sage 10.10, through the native zenity error
  dialogs and the xdg-portal file dialogs.
- **macOS (Apple Silicon)** — best effort.  It compiles and passes the same
  test suite in CI, but it has **not** been runtime-tested on real hardware:
  the AppKit panels, the signed .app cold start, and Retina rendering are
  unverified.  No .app bundle or signing setup is provided yet.
- **Windows** — unsupported.

## Building

Rust **1.89+** (the dependency graph's MSRV, declared in `Cargo.toml`).

### Linux (Debian/Ubuntu)

```sh
sudo apt update
sudo apt install pkg-config libxkbcommon-dev libwayland-dev libxcb1-dev \
  libx11-dev mesa-vulkan-drivers libvulkan-dev
# rfd uses xdg-portal on Linux, so GTK is not needed.  Install libgtk-3-dev
# only if you deliberately switch rfd to its gtk3 feature.
# Recommended for SVG labels and CJK math fallback, not for compilation:
#   fonts-dejavu-core fonts-noto-cjk
# rfd uses zenity for native error message boxes:
#   zenity
```

### macOS (Apple Silicon)

```sh
xcode-select --install
```

Then:

```sh
cargo run --release                 # the eframe/wgpu app
cargo run --bin sadrive             # headless probe
```

Over SSH or under llvmpipe, the GUI needs `WGPU_BACKEND=gl`; the default wgpu
path fails with a Vulkan error that reads like a code bug.  Confirm the Metal
adapter once with `RUST_LOG=wgpu=info` rather than shipping a silent GL
fallback.  On Apple Silicon wgpu resolves to Metal.  The Linux rfd build uses
xdg-portal; a running desktop portal is required for the file dialogs.  Native
error message boxes on Linux are implemented by rfd through **zenity**; install
it if your desktop does not already provide it.

## Testing

`cargo test --all-targets` runs 70 tests: 24 lib + 36 app/UI/render + 10
fixture-integration.  It covers protocol serde round-trips, unknown-frame
tolerance, the notebook codec, stale-on-load, format version, atomic save, cell
moves, discovery version parsing/natural sort, validation by execution,
ready/start/stream/result/end, error/end, SIGINT recovery, watchdog restart,
mid-cell death, queued-cell drop, introspection, completion, documentation,
headless egui passes (cell workspace, menu bar, status bar, inspector panel,
synthesized Shift+Enter), completion token/insertion helpers, doc text
formatting, Carbon palette contrast,
rendering (RaTeX math, SVG rectangle, 1×1 PNG, texture cache, ANSI
stripping, mixed rich outputs).

`cargo clippy --all-targets -- -D warnings` must be clean.

## Layout

```text
Cargo.toml
src/main.rs             # eframe entry point (wgpu)
src/app.rs              # App state, startup, event routing, files, modals
src/ui.rs               # menu/status bar, cell workspace, syntect editor, completion
src/render.rs           # RaTeX/tiny-skia, usvg/resvg, PNG, texture cache
src/settings.rs         # settings, keybindings, Carbon palette, bundled fonts
src/text.rs             # ANSI stripping for displayed output
assets/fonts/           # embedded IBM Plex Sans/Mono (SIL OFL 1.1)
src/bin/sadrive.rs      # headless probe
src/lib.rs
src/backend/mod.rs      # EvalBackend, BackendEvent, ids, mimetypes
src/backend/protocol.rs # serde frame/request types
src/backend/driver.rs   # spawn, SIGINT, watchdog, NDJSON parser
src/cells.rs            # notebook model, .sagenote codec, ipynb hook
src/platform.rs         # Sage discovery, temp dir, macOS branches
sage/driver.py          # the Python driver
tests/fixtures/fake_driver.py
tests/driver_integration.rs
docs/DESIGN.md          # authority on protocol/layout/platform choices
```

`src/backend/` and `src/cells.rs` import no `eframe`, `egui`, or windowing
types.  That is what keeps `cargo test` and `cargo run --bin sadrive` runnable
headless on both platforms.  `app.rs`/`ui.rs` own every pixel and never reach
into the supervisor's process handles except through `EvalBackend`.

## CI

`.github/workflows/ci.yml` runs on `macos-14` (arm64) and `ubuntu-22.04` with
native builds.  It runs formatting, clippy, `cargo test --all-targets` (backend,
cells, fake-driver integration, headless egui workspace, math/SVG/PNG, and
inspector/menu tests) and a release build of both binaries.  The release
binaries are uploaded as per-OS artifacts so each platform can be smoke-tested
without a local toolchain.  The native error dialogs and GUI are smoke-tested
manually; wgpu/eframe cannot be cross-compiled to `aarch64-apple-darwin` from
Linux.

## License

MIT OR Apache-2.0; see [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).  SageMath itself is not distributed here.
The embedded KaTeX TTFs come from `ratex-katex-fonts` and remain under the
SIL Open Font License; the notice and license are vendored at
[third_party/katex-fonts/](third_party/katex-fonts/).  The bundled UI typefaces
(IBM Plex Sans/Mono in [assets/fonts/](assets/fonts/)) are Copyright IBM Corp.
and are licensed under the SIL Open Font License 1.1; see
[assets/fonts/OFL.txt](assets/fonts/OFL.txt).
