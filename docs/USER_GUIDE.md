# sagenote user guide

This guide explains how to use sagenote from an end-user perspective. For
build instructions see the [README](../README.md); for design decisions see
[DESIGN.md](DESIGN.md).

## Starting the app

```sh
cargo run --release
```

The app discovers your Sage installation automatically (SAGE_BIN, config.toml,
PATH, macOS .app bundles). The first launch can take 10–20 s while Sage starts;
a spinner and "Starting Sage…" indicator are shown during this time.

## The cell workspace

A notebook is a vertical list of cells. Each cell has:

- A **source editor** — syntax-highlighted Sage code, multi-line
- An **output panel** — results, stream output, errors, images
- Per-cell controls — Run, insert below (＋), move up (↑), move down (↓),
  delete (✕)
- A **state pill** — idle, queued, running (with elapsed time), ok, error,
  or interrupted

A "＋ Add cell" button at the bottom appends a new cell.

### Running code

- **Shift+Enter** in a cell evaluates it
- The **Run** button on a cell evaluates that cell
- **Run All** (menu or toolbar) evaluates every non-empty cell in order
- **Run Above** evaluates all cells above the focused one

At most one cell runs at a time; others are queued and shown as "queued".
Run All and Run Above skip empty cells.

### Understanding output

Output appears in a light-grey panel below the cell's editor:

- **Math** — rendered with RaTeX (Rust KaTeX) as typeset mathematics
- **SVG** — rendered with resvg, natural size, capped at 2048 px
- **PNG** — decoded and displayed, capped at 720 pt width
- **Stream** — plain text from `print()`, stderr in orange
- **Errors** — full red traceback

ANSI escape sequences are stripped at display time; the saved output keeps
the original bytes.

### Stale outputs

Outputs saved in a `.sagenote` file are marked **stale** on load — the
globals that produced them do not exist in a fresh Sage session. They are
rendered greyed with a "stale" tag rather than silently re-run.

A backend restart or death also stales every live output and returns settled
Ok/Error cells to Idle.

## The variable inspector

A resizable **Variables** panel on the right side of the window, visible by
default. Toggle it with **View → Inspector** or **Ctrl/⌘+I**; close it with
its ✕ button.

The panel shows:

- Each user-defined variable's **name** (monospace), **Python type**, and
  **repr value** (bounded to 60 chars, depth 3)
- A **status line** — "N variables · updated Xs ago · auto 2 s"
- Loading, paused, error, and empty states as appropriate

The inspector auto-refreshes every 2 s **only while the panel is visible**,
and is suppressed while any cell is queued or running. A manual ↻ button is
available for on-demand refresh.

The inspector is read-only. Backend start/restart/death clears the list;
opening another notebook does **not** clear it, because the inspector
describes the live Sage session, not the file.

## Files and persistence

- **New** (Ctrl/⌘+N) — create a fresh notebook with one empty cell
- **Open…** (Ctrl/⌘+O) — open a `.sagenote` file
- **Save** (Ctrl/⌘+S) — save to the current path (prompts for path if none)
- **Save As…** (Ctrl/⌘+Shift+S) — save to a new path

Saves are atomic: a temp file is written in the destination directory, fsync'd,
then renamed. The title bar shows `sagenote — name *` while there are unsaved
changes.

Window close, menu Quit, ⌘/Ctrl+Q and ⌘/Ctrl+W all funnel through one
unsaved-changes modal (Save / Discard / Cancel).

## Interrupting and restarting

- **Interrupt** (Ctrl/⌘+.) — sends SIGINT to the Sage process group. The
  current cell is aborted with a KeyboardInterrupt error; queued cells are
  dropped and marked Interrupted.
- **Restart Sage** (menu) — kills the Sage process (SIGTERM → wait → SIGKILL)
  and starts a fresh one. All previous outputs become stale.

The watchdog ensures robustness: if no `end` frame arrives within the
interrupt timeout (default 5 s) after SIGINT, the process group is SIGKILLed,
Sage is restarted, and in-flight/queued cells are marked Interrupted.

## Error handling

Errors use native dialogs (zenity on Linux, AppKit on macOS):

- **Recovery errors** (Sage not found, driver crash) show Retry / Browse for
  sage… / Keep editing. Discovery attempts and reasons are in the description.
- **Informational errors** (open/save failures) show a single OK.

The in-window "Sage is not running" banner remains the fallback if no native
message provider is available.

## Keyboard shortcuts

All shortcuts use ⌘ on macOS, Ctrl elsewhere:

| Shortcut | Action |
|---|---|
| Shift+Enter | Evaluate current cell |
| Ctrl/⌘+N | New notebook |
| Ctrl/⌘+O | Open notebook |
| Ctrl/⌘+S | Save notebook |
| Ctrl/⌘+Shift+S | Save notebook as |
| Ctrl/⌘+I | Toggle inspector |
| Ctrl/⌘+. | Interrupt current evaluation |
| Ctrl/⌘+Q | Quit (goes through unsaved-changes modal) |
| Ctrl/⌘+W | Close window (goes through unsaved-changes modal) |

⌘Q/⌘W are intercepted explicitly because egui does not provide them;
otherwise macOS could not quit from the keyboard.

## Platform notes

### Linux

- Install: `pkg-config libxkbcommon-dev libwayland-dev libxcb1-dev libx11-dev
  mesa-vulkan-drivers libvulkan-dev`
- Recommended fonts: `fonts-dejavu-core fonts-noto-cjk` (for SVG labels and
  CJK math fallback)
- Native error dialogs need `zenity`
- File dialogs use xdg-portal (no GTK needed)
- Over SSH or under llvmpipe: `WGPU_BACKEND=gl cargo run --release`

### macOS (Apple Silicon)

- Install: `xcode-select --install`
- wgpu resolves to Metal on M1
- Finder launches inherit only `/usr/bin:/bin:/usr/sbin:/sbin`; when PATH
  discovery fails, the app re-derives it once via `$SHELL -lic 'echo "$PATH"'`

### Windows

Unsupported.
