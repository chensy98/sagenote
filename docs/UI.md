# UI

## Version pin

eframe/egui **0.31.1**. eframe 0.37 replaced `App::update` with an
`App::ui`/root-`Ui` model and removed `TopBottomPanel`; 0.31 matches the
`update()` + panels design this project is built around. The API used here was
checked against the actual 0.31.1 source, not assumed.

## Editor

We evaluated `egui_code_editor` and used the spec's fallback: 0.4.2 pins egui
0.36 while this app uses 0.31, and it ships its own highlighter rather than
accepting a caller-supplied `syntect::SyntaxSet`. The editor is
`TextEdit::multiline` plus a syntect layouter (`SyntaxSet::load_defaults_newlines`
+ `InspiredGitHub`). Shift+Enter is consumed with
`input.consume_key(Modifiers::SHIFT, Key::Enter)` before the TextEdit is drawn;
a headless egui test synthesizes the key and asserts a Run action. Bare Enter
still inserts a newline.

## Stable identity

The outer `ScrollArea` has an `id_salt`; each cell is inside
`ui.push_id(cell.id, …)`; the editor has an explicit
`Id::new(("sagenote-cell-editor", uuid))`. `focus_request` is applied after
add/move/delete/open/new via `response.request_focus()`.

## State

`CellRuntime` holds the transient state (queued/running/ok/error/interrupted,
elapsed, live outputs, error) and the notebook keeps the persistent
`Cell.outputs` / `state`. `sync_outputs()` projects runtime → `.sagenote` at
save time. Streams are persisted as `text/plain`; tracebacks are persisted as
`text/plain` because `.sagenote` v1 has no error kind. Saved outputs are
stale-tagged on load, on source edit, and on backend restart/death. A ready
backend represents a fresh globals namespace, so settled Ok/Error cells go back
to Idle while interrupted cells keep that badge.

## Workspace

Run / insert below / move up / move down / delete per cell, plus a bottom add;
menus and quick buttons for Run All / Run Above / Interrupt. Run All/Above
skip empty cells. The supervisor queues at most one active cell, and the UI
shows queued ones. Outputs are light grey with a soft separator between cells.
Text/stderr/tracebacks and the math/SVG/PNG pipeline share the panel; HTML/3D
are explicit scope-limit placeholders.

## Files and shortcuts

New/Open/Save/Save As via rfd (xdg-portal/Wayland on Linux, AppKit on macOS).
Atomic save; title bar `sagenote — name *` while dirty. Window close, menu
Quit, ⌘/Ctrl+Q and ⌘/Ctrl+W all funnel through one unsaved-changes modal
(Save/Discard/Cancel). All shortcuts use `Modifiers::COMMAND`: Shift+Enter;
N/O/S/Shift+S; `.` interrupt; I inspector. ⌘Q/⌘W are intercepted because egui
does not provide them; otherwise macOS could not quit from the keyboard.

## Startup/recovery

A spinner + "Starting Sage…" is drawn while the blocking task runs. Discovery
failure and driver exit/fatal present a **native rfd message dialog** (AppKit
on macOS; zenity on Linux) with all five attempts in the description and
custom Retry / Browse / Keep-editing buttons. Retry restarts the existing
supervisor rather than rediscovering. Browse validation runs in the background
discovery task; it never blocks the eframe update thread. The in-window "Sage
is not running" banner remains the fallback if the native provider is missing.
File errors use a native OK dialog. Menus are in-window egui; `muda` is the
future native-menu path.

## Styling

Menu/status bars use an off-white surface with a hairline border; cell cards
are 8 px rounded with a 12 px inset and a blue border while running; state is
a tinted pill. The About dialog reports the version and the shortcut set.

## Inspector

A resizable right `SidePanel` ("Variables"), visible by default, toggled by
View → Inspector or Ctrl/⌘+I, closable with its ✕. Width range 220–420 px;
egui remembers the width by panel id.

One `introspect` request per refresh. The driver answers with
`set(_user_ns) - _baseline`; Sage's own globals are excluded. Values use
`reprlib.Repr(maxstring=60, maxother=60, maxlist=20, maxdict=20, maxlevel=3)`
and a 4000-char cap; the driver caps at 1000 names.

`App` tracks `IntrospectId` and at most one in-flight request. Responses with
a different id are logged and ignored. Auto-refresh runs every 2 s only while
the panel is visible, and is suppressed while any cell is queued/running. A
manual ↻ is disabled while loading/running/unavailable.

The panel shows a status line (starting/loading/paused/error/count/updated/auto
2 s), then name/type/repr rows in a vertical scroll area. The empty state is
shown only after a successful read; it explains that assignments in a cell are
what populate the panel.

"Running" includes the backend's active eval even after the user edits or
deletes that cell: `App.active` is consulted, not just the visible cell states.
That keeps auto-refresh, Interrupt and New/Open consistent.

Backend start/restart/death clears the list and in-flight id. Opening or
creating a notebook does **not** clear it: the inspector describes the live
Sage session, which survives notebook file changes.

The inspector is read-only; no value editing.
