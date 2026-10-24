# sagenote

Rust-native SageMath notebook — eframe + egui, wxMaxima-flavoured. Linux (x11/wayland) and
macOS arm64. A cell is source plus typed outputs.

`docs/DESIGN.md` is the authority on protocol, layout, output types and platforms. Read the
section you are about to touch before touching it; this file only carries what the code
cannot show you.

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
- rfd's Linux file dialogs use xdg-portal, but its **message** dialogs use `zenity`.
  Native error dialogs need zenity on the desktop; the in-window banner is the fallback.
- The inspector is read-only and sends one `introspect` at a time.  Never run user code
  from the inspector or refresh it while a cell is queued/running.
- Editing or deleting the running cell clears its visible state but the backend eval is
  still active.  `App::any_running` includes `App.active` for exactly this reason; do not
  reduce it to a scan of cell states.

## Current milestone: M4, inspector/native errors/styling (complete)

M1–M3 are complete; M4 adds:

- right-side **Variables** inspector: default visible, resizable, View menu or
  Ctrl/⌘+I, close button; driver `introspect` baseline, reprlib caps, one
  `IntrospectId` in flight, stale responses ignored
- manual refresh and auto-refresh every 2 s only while visible; suppressed while
  any cell is queued/running; 500 ms repaint while visible
- list rows name/type/repr with loading/paused/error/empty states; list cleared
  on backend start/restart/death but not on notebook open/new
- native rfd `MessageDialog` errors: AppKit on macOS, zenity on Linux; custom
  Retry / Browse for sage… / Keep editing for recovery, OK for file errors;
  in-window banner fallback; no native Quit button
- styling pass: off-white menu/status surfaces, rounded cell cards, blue running
  border, tinted state pills, M4 About/shortcut text
- M3 remains: RaTeX 0.1.15 → tiny-skia, usvg/resvg SVG, base64 PNG, ANSI, LRU

Verification: `cargo test --all-targets` (22 lib + 24 app/UI/render + 9 fixture
integration = 55), `cargo clippy --all-targets -- -D warnings`, fmt clean.
Live Sage 10.10: inspector auto-refreshed at 2 s intervals with increasing
`IntrospectId`s; the native discovery-failure dialog opened through zenity with
the custom labels. The GUI was not runtime-tested on macOS hardware; CI
builds/tests the fixture path.

All four planned milestones are complete.  Do not start new scope without a
direct request.

## Out of scope until asked

Jupyter kernel backend, ipynb round-trip, 3D/HTML plots, autocompletion. Each already has its
stub site named in `docs/DESIGN.md`; leave the seam, skip the feature.

`CLAUDE.md` is a symlink to this file — edit this one.
