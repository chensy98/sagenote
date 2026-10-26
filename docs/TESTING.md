# Testing

`cargo test` runs without Sage or a display:

- protocol serde round-trips for every frame, unknown-frame tolerance
- notebook codec, stale-on-load, format version, atomic save, cell moves
- discovery version parsing/natural sort, validation by execution
- supervisor integration against `tests/fixtures/fake_driver.py`:
  ready/start/stream/result/end, error/end, SIGINT recovery, watchdog restart,
  mid-cell death, queued-cell drop, introspection, empty completion
- headless egui passes: the cell workspace, menu bar and status bar render via
  `egui::Context::run`; a synthesized Shift+Enter reaches the app and queues a
  Run; saved outputs are stale in the runtime projection
- rendering: RaTeX `x^2 + \frac{1}{2}` produces non-empty premultiplied
  pixels; invalid LaTeX returns the source fallback; a rectangle SVG and a 1×1
  PNG decode; the texture cache returns the same handle; mixed rich outputs
  render through a headless egui pass; ANSI CSI/OSC stripping is unit-tested
- manual: a real Sage 10.10 `plot(sin(x), ...)` SVG was rendered with the
  same usvg/resvg/fontdb path; labels require the system font database
- the inspector panel renders headlessly with variables and with an empty
  loading state; age formatting is unit-tested; the menu/status bar include the
  Inspector entries
- manual: the live app auto-refreshed the inspector at 2 s intervals against
  Sage 10.10 (increasing `IntrospectId`s); the native discovery-failure dialog
  opened through zenity with Retry / Browse / Keep-editing labels

The fixture is spawned with `DriverLaunch::Python`; the real path adds
`-python -u`. CI matrix: `macos-14` + `ubuntu-22.04`, native builds only.

## Running the tests

```sh
cargo test --all-targets              # everything
cargo test --bin sagenote             # app/UI/render tests only
cargo test --test driver_integration # fixture integration only
```

## Test count

55 total: 22 lib + 24 app/UI/render + 9 fixture-integration.
