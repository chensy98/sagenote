# sagenote design

This document is the authority for the protocol, layout, output handling and
platform decisions. Read the section you are about to touch. When code and
this file disagree, fix one deliberately — do not let them drift.

## Non-negotiables

* Every pixel is egui/eframe; every calculation is an external `sage` spawned
  at runtime. No WebView, no JavaScript, no WASM frontend, no bundled Sage.
* The UI follows the IBM Carbon palette in `src/settings.rs` (light by default,
  Carbon dark when the theme is Dark/System-dark): square corners, 8 px grid,
  hairline borders, Blue 60 focus.  Do not reintroduce ad-hoc greys in `ui.rs`.
* `src/backend/` and `src/cells.rs` import no `eframe`, no `egui`, no windowing
  type. CI can therefore run them headless.
* Frames are typed by `type`, not parsed out of user text. Output kinds are
  mimetypes in a bundle, never string sniffing on `repr`.
* The only backend implementation is `StdioDriverBackend`. A future
  `JupyterKernelBackend` (ZeroMQ, `interrupt_request`, per-request `msg_id`)
  must be droppable in without UI changes.

## Topic documents

| Document | Topic |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Layering, module boundaries, threading, event routing |
| [PROTOCOL.md](PROTOCOL.md) | Wire protocol, driver design, discovery, process control |
| [UI.md](UI.md) | Editor, workspace, inspector, native dialogs, styling |
| [RENDERING.md](RENDERING.md) | Math pipeline, SVG/PNG, texture cache, fallbacks |
| [NOTEBOOK_FORMAT.md](NOTEBOOK_FORMAT.md) | `.sagenote` JSON schema, save/load semantics |
| [PLATFORMS.md](PLATFORMS.md) | Linux/macOS differences, building, platform support |
| [TESTING.md](TESTING.md) | Test structure, coverage, how to run |
| [USER_GUIDE.md](USER_GUIDE.md) | End-user guide: cells, running, inspector, saving |

## Scope boundaries

No LSP/server-backed autocompletion (Tab completion is driver-side prefix and
attribute completion only; Ctrl+Space docs is `inspect.getdoc`, not a help
server), no 3D/HTML rendering (placeholder), no Windows, no Jupyter kernel
backend, no ipynb round-trip, no bundled Sage. The seams are named above;
leave them.

Official Sage is the sole Python-side dependency: the driver uses only the
Python standard library and Sage's own preparser, and nothing may require
installing extra packages into the user's Sage environment. The live Sage
namespace is the only source of completion and documentation; static analysis
(LSP over stubs) cannot see session state and is excluded as policy, not
deferred — revisit only if official Sage ever ships a language server itself.
