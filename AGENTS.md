# sagenote

Rust-native SageMath notebook — eframe + egui, wxMaxima-flavoured. Linux (x11/wayland) and
macOS arm64. A cell is source plus typed outputs.

`docs/DESIGN.md` is the index and authority on protocol, layout, output types and
platforms. Read the topic file you are about to touch before touching it; this file only
carries what the code cannot show you.

## Project state

The project is complete and stable.  All features described in `docs/DESIGN.md` are
implemented and tested: the cell workspace with syntax-highlighted editing, RaTeX math
rendering, SVG/PNG image display, the variable inspector, native error dialogs, atomic
notebook persistence, robust process control (SIGINT/watchdog/cleanup), Tab completion,
Ctrl+Space documentation lookup, the IBM Carbon theme, and headless testing without
Sage or a display.

Nothing is in progress.  Do not start new scope without a direct request.

## Invariants

**Every pixel is egui, every calculation is the spawned Sage.** The app draws with egui and
computes by piping to an external `sage` it finds at runtime. It ships without Sage, and it
follows the user's Sage, quirks included.

**The backend does not know the UI exists.** `src/backend/` and `src/cells.rs` import no
`eframe`, no `egui`, no windowing. That keeps `cargo test` and `cargo run --bin sadrive`
runnable headless on both platforms — the only code CI can check, and where the protocol bugs
live. `egui` reaching into the backend is the single change that costs the most to undo.

**Frames are typed, not parsed text.** The wire is newline-delimited JSON, one object per
frame, shaped like a Jupyter message. A new output kind is a new mimetype in the bundle, never
a new string sniff on `repr` output.

## Commands

```
cargo test                      # backend, cells, codecs, headless UI/math/inspector
cargo run --bin sadrive         # headless: spawn driver, eval one cell, print frames
cargo run --release             # the app (default-run = sagenote)
WGPU_BACKEND=gl cargo run --release   # over SSH / llvmpipe
```

`tests/fixtures/fake_driver.py` emits the same frames under plain `python3`, so the parser is
testable with Sage absent — pass it as the driver and `/usr/bin/python3` as the interpreter.

## Known traps

- `Child::kill()` is SIGKILL. Interrupt is `libc::kill(-pgid, SIGINT)`, and the spawned pid
  is the group leader because `sage` is a shell script wrapping Python.
- The driver script is written into `std::env::temp_dir()`. `$XDG_RUNTIME_DIR` does not exist
  on macOS.
- `sage -python` does not source `~/.sage/init.sage`; the driver sources it explicitly and
  re-sources it on restart.
- Outputs saved in a `.sagenote` are stale on load — their globals do not exist in a fresh
  session. Render them greyed and tagged; do not re-run the notebook on open. A backend
  restart/death also stales every live output and returns settled Ok/Error cells to Idle.
- Crate APIs here are genuinely young. `ratex-layout` is 0.1.x and `egui_code_editor` tracks
  egui closely; verify the API you use against the actual source and record the exact
  versions in `Cargo.toml` rather than trusting the spec's guesses.
- The UI embeds IBM Plex in `assets/fonts/` (SIL OFL 1.1). `settings::install_fonts`
  must run once before `apply_with_palette`, which binds `TextStyle`s to the named
  Medium/SemiBold families; an unbound family panics inside epaint. Do not call
  `ctx.set_fonts` from the settings dialog path (it would rebuild the atlas per frame).
- The window clear colour is `App::clear_color` and the central panel is filled with
  `Frame::central_panel`. `Frame::default()` is transparent; using it for the central
  panel exposes eframe's near-black clear colour even in light mode.
- rfd's Linux file dialogs use xdg-portal, but its **message** dialogs use `zenity`.
  Native error dialogs need zenity on the desktop; the in-window banner is the fallback.
- The inspector is read-only and sends one `introspect` at a time.  Never run user code
  from the inspector or refresh it while a cell is queued/running.
- Tab completion is advisory and serialized with evaluation on the single-threaded driver.
  It is suppressed while a cell is queued/running, and a response is discarded unless the
  cell source still matches the request exactly.  Completion must never block or fail the
  session: a failed request is a warning plus an empty candidate list.
- While the completion popup is open it owns the keyboard.  `App::handle_shortcuts`
  returns early, so the configurable Ctrl+N/Ctrl+P (defaults) reach the popup instead of
  New Notebook.  Use the `completion_next`/`completion_prev` bindings, not hard-coded keys.
- Doc lookup (`show_docs`, default Ctrl+Space) runs `inspect.getdoc`/attribute access in
  the driver and has the same single-thread/source-guard rules as completion.  It opens a
  non-modal window; Escape closes it.  Never route docs through the inspector path.
- `DisplayManager` objects are filtered from introspection and completion.  `dm` usually
  comes from `~/.sage/init.sage`, not from Sage itself; do not add a hard-coded `dm` name
  filter, because a user could have a real variable with that name.
- Editing or deleting the running cell clears its visible state but the backend eval is
  still active.  `App::any_running` includes `App.active` for exactly this reason; do not
  reduce it to a scan of cell states.

## Out of scope until asked

Jupyter kernel backend, ipynb round-trip, 3D/HTML plots.  Each already has its stub site
named in `docs/DESIGN.md`; leave the seam, skip the feature.  Autocompletion is now a
driver-side prefix/attribute completer (see `docs/PROTOCOL.md` and `docs/UI.md`), not an
LSP, and is intentionally small.

`CLAUDE.md` is a symlink to this file — edit this one.
