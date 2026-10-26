# Architecture

## Layering

```text
sadrive (headless)            app/ui (GUI)                     future
    |                            |                                |
    +-------- EvalBackend -------+--------- JupyterKernelBackend -+
                      |
               StdioDriverBackend (supervisor thread)
                      |  NDJSON over pipes
               sage -python -u driver.py
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

## Module boundaries

| Module | Responsibility |
|---|---|
| `src/backend/mod.rs` | `EvalBackend` trait, `BackendEvent`, ids, mimetypes |
| `src/backend/protocol.rs` | Serde frame/request types |
| `src/backend/driver.rs` | Spawn, SIGINT, watchdog, NDJSON parser |
| `src/cells.rs` | Notebook model, `.sagenote` codec, ipynb hook |
| `src/platform.rs` | Sage discovery, temp dir, macOS branches |
| `src/app.rs` | App state, startup, event routing, files, modals |
| `src/ui.rs` | Menu/status bar, cell workspace, syntect editor |
| `src/render.rs` | RaTeX/tiny-skia, usvg/resvg, PNG, texture cache |
| `src/text.rs` | ANSI stripping for displayed output |
| `src/bin/sadrive.rs` | Headless probe |
| `sage/driver.py` | The Python driver |

`src/backend/` and `src/cells.rs` import no `eframe`, `egui`, or windowing
types. That is what keeps `cargo test` and `cargo run --bin sadrive` runnable
headless on both platforms. `app.rs`/`ui.rs` own every pixel and never reach
into the supervisor's process handles except through `EvalBackend`.

## Threading model

The runtime is implemented without `#[tokio::main]`: winit owns the main
thread, `App` stores a multi-threaded `tokio::runtime::Runtime` and its
`Handle`, and discovery + `start()`/`restart` run via `Handle::spawn_blocking`
so the 90 s handshake cannot freeze the window.

The supervisor's `emit` calls an opaque `WakeCallback` that the GUI populates
with `egui::Context::request_repaint`; the startup task also calls it after
delivering its result.

`App::update()` is the sole writer of the notebook. There is no
`Arc<Mutex<Notebook>>` and no `block_on` inside `update()`. While a cell is
running, a 100 ms repaint keeps the elapsed label live.

### Process control

Both platforms are Unix.

- `Command::process_group(0)`; the spawned pid is the group leader because
  `sage` is a shell script wrapping Python.
- Interrupt: `libc::kill(-pgid, SIGINT)`. `Child::kill()` is SIGKILL and is
  reserved for the watchdog/drop fallback.
- Restart: SIGTERM group → wait `shutdown_grace` → SIGKILL → respawn → ready
  → preamble. Start and restart call the same `spawn_and_handshake`.
- Watchdog: no `end` within `interrupt_timeout` after SIGINT ⇒ SIGKILL group,
  restart, mark in-flight and queued cells `Interrupted`.
- `Drop`: SIGTERM, wait, SIGKILL, wait. No orphaned Python holding notebook
  data.
- One supervisor thread owns the `Child` and stdin. A reader thread per child
  parses stdout; a second drains stderr. Events cross a channel with a
  generation number, so late frames from a killed process are ignored.
- At most one cell is written to the driver at a time; the rest wait in the
  supervisor queue. That is what makes "drop queued cells on interrupt" exact.

### Startup and shutdown

Closing during startup is handled even though the backend temporarily lives in
the blocking task: the backend exposes its shared pgid slot, and `App::drop`
SIGKILLs it if a start task still owns the process. Normal shutdown drops the
backend first (SIGTERM → wait → SIGKILL) and then calls
`Runtime::shutdown_timeout`.

## Event routing

`App` keeps `active: Option<(CellId, RunId)>`. Streams route to the active
cell only if `runtime.run` still matches; results/errors/end are checked
against `runtime.run`. A deleted cell has no runtime entry, so its output is
discarded; an edited running cell has `run = None` and becomes superseded.
End clears `active` even for a superseded run.

Ids:

- `CellId` — UUID string on the wire, stable across save/load and reordering.
- `RunId` — monotonic per evaluation. Results route by `(cell, run)` and are
  discarded if the cell is gone or the run was superseded.
- `IntrospectId` — request id echoed by the `introspect` frame.
