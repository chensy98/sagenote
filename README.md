# sagenote

A Rust-native SageMath notebook in the spirit of wxMaxima: every pixel is
`egui`, every calculation comes from a `sage` already installed on the user's
machine.  There is no WebView, no JavaScript, no WASM frontend, and no bundled
SageMath.

**Status: M1 + M2 + M3 + M4 are implemented.**  M1 supplies the typed NDJSON
backend, process supervision, Sage discovery and the notebook model.  M2 adds
the eframe/wgpu desktop shell: scrollable cell workspace, a Rust-native syntax-
highlighted editor, new/open/save, dirty tracking, close confirmation, Run All /
Run Above, and the keyboard shortcuts.  M3 renders `text/latex` with RaTeX +
tiny-skia, `image/svg+xml` with usvg/resvg, and base64 `image/png` with the
image crate.  M4 adds the variable inspector, native error dialogs, and a styling
pass.  See [Milestones](#milestones); protocol, platform and output decisions
are recorded in [docs/DESIGN.md](docs/DESIGN.md).

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
   executable, validates it with `--version`, and saves the config entry.  M4
   presents this as a native rfd error dialog, with the in-window banner as a
   fallback.

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
cargo run --bin sadrive -- --fake-driver -e latex -e raise -e '1+1' --introspect
cargo run --bin sadrive -- --fake-driver -e hang --interrupt-after 0.5 -e '1+1'
cargo run --bin sadrive -- --fake-driver -e ignore-sigint --interrupt-after 0.5 --interrupt-timeout 1
cargo run --bin sadrive -- --fake-driver -e die
```

## M1 acceptance checklist

The criteria in `AGENTS.md` were exercised against real SageMath 10.10 on Linux
and against the fixture on every `cargo test`:

| Criterion | Command |
|---|---|
| multi-statement cell: `start`, `stream`/`result`, `end` | `sadrive -e $'a = 6*7\na/2'` |
| trailing bare expression yields `text/latex` or `text/plain` | `sadrive -e 'sqrt(2)'` |
| raising cell: `error` with traceback, then `end` | `sadrive -e 'raise ValueError("boom")'` |
| Ctrl-C during `while True: pass`: `error` then `end`, next eval works | `sadrive -e 'while True: pass' --interrupt-after 1 -e '40+2'` |
| driver dies mid-cell: noticed, not waited on forever | `sadrive -e $'import os\nos._exit(9)'` |
| watchdog: ignored SIGINT => SIGKILL, restart, next eval works | see the `signal.SIG_IGN` example below |
| introspection frame | `sadrive -e 'a=42' --introspect` |

Real-watchdog command:

```sh
cargo run --bin sadrive -- \
  -e $'import signal\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\nwhile True:\n    pass' \
  --interrupt-after 1 --interrupt-timeout 2 -e '40+2'
```

Plot coverage uses Sage's rich-output machinery: SVG for 2D graphics and an
explicit text placeholder for 3D/`OutputScene*` results.

## M2 workspace

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

### Cell workspace

* Vertical scroll area with a stable `id_salt`; each cell is wrapped in
  `ui.push_id(cell.id, …)` and its editor has an explicit stable id.  Adding,
  deleting or moving a cell therefore does not reset the focused editor's
  cursor.
* Rust-native editor: `TextEdit::multiline` + a syntect layouter.  The current
  `egui_code_editor` release pins egui 0.36 and ships its own highlighter instead
  of accepting a caller-supplied `syntect::SyntaxSet`, so the spec's fallback is
  used.  Shift+Enter is consumed before the editor sees it; bare Enter inserts a
  newline.
* Per-cell state inline: idle / queued / running / ok + elapsed / error /
  interrupted.
* Buttons: Run, insert below, move up, move down, delete.  A bottom
  "＋ Add cell" appends.  Menus: **Run All**, **Run Above**.  Run All and Run
  Above skip empty cells.  At most one cell is on the driver's stack; the
  supervisor queues the rest, and the UI shows them as queued.
* Outputs live in a light-grey panel below the editor.  M2's text pipeline
  renders plain text, stderr (orange), and full red tracebacks; M3 adds RaTeX
  math, SVG and PNG (below).  HTML/3D show the scope-limit placeholder.  Saved
  outputs are dirty-checked and greyed with a **stale** tag after load; a
  backend restart/death also stales every output, because the session's globals
  are gone.
* Moving/deleting a cell mid-computation is safe: events are routed by
  `(CellId, RunId)`, and output for a deleted/superseded cell is discarded.  On
  source edit, any in-flight run is marked superseded for routing and the old
  output is tagged stale.

### Files, dirty state, shortcuts

* `.sagenote` new/open/save/save-as.  rfd is target-scoped: xdg-portal on Linux
  (no GTK), AppKit panels on macOS.
* Atomic save; title bar shows `sagenote — name *` while dirty.
* Window close, **Quit**, ⌘/Ctrl+Q and ⌘/Ctrl+W go through one unsaved-changes
  modal (Save / Discard / Cancel).  Save failure keeps the notebook open.
* Shortcuts all use `egui::Modifiers::COMMAND` (⌘ on macOS, Ctrl elsewhere):
  Shift+Enter evaluate · Ctrl/⌘+N new · +O open · +S save · +Shift+S save as ·
  +. interrupt · Ctrl/⌘+I inspector.  ⌘Q/⌘W are intercepted explicitly because
  egui does not handle them; otherwise macOS could not quit from the keyboard.

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

### M2 smoke test

```sh
# real Sage
DOT_SAGE=/tmp/sagenote-dot-sage cargo run --release

# headless checks (no display)
cargo test --all-targets
```

The headless tests render the cell workspace, menu bar and status bar through
`egui::Context::run`, including a synthesized Shift+Enter key event.  They need
no X11/Wayland server.

**M2 boundaries:** math/image rendering is M3 and the inspector/native errors
are M4, below.

## M3 math and images

M3 lives behind one wrapper, `src/render.rs`; the backend and `.sagenote` model
remain windowing-free.

### Math (`text/latex`)

* RaTeX `ratex-types`, `ratex-parser`, `ratex-layout` and
  `ratex-katex-fonts` are pinned to **0.1.15**.  `ratex-parser` is required in
  addition to the two crates named in the original design: `layout()` consumes
  `ParseNode`s.
* `parse → layout → validate_layout → to_display_list`, then the DisplayList is
  rasterized with **tiny-skia** into premultiplied RGBA and uploaded through
  `ColorImage::from_rgba_premultiplied`.  The KaTeX TTFs are embedded; glyph
  outlines come from `ttf-parser`.  Painting uses tiny-skia's `SimpleSRGB`
  blend space so the bytes egui receives are sRGB, not linear.
* Glyphs missing from KaTeX (CJK, emoji, unusual symbols) fall back to a system
  face found by `fontdb`.  `RATEX_UNICODE_FONT`, when set, is loaded into that
  database first.  If no face has the outline, the whole formula falls back.
* **Never blank:** a parse/layout panic, resource-limit error, unsupported
  command, missing glyph, or >2048 px texture shows a red `unrendered` tag and
  the LaTeX source in monospace.  The tag's hover text carries the reason.
* The display style comes from the driver's `display` field: block uses
  `MathStyle::Display` at 19 pt, inline uses `MathStyle::Text` at 15 pt and
  `horizontal_wrapped`.
* Cache key is `(latex hash, style, color, px_size * pixels_per_point)`;
  `Renderer` keeps an LRU of 64 textures.  Cell delete and new/open clear the
  cache, dropping the texture handles.  Rendered images are fitted to the
  available cell width without changing the cache.

### SVG and PNG

* `image/svg+xml` → `usvg::Tree::from_str` with a shared
  `fontdb::Database::load_system_fonts()` database (mandatory for matplotlib
  labels), then `resvg::render` into a `tiny_skia::Pixmap`.  Natural SVG size
  is used; rasterization scale follows `pixels_per_point` and is capped at 2048
  px per side.
* `image/png` is base64-decoded (the driver's `_image_bundle` format), decoded
  with the `image` crate, converted to RGBA8, downscaled to 2048 px if needed,
  and uploaded as unmultiplied sRGB.  Display width is capped at 720 pt.
* Both are tinted grey and tagged **stale** after load.  Inline images use
  `horizontal_wrapped`; block images are placed below the MIME label.
* `text/plain`, stderr and tracebacks are monospaced.  ANSI CSI/OSC sequences
  are stripped at display time by `src/text.rs`; the saved output keeps the
  original bytes.
* `text/html` remains an explicit "not supported in this prototype" label; the
  driver already returns the "interactive 3D plot not supported" text for
  `Graphics3d`.

### M3 verification

```sh
cargo test --bin sagenote     # 24 app/UI/render tests
cargo test --all-targets      # includes 22 lib + 9 fixture integration
DOT_SAGE=/tmp/sagenote-dot-sage cargo run --release
```

The headless tests rasterize `x^2 + \frac{1}{2}`, an SVG rectangle and a
1×1 PNG, exercise the texture-cache hit path, strip ANSI, and render mixed rich
outputs through `egui::Context::run`.  A real Sage 10.10 `plot(sin(x), ...)`
SVG was also rasterized with resvg during verification; its axis labels required
the system-font database.

**M3 boundaries:** no HTML renderer and no interactive 3D; both are explicit
placeholders.  The inspector and native error dialogs are M4, below.

## M4 inspector, native errors, styling

### Variable inspector

* A resizable **Variables** side panel on the right, visible by default.  Toggle
  it with **View → Inspector** or Ctrl/⌘+I; close with its ✕.  The panel is
  220–420 px wide and remembers its width for the session through egui.
* The app sends one `introspect` request per refresh.  The driver answers with
  `set(_user_ns) - _baseline` and a bounded `reprlib.Repr` (60-char strings, 20
  list/dict items, depth 3, 4000-char cap).  Only the user's names are shown;
  Sage's own globals are excluded by the driver's baseline.
* One request in flight at a time.  A manual ↻ is available; auto-refresh runs
  every 2 s **only while the panel is visible**, and is suppressed while any cell
  is queued/running, including a backend eval that is still active after its
  cell was edited or deleted.  The panel shows the count, "updated …", and
  "auto 2 s"; responses with a stale `IntrospectId` are ignored.
* Each row shows the name in monospace, the Python type, and the repr value.
  Backend start/restart/death clears the list; opening another notebook does
  **not**, because the inspector describes the live Sage session, not the file.
* Ctrl/⌘+I is consumed before the editor, so it cannot become italic text.

### Native error dialogs

* Errors use `rfd::MessageDialog`: AppKit on macOS; on Linux the rfd xdg
  backend uses **zenity** for message boxes.  Install `zenity` for native
  dialogs on a minimal Linux desktop.
* Recovery errors expose **Retry**, **Browse for sage…**, and **Keep editing**
  custom buttons.  The discovery attempts and reasons are in the dialog
  description.  Informational errors (open/save failures) are a single OK.
  Browse validates the chosen executable in the background discovery task, so
  a broken binary cannot freeze the UI for the 90 s validation timeout.
* The in-window "Sage is not running" banner remains the fallback if no native
  message provider is available; the driver log records the error either way.
  The native dialog intentionally has no fourth Quit button; the window close,
  File → Quit, and Ctrl/⌘+Q still work after Keep editing.
* This is a blocking native modal on the UI thread, like the file pickers.

### Styling pass

* Menu and status bars use a subtle border and off-white fill.
* Cell cards have 8 px corners, a 12 px inset, and a blue border while running.
* Per-cell state is a tinted pill (idle, queued, running, ok + elapsed, error,
  interrupted).
* The inspector has its own panel surface and section rhythm; the About dialog
  now reports M4 and the current shortcut set.
* Output panels keep the M3 MIME/header/stale structure.

### M4 verification

```sh
cargo test --all-targets      # 22 lib + 24 app/UI/render + 9 fixture = 55
cargo clippy --all-targets -- -D warnings
cargo run --release
```

The live app was run against Sage 10.10: the inspector auto-refreshed at exactly
2 s intervals (request/response pairs with increasing `IntrospectId`), and the
native discovery-failure dialog opened through zenity with the custom Retry /
Browse / Keep editing labels.  The headless tests cover the inspector panel,
menu/status rendering and a synthesized Shift+Enter.  The GUI was not
runtime-tested on macOS; the AppKit path is compile-time `cfg`-gated and the CI
matrix builds/tests the fixture path.  Screenshot capture is denied in this
session, so visual checks were done through generated previews and the native
dialog's process args.

**M4 boundaries:** no Jupyter kernel backend, no ipynb round-trip, no
interactive 3D/HTML, no Windows.  The inspector is read-only; value editing is
out of scope.

## Protocol (M1)

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
{"type":"end","id":"<uuid>","run":1}
{"type":"pong"}          # auxiliary answer to ping, never cell output
```

Stdin is a stream of `{"op":"eval"|"introspect"|"set_display"|"ping", ...}`
objects.  `bundle` is always a mimetype map; `image/png` is base64, SVG/latex/
plain are UTF-8.  `display` is the driver's `block`/`inline` decision, never
inferred from the LaTeX string.

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

* Spawn with `Command::process_group(0)`; `sage` is a shell script wrapping
  Python, so the spawned pid is the group leader, not the interpreter.
* Interrupt with `libc::kill(-pgid, SIGINT)`.  Never `Child::kill()` for an
  interrupt: that is SIGKILL and silently destroys the session.
* Restart = SIGTERM the group → wait with a timeout → SIGKILL if needed →
  respawn → ready handshake → preamble (`set_display`; the driver has already
  snapshotted the post-`sage.all` baseline and re-sourced
  `~/.sage/init.sage`).  Start and restart share
  `spawn_and_handshake`, so the preamble cannot be forgotten.
* Watchdog: if no `end` arrives within `interrupt_timeout` (default 5 s) after
  SIGINT, SIGKILL the group, restart, and mark in-flight/queued cells
  `Interrupted`.
* `Drop` sends SIGTERM, waits `shutdown_grace` (default 2 s), then SIGKILLs and
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

## Layout

```text
Cargo.toml
src/main.rs             # eframe entry point (wgpu)
src/app.rs              # App state, startup, event routing, files, modals
src/ui.rs               # menu/status bar, cell workspace, syntect editor
src/render.rs           # M3: RaTeX/tiny-skia, usvg/resvg, PNG, texture cache
src/text.rs             # ANSI stripping for displayed output
src/bin/sadrive.rs      # M1 headless probe
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

## Notebook format

`.sagenote` is JSON with `format_version: 1` from the first commit:

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
cargo run --bin sadrive             # headless M1 probe
```

Over SSH or under llvmpipe, the GUI needs `WGPU_BACKEND=gl`; the default wgpu
path fails with a Vulkan error that reads like a code bug.  Confirm the Metal
adapter once with `RUST_LOG=wgpu=info` rather than shipping a silent GL
fallback.  On Apple Silicon wgpu resolves to Metal.  The Linux rfd build uses
xdg-portal; a running desktop portal is required for the file dialogs.  Native
error message boxes on Linux are implemented by rfd through **zenity**; install
it if your desktop does not already provide it.

**Note on the sandbox used for the M1 verification:** this development machine's
`$HOME/.sage` is read-only, so the real-Sage checks were run with
`DOT_SAGE=$PWD/.sagehome/.sage`.  That is an environment detail, not an
application requirement.

## CI

`.github/workflows/ci.yml` runs on `macos-14` (arm64) and `ubuntu-22.04` with
native builds.  It runs formatting, clippy, `cargo test --all-targets` (backend,
cells, fake-driver integration, headless egui workspace, M3 math/SVG/PNG, and M4
inspector/menu tests) and a release build of both binaries.  The native error
dialogs and GUI are smoke-tested manually; wgpu/eframe cannot be cross-compiled
to `aarch64-apple-darwin` from Linux.

## Milestones

* **M1 (done)** — headless `sadrive`: spawn driver, evaluate cells, print
  typed frames, SIGINT, watchdog, death detection, discovery, notebook codec.
* **M2 (done)** — eframe/wgpu shell: scrollable cell workspace, Rust-native
  syntect editor, plain-text output, new/open/save, dirty/close-confirm,
  Run All / Run Above, shortcuts, background Sage startup and recovery.
* **M3 (done)** — RaTeX + tiny-skia math, usvg/resvg SVG, base64 PNG, ANSI
  cleanup, LRU textures, stale rendering, and source fallback.
* **M4 (done)** — variable inspector, native error dialogs, styling pass.

All four milestones are now implemented.  Out of scope until asked: Jupyter
kernel backend, ipynb round-trip, 3D/HTML plots, real autocompletion, Windows,
bundled Sage.  The seams are present: `EvalBackend`, `MimeBundle`,
`NotebookCodec`, `InterruptScope`, and `completion()` (currently emits an
empty vector).

## License

MIT OR Apache-2.0.  SageMath itself is not distributed here.  The embedded
KaTeX TTFs come from `ratex-katex-fonts` and remain under the SIL Open Font
License; see that crate's `FONT_NOTICE.txt` and `OFL.txt`.
