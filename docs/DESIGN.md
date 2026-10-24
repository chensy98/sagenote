# sagenote design

This document is the authority for the protocol, layout, output handling and
platform decisions.  Read the section you are about to touch.  When code and
this file disagree, fix one deliberately — do not let them drift.

Status: **M1 (headless), M2 (eframe/wgpu shell), M3 (math/SVG/PNG) and M4
(inspector/native errors/styling) implemented.**

## 0. Non-negotiables

* Every pixel is egui/eframe; every calculation is an external `sage` spawned
  at runtime.  No WebView, no JavaScript, no WASM frontend, no bundled Sage.
* `src/backend/` and `src/cells.rs` import no `eframe`, no `egui`, no windowing
  type.  CI can therefore run them headless.
* Frames are typed by `type`, not parsed out of user text.  Output kinds are
  mimetypes in a bundle, never string sniffing on `repr`.
* The only backend implementation is `StdioDriverBackend`.  A future
  `JupyterKernelBackend` (ZeroMQ, `interrupt_request`, per-request `msg_id`)
  must be droppable in without UI changes.

## 1. Layering

```text
sadrive (M1)                 app/ui (M2/M3)                   future
    |                            |                                |
    +-------- EvalBackend -------+--------- JupyterKernelBackend -+
                     |
              StdioDriverBackend (supervisor thread)
                     |  NDJSON over pipes
              sage -python -u driver.py

app.rs    — state, Tokio startup, event routing, file lifecycle, modals
ui.rs     — menu/status bar, cell workspace, syntect editor, output blocks
render.rs — M3: RaTeX/tiny-skia, usvg/resvg, PNG, texture cache
text.rs   — ANSI stripping for display only
main.rs   — eframe/wgpu entry point
```

`crate::backend::EvalBackend` is the UI-facing seam:

```rust
fn start(&mut self) -> Result<()>;
fn restart(&mut self) -> Result<()>;
fn eval(&mut self, cell: CellId, run: RunId, source: &str) -> Result<()>;
fn interrupt(&mut self, scope: InterruptScope) -> Result<()>;   // Cell | Kernel
fn introspect(&mut self, req: IntrospectId) -> Result<()>;
fn completion(&mut self, cell: CellId, offset: usize) -> Result<()>;
fn declared_mimetypes(&self) -> &[MimeType];
fn events(&self) -> mpsc::Receiver<BackendEvent>;
```

`mpsc` is `crossbeam_channel` so the receiver can be cloned out of `&self`.
The channel is single-consumer in practice: `App::update()` (or `sadrive`) is
the only drainer.

Ids:

* `CellId` — UUID string on the wire, stable across save/load and reordering.
* `RunId` — monotonic per evaluation.  Results route by `(cell, run)` and are
  discarded if the cell is gone or the run was superseded.
* `IntrospectId` — request id echoed by the `introspect` frame.

## 2. Discovery

Order, first **validated** hit wins:

1. `SAGE_BIN`
2. `~/.config/sagenote/config.toml`, key `sage` (alias `sage_bin`), written by
   the picker
3. `PATH`
4. macOS `/Applications/SageMath*.app`, `~/Applications/SageMath*.app`,
   `Contents/Frameworks/Sage.framework/Versions/{Current,*}/local/bin/sage`.
   `Current` may dangle; versioned directories are globbed and sorted
   naturally (10.10 > 9.8).
5. Browse button (M4), which accepts any executable and saves the config.

Validation is always `<candidate> --version`, never `which` or a file test.
The oracle distinguishes "missing" from "broken install" and supplies the
status-bar version.  Failures are accumulated as
`DiscoveryFailure { stage, candidate, reason }`; the error dialog shows all
five stages.

macOS Finder launches inherit `/usr/bin:/bin:/usr/sbin:/sbin`.  If step 3 fails,
re-derive PATH once via `$SHELL -lic 'echo "$PATH"'`, retry, and splice that
PATH into the driver child's environment.  The resolved absolute Sage path is
shown in the status bar.  Signed .app cold start is 10–20 s; the ready handshake
allows 90 s and the UI shows a visible "Starting Sage…" state.

## 3. Wire protocol

`sage -python -u <driver.py>` (compatibility fallback, below).  One JSON object
per line, Jupyter-shaped:

| type | fields |
|---|---|
| `ready` | `sage_version` |
| `start` | `id`, `run` |
| `stream` | `name`: `stdout`/`stderr`, `text` |
| `result` | `id`, `run`, `bundle`, `display`: `block`/`inline` |
| `error` | `id`, `run`, `traceback` |
| `introspect` | `id`, `vars`: `[{name,type,value}]` |
| `end` | `id`, `run` |
| `pong` | auxiliary liveness answer to `ping`; never cell output |

Mimetypes in a bundle: `text/latex`, `image/svg+xml`, `image/png` (base64),
`text/plain`, `text/html` (placeholder/HTML-only).  Unknown keys are preserved
by `MimeBundle`; unknown frame types parse to `Frame::Unknown` and do not kill
the session.

Stdin requests: `{"op":"eval"|"introspect"|"set_display"|"ping"}`.
`set_display` carries `mode`: `block`|`inline`.

Framing rationale: a JSON line cannot contain a raw newline, so user data cannot
collide with the delimiter and no sentinel is needed.  The raw-REPL sentinel
`===SAGE_OUTPUT_BEGIN_<session_nonce>===` is kept only as a documented debug
fallback; it would base64-encode payloads.  It is not used.

### Driver, and why not the interactive REPL

The Sage REPL is IPython-based.  Over a pipe it emits a banner, `In [n]:` prompt
echo, ANSI colour and `...` continuation prompts; stdout is block-buffered when
not a tty; `%display latex` is IPython-magic global state the user can switch
off; and pasting a multi-line block requires continuation handling.

`sage/driver.py` instead:

* `from sage.all import *`, then snapshots `_baseline = set(globals())`
  immediately, so the inspector sees user variables rather than ~3000 Sage
  builtins.  Driver internals are excluded.
* Dups fd 1 for frames and redirects fd 1 to `/dev/null` **before** importing
  Sage, then replaces `sys.stdout`/`sys.stderr` with capped `StringIO`
  buffers drained into `stream` frames.  A `print` loop cannot corrupt framing
  or allocate 50 MB.
* Applies `sage.repl.preparse.preparse`, so `^`, `R.<x> =` and friends keep
  Sage semantics.
* Executes the preparsed AST body, then `eval`s a trailing `ast.Expr` (there is
  no display hook; bare `exec` prints nothing).  A trailing `;` suppresses
  display, as in the REPL.
* Probes output: `latex(obj)` → Sage rich-output machinery (`_rich_output_formatter`
  / `_rich_repr_`) for SVG/PNG/HTML/scene → `repr(obj)`.  Many Sage objects
  define `_latex_` but not `_repr_latex_`, so `latex()` is canonical.  A
  graphical object whose `latex()` is a matplotlib PGF dump or
  `Graphics3d Object` is routed to the rich machinery instead; that text is
  not KaTeX math.
* Catches `KeyboardInterrupt` and emits `error` then `end`, never swallowing it.
* Caps every payload at ~1 MB with an explicit marker before it crosses the
  pipe.  Oversized images become a visible text placeholder rather than
  truncated base64.
* Sources `$SAGE_STARTUP_FILE` / `~/.sage/init.sage` explicitly, because
  `sage -python` does not.

### Sage CLI compatibility

The primary launch is exactly `sage -python -u driver.py`.  Sage 10.10's new
`sage.cli` rejects `-python`.  If the primary launch exits before `ready`,
`backend/driver.rs` re-executes the driver through Sage's own Python with
`sage -c 'os.execv(sys.executable, [...])'`.  This keeps the environment the
Sage wrapper set up, which is required inside the macOS .app framework, and the
launch is cached for restarts.  The fallback is a compatibility branch, not a
second supported product.

### Known limitation

SIGINT on this stdio prototype interrupts the entire `sage -python` process
group, not an individual cell.  Sage evaluation is single-threaded and the
driver's `run()` is the only thing on the stack, so an interrupt can only abort
whatever is currently executing; cells queued behind it are dropped by the
client and marked `Interrupted`.  Per-cell cancellation arrives with the Jupyter
kernel backend (`interrupt_request`), which is why `interrupt()` already takes
`InterruptScope::{Cell, Kernel}`.  Both currently mean "SIGINT the group".

## 4. Process control and threading

Both platforms are Unix.

* `Command::process_group(0)`; the spawned pid is the group leader because
  `sage` is a shell script wrapping Python.
* Interrupt: `libc::kill(-pgid, SIGINT)`.  `Child::kill()` is SIGKILL and is
  reserved for the watchdog/drop fallback.
* Restart: SIGTERM group → wait `shutdown_grace` → SIGKILL → respawn → ready
  → preamble.  Start and restart call the same `spawn_and_handshake`.
* Watchdog: no `end` within `interrupt_timeout` after SIGINT ⇒ SIGKILL group,
  restart, mark in-flight and queued cells `Interrupted`.
* `Drop`: SIGTERM, wait, SIGKILL, wait.  No orphaned Python holding notebook
  data.
* One supervisor thread owns the `Child` and stdin.  A reader thread per child
  parses stdout; a second drains stderr.  Events cross a channel with a
  generation number, so late frames from a killed process are ignored.
* At most one cell is written to the driver at a time; the rest wait in the
  supervisor queue.  That is what makes "drop queued cells on interrupt" exact.

M2 implements the runtime without `#[tokio::main]`: winit owns the main thread,
`App` stores a multi-threaded `tokio::runtime::Runtime` and its `Handle`, and
discovery + `start()`/restart run via `Handle::spawn_blocking` so the 90 s
handshake cannot freeze the window.  The supervisor's `emit` calls an opaque
`WakeCallback` that the GUI populates with `egui::Context::request_repaint`;
the startup task also calls it after delivering its result.  `App::update()` is
the sole writer of the notebook.  There is no `Arc<Mutex<Notebook>>` and no
`block_on` inside `update()`.  While a cell is running, a 100 ms repaint keeps
the elapsed label live.

Closing during startup is handled even though the backend temporarily lives in
the blocking task: the backend exposes its shared pgid slot, and `App::drop`
SIGKILLs it if a start task still owns the process.  Normal shutdown drops the
backend first (SIGTERM → wait → SIGKILL) and then calls
`Runtime::shutdown_timeout`.

## 5. Cells and UI (M2, implemented)

**Version pin.** eframe/egui **0.31.1**.  eframe 0.37 replaced `App::update` with
an `App::ui`/root-`Ui` model and removed `TopBottomPanel`; 0.31 matches the
`update()` + panels design this project is built around.  The API used here was
checked against the actual 0.31.1 source, not assumed.

**Editor.** We evaluated `egui_code_editor` and used the spec's fallback:
0.4.2 pins egui 0.36 while this app uses 0.31, and it ships its own highlighter
rather than accepting a caller-supplied `syntect::SyntaxSet`.  The editor is
`TextEdit::multiline` plus a syntect layouter (`SyntaxSet::load_defaults_newlines`
+ `InspiredGitHub`).  Shift+Enter is consumed with
`input.consume_key(Modifiers::SHIFT, Key::Enter)` before the TextEdit is drawn;
a headless egui test synthesizes the key and asserts a Run action.  Bare Enter
still inserts a newline.

**Stable identity.** The outer `ScrollArea` has an `id_salt`; each cell is
inside `ui.push_id(cell.id, …)`; the editor has an explicit
`Id::new(("sagenote-cell-editor", uuid))`.  `focus_request` is applied after
add/move/delete/open/new via `response.request_focus()`.

**State.** `CellRuntime` holds the transient state (queued/running/ok/error/
interrupted, elapsed, live outputs, error) and the notebook keeps the persistent
`Cell.outputs` / `state`.  `sync_outputs()` projects runtime → `.sagenote` at save
time.  Streams are persisted as `text/plain`; tracebacks are persisted as
`text/plain` because `.sagenote` v1 has no error kind.  Saved outputs are
stale-tagged on load, on source edit, and on backend restart/death.  A ready
backend represents a fresh globals namespace, so settled Ok/Error cells go back
to Idle while interrupted cells keep that badge.

**Routing.** `App` keeps `active: Option<(CellId, RunId)>`.  Streams route to
the active cell only if `runtime.run` still matches; results/errors/end are
checked against `runtime.run`.  A deleted cell has no runtime entry, so its
output is discarded; an edited running cell has `run = None` and becomes
superseded.  End clears `active` even for a superseded run.

**Workspace.** Run / insert below / move up / move down / delete per cell, plus
a bottom add; menus and quick buttons for Run All / Run Above / Interrupt.  Run
All/Above skip empty cells.  The supervisor queues at most one active cell, and
the UI shows queued ones.  Outputs are light grey with a soft separator between
cells.  Text/stderr/tracebacks and the M3 math/SVG/PNG pipeline share the
panel; HTML/3D are explicit scope-limit placeholders.

**Files and shortcuts.** New/Open/Save/Save As via rfd (xdg-portal/Wayland on
Linux, AppKit on macOS).  Atomic save; title bar `sagenote — name *`.  Window
close, menu Quit, ⌘/Ctrl+Q and ⌘/Ctrl+W all funnel through one unsaved-changes
modal (Save/Discard/Cancel).  All shortcuts use `Modifiers::COMMAND`:
Shift+Enter; N/O/S/Shift+S; `.` interrupt; I inspector.  ⌘Q/⌘W are intercepted
because egui does not provide them.

**Startup/recovery.** A spinner + "Starting Sage…" is drawn while the blocking
task runs.  Discovery failure and driver exit/fatal present a **native rfd
message dialog** (AppKit on macOS; zenity on Linux) with all five attempts in
the description and custom Retry / Browse / Keep-editing buttons.  Retry
restarts the existing supervisor rather than rediscovering.  Browse validation
runs in the background discovery task; it never blocks the eframe update thread.
The in-window "Sage is not running" banner remains the fallback if the native
provider is missing.  File errors use a native OK dialog.  Menus are in-window egui; `muda`
is the future native-menu path.

**M4 styling.** Menu/status bars use an off-white surface with a hairline border;
cell cards are 8 px rounded with a 12 px inset and a blue border while running;
state is a tinted pill.  The About dialog reports M4 and the shortcut set.

## 6. Math rendering (M3, implemented)

* Native RaTeX only: `ratex-types`, `ratex-parser`, `ratex-layout` and
  `ratex-katex-fonts` are pinned to `=0.1.15`.  `ratex-parser` is an additional
  direct dependency because `layout()` consumes `ParseNode`s.  Never
  `ratex-wasm`; no KaTeX, MathJax or web renderer.
* Pipeline: `parse → layout → validate_layout → to_display_list`.  The
  DisplayList is rasterized with **tiny-skia**; glyph outlines are read from the
  embedded KaTeX TTFs with `ttf-parser`.  Upload is
  `ColorImage::from_rgba_premultiplied`; tiny-skia bytes are premultiplied and
  the paint uses `SimpleSRGB` so the texture is sRGB, not linear.
* Glyphs absent from KaTeX (CJK, emoji, unusual symbols) try a system face from
  `fontdb::Database::load_system_fonts()`.  `RATEX_UNICODE_FONT`, if set, is
  loaded into that database before rendering.  If no face owns the outline, the
  whole formula uses the source fallback.
* Cache key `(latex hash, style, color, px_size * pixels_per_point)` with an
  LRU of 64 textures.  `Renderer::clear()` runs on cell delete and on
  new/open, dropping handles immediately.  Rendered images are fitted to the
  cell width at draw time, not cached at that width.
* Inline vs display comes from the driver's `display` field:
  `MathStyle::Display` at 19 pt for block, `MathStyle::Text` at 15 pt for
  inline (inside `horizontal_wrapped`).
* Every render call has a fallback: LaTeX source in monospace plus a red
  "unrendered" tag (hover shows the reason).  RaTeX is 0.1.x, so parse/layout
  is wrapped in `catch_unwind`; resource limits, missing glyphs, unsupported
  commands and textures over 2048 px per side all use the same fallback.  Never
  blank.
* Keep RaTeX behind `src/render.rs::Renderer` so a version bump is one file.

## 7. Outputs (M3, implemented)

* `text/latex` → RaTeX + tiny-skia as above.
* `image/svg+xml` → `usvg::Tree::from_str` (with the shared system fontdb),
  then `resvg::render(&tree, transform, &mut pixmap.as_mut())` into a
  `tiny_skia::Pixmap` → egui texture.  Natural SVG size is displayed; the raster
  scale follows `pixels_per_point` and is capped at 2048 px per side.
* `image/png` → base64 decode (the driver's `_image_bundle` format) →
  `image::load_from_memory` → RGBA8 → unmultiplied sRGB texture.  Images larger
  than 2048 px are downscaled; display width is capped at 720 pt.
* `text/plain`, stderr and errors are monospaced; errors are red with the full
  traceback.  ANSI CSI/OSC escapes are stripped at display time by
  `src/text.rs`; the notebook preserves the original bytes.
* `text/html` and 3D (`OutputScene*`, `Graphics3d`) remain explicit
  "interactive 3D plot not supported in this prototype" placeholders.  Do not
  attempt a browser fallback.
* Saved outputs are stale on load: math is rasterized in grey, SVG/PNG receive
  a grey tint, text is grey, and every output has a "stale" tag.  Do not re-run
  the notebook on open.



## 8. Notebook format

`.sagenote`, JSON, serde, `format_version: 1` from the first commit:

```json
{"format_version":1,
 "meta":{"created":"...","sage_version":"..."},
 "cells":[{"id":"<uuid>","source":"...",
           "outputs":[{"mime":"text/latex","data":"...","display":"block","stale":false}],
           "state":"ok"}]}
```

* Atomic save: temp file in the destination directory, fsync, `fs::rename`
  (atomic within one filesystem on both targets).
* Dirty flag, close confirm, "unsaved changes" in the title bar.
* Loading sets `stale = true` on every output and normalises transient
  `queued`/`running`/`interrupted` states to `idle`.  The `stale` flag is in the
  format so the decision is reversible.
* `NotebookCodec` trait (`decode`/`encode`): `SagenoteCodec` implemented; `IpynbCodec` returns a
  clear "not implemented in this prototype" error.  Hooks only.

## 9. Inspector (M4, implemented)

* A resizable right `SidePanel` ("Variables"), visible by default, toggled by
  View → Inspector or Ctrl/⌘+I, closable with its ✕.  Width range 220–420 px;
  egui remembers the width by panel id.
* One `introspect` request per refresh.  The driver answers with
  `set(_user_ns) - _baseline`; Sage's own globals are excluded.  Values use
  `reprlib.Repr(maxstring=60, maxother=60, maxlist=20, maxdict=20, maxlevel=3)`
  and a 4000-char cap; the driver caps at 1000 names.
* `App` tracks `IntrospectId` and at most one in-flight request.  Responses
  with a different id are logged and ignored.  Auto-refresh runs every 2 s only
  while the panel is visible, and is suppressed while any cell is
  queued/running.  A manual ↻ is disabled while loading/running/unavailable.
* The panel shows a status line (starting/loading/paused/error/count/updated/auto
  2 s), then name/type/repr rows in a vertical scroll area.  The empty state is
  shown only after a successful read; it explains that assignments in a cell are
  what populate the panel.
* "Running" includes the backend's active eval even after the user edits or
  deletes that cell: `App.active` is consulted, not just the visible cell
  states.  That keeps auto-refresh, Interrupt and New/Open consistent.
* Backend start/restart/death clears the list and in-flight id.  Opening or
  creating a notebook does **not** clear it: the inspector describes the live
  Sage session, which survives notebook file changes.
* The inspector is read-only; no value editing.


## 10. Platforms

Linux (x11 + wayland) is the supported platform; macOS (Apple Silicon) is
best effort — it compiles and passes CI, but has not been runtime-tested on
real hardware.  Windows is unsupported.  Everything below is verified on Linux
with real Sage 10.10, through native zenity error dialogs and xdg-portal file
dialogs.

Both targets are Unix: signals, process groups and atomic rename are shared.
Divergences live in `platform.rs` with `cfg`, not a `trait Platform`:

* driver location: `std::env::temp_dir()/sagenote-<pid>/driver.py`
  (`$XDG_RUNTIME_DIR` does not exist on macOS)
* `rfd`: target-scoped `xdg-portal` on Linux; AppKit panels on macOS.  GTK may
  be omitted if xdg-portal is used.  rfd's Linux **message** dialog is implemented
  through `zenity`; native error dialogs require it (the in-window banner is the
  fallback).
* shortcut modifiers: `Modifiers::COMMAND` everywhere, no branching.
* `cargo-bundle` `Info.plist` must set `NSHighResolutionCapable` to `YES`, or
  the whole UI renders at 1× while the terminal binary looks fine.
* wgpu resolves to Metal on M1.  Confirm once with `RUST_LOG=wgpu=info`; no
  silent GL fallback.
* M3 uses `fontdb` system fonts for SVG text (matplotlib labels) and for CJK/
  emoji glyphs absent from KaTeX.  A minimal Linux install needs DejaVu/Noto
  fonts for labels; if no face has a CJK outline the formula falls back to
  source.  `RATEX_UNICODE_FONT` is loaded into the same database when set.

Linux packages: `pkg-config libxkbcommon-dev libwayland-dev libxcb1-dev
libx11-dev mesa-vulkan-drivers libvulkan-dev` (GTK only if rfd switches to
gtk3).  Recommended fonts: `fonts-dejavu-core fonts-noto-cjk`.  Install
`zenity` for native message dialogs.
macOS: `xcode-select --install`.

## 11. Testing

`cargo test` runs without Sage or a display:

* protocol serde round-trips for every frame, unknown-frame tolerance
* notebook codec, stale-on-load, format version, atomic save, cell moves
* discovery version parsing/natural sort, validation by execution
* supervisor integration against `tests/fixtures/fake_driver.py`:
  ready/start/stream/result/end, error/end, SIGINT recovery, watchdog restart,
  mid-cell death, queued-cell drop, introspection, empty completion
* headless egui passes: the cell workspace, menu bar and status bar render via
  `egui::Context::run`; a synthesized Shift+Enter reaches the app and queues a
  Run; saved outputs are stale in the runtime projection
* M3 rendering: RaTeX `x^2 + \frac{1}{2}` produces non-empty premultiplied
  pixels; invalid LaTeX returns the source fallback; a rectangle SVG and a 1×1
  PNG decode; the texture cache returns the same handle; mixed rich outputs
  render through a headless egui pass; ANSI CSI/OSC stripping is unit-tested
* manual M3: a real Sage 10.10 `plot(sin(x), ...)` SVG was rendered with the
  same usvg/resvg/fontdb path; labels require the system font database
* M4: the inspector panel renders headlessly with variables and with an empty
  loading state; age formatting is unit-tested; the menu/status bar include the
  Inspector entries; 22 lib + 24 app/UI/render + 9 fixture = 55 tests
* manual M4: the live app auto-refreshed the inspector at 2 s intervals against
  Sage 10.10 (increasing `IntrospectId`s); the native discovery-failure dialog
  opened through zenity with Retry / Browse / Keep-editing labels

The fixture is spawned with `DriverLaunch::Python`; the real path adds
`-python -u`.  CI matrix: `macos-14` + `ubuntu-22.04`, native builds only.

## 12. Scope boundaries

No LSP/real autocompletion (trait + empty event only), no 3D/HTML rendering
(placeholder), no Windows, no Jupyter kernel backend, no ipynb round-trip, no
bundled Sage.  The seams are named above; leave them.
