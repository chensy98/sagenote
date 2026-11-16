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
+ `InspiredGitHub`, with `base16-ocean.dark` in dark mode). Shift+Enter is
consumed with `input.consume_key(Modifiers::SHIFT, Key::Enter)` before the
TextEdit is drawn; a headless egui test synthesizes the key and asserts a Run
action. Bare Enter still inserts a newline. Tab is consumed the same way for
completion, so it never inserts a literal tab.

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
shows queued ones. Outputs are Gray 10 with a hairline border; cells are
separated by spacing and a square 1 px border, not by coloured pills.
Text/stderr/tracebacks and the math/SVG/PNG pipeline share the panel; HTML/3D
are explicit scope-limit placeholders.

## Files and shortcuts

New/Open/Save/Save As via rfd (xdg-portal/Wayland on Linux, AppKit on macOS).
Atomic save; title bar `sagenote — name *` while dirty. Window close, menu
Quit, ⌘/Ctrl+Q and ⌘/Ctrl+W all funnel through one unsaved-changes modal
(Save/Discard/Cancel). Global shortcuts use `Modifiers::COMMAND`:
N/O/S/Shift+S; `.` interrupt; I inspector. Editor bindings are Shift+Enter
(run), Tab (complete), Ctrl+Space (docs), Ctrl+Enter (run in place), Alt+Enter
(run and insert below), and Ctrl+N/Ctrl+P (next/previous completion candidate).
Cmd+Q/Cmd+W are intercepted because egui does not provide them; otherwise
macOS could not quit from the keyboard.  Settings → Settings… lists every
remappable binding; **Change** records a new one and **Clear** disables it
(shown as `—`).

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

The UI follows **IBM Carbon** (v11 tokens in `src/settings.rs::carbon`). Light
is the default; `Dark`/`System`-dark use the Carbon dark palette. The UI ships
with **IBM Plex Sans Text/Regular/Medium/SemiBold** and **IBM Plex Mono**
embedded, so it looks the same on every machine instead of inheriting egui's
thin Ubuntu-Light default; body copy uses Plex Sans Text (the small-size
optical variant), buttons Plex Medium for clarity, and headings Plex SemiBold.
The default body size is 15 px and code 14 px (Carbon's 14 px scale is a little
small for egui's unhinted grayscale AA at 1×). Surfaces are
white or Gray 10, borders are Gray 20 hairlines, the focus/selection colour is
Blue 60, and corners are square (0 radius) on Carbon's 8 px grid. The window
clear colour and the central panel both use the Carbon panel fill, so no
eframe near-black clear colour shows through in light mode. Buttons are
Carbon-style outlined controls; primary actions (Run, Run All) are Blue 60 and
Interrupt is the danger treatment. Status badges use Carbon tag fills (Blue 10,
Green 10, Red 10, Yellow 10) rather than tinted pills. The code editor switches
syntect themes with the UI theme so dark mode is never dark-on-dark. The About
dialog reports the version and the shortcut set.

## Tab completion

Pressing **Tab** in a focused editor completes the dotted identifier ending at
the caret. A single candidate is inserted directly; several candidates open a
compact Carbon popup below the caret (22 px rows, 13 px monospace). **Up/Down**
or the configurable **Ctrl+N / Ctrl+P** bindings move the selection,
Enter/Tab accept, Esc closes, and a click accepts the clicked row. Any text
change closes the popup, and the next Tab asks the driver again.

Candidates come from the live driver namespace (`rlcompleter`): user variables,
Sage globals, keywords, Python builtins and attributes, with `(` appended to
callables. The token may contain `.` so `a.bi` is replaced by `a.bit_length`
as a whole. The driver hides driver internals, dunders (unless the token starts
with `_`), and `DisplayManager` objects such as the `dm` from `init.sage`.

Because the driver is single-threaded, completion is suppressed while a cell is
queued/running. A response is applied only if the cell source is exactly the
string that was sent; otherwise it is dropped. A request failure becomes a log
line and an empty candidate list, never a session error.

While a candidate list is open it owns the keyboard: global shortcuts are
deferred so Ctrl+N cannot trigger New Notebook before it moves the selection.

## Documentation

**Ctrl+Space** (configurable, and clearable in Settings) asks the driver for the
docstring of the dotted name ending at the caret. The driver resolves the name
and returns `inspect.getdoc` plus the type and signature; the UI opens a
non-modal, resizable **Documentation** window with a signature block, RST-style
section headings, wrapped prose and monospaced code blocks. Escape or the ×
button closes it. No HTML/WebView is involved.

Docs are suppressed while a cell is queued/running and share the completion
source guard: an answer is ignored if the cell changed after the request. A
backend restart/death closes the window. If the resolution fails, the window
still opens with a readable message instead of an error dialog.

## Inspector

A resizable right `SidePanel` ("Variables"), visible by default, toggled by
View → Inspector or Ctrl/⌘+I, closable with its ✕. Width range 220–420 px;
egui remembers the width by panel id.

One `introspect` request per refresh. The driver answers with
`set(_user_ns) - _baseline`; Sage's own globals are excluded. Values use
`reprlib.Repr(maxstring=60, maxother=60, maxlist=20, maxdict=20, maxlevel=3)`
and a 4000-char cap; the driver caps at 1000 names. `DisplayManager` objects
(the `dm` that `init.sage` often creates) are filtered out; ordinary init.sage
variables such as `x`, `y`, `z` are kept.

`App` tracks `IntrospectId` and at most one in-flight request. Responses with
a different id are logged and ignored. Auto-refresh runs every 2 s only while
the panel is visible, and is suppressed while any cell is queued/running. A
manual ↻ is disabled while loading/running/unavailable.

The panel is deliberately concise: **no permanent status line and no
timestamp**.  The list itself is the answer, and a clock that changed on every
auto-refresh only added flicker.  Each variable is one compact block (name +
type on one line, repr below) with spacing rather than per-entry boxes or
separators.  Starting/loading/error/empty are the only state messages shown,
and only when the list has nothing else to say.

"Running" includes the backend's active eval even after the user edits or
deletes that cell: `App.active` is consulted, not just the visible cell states.
That keeps auto-refresh, Interrupt and New/Open consistent.

Backend start/restart/death clears the list and in-flight id. Opening or
creating a notebook does **not** clear it: the inspector describes the live
Sage session, which survives notebook file changes.

The inspector is read-only; no value editing.
