# Wire protocol

## Frame types

`sage -python -u <driver.py>` (compatibility fallback, below). One JSON object
per line, Jupyter-shaped:

| type | fields |
|---|---|
| `ready` | `sage_version` |
| `start` | `id`, `run` |
| `stream` | `name`: `stdout`/`stderr`, `text` |
| `result` | `id`, `run`, `bundle`, `display`: `block`/`inline` |
| `error` | `id`, `run`, `traceback` |
| `introspect` | `id`, `vars`: `[{name,type,value}]` |
| `complete` | `id`, `offset`, `items`: candidate strings for the token at `offset` |
| `doc` | `id`, `offset`, `name`, `kind`, `signature`, `text`: docstring for the name |
| `end` | `id`, `run` |
| `pong` | auxiliary liveness answer to `ping`; never cell output |

Mimetypes in a bundle: `text/latex`, `image/svg+xml`, `image/png` (base64),
`text/plain`, `text/html` (placeholder/HTML-only). Unknown keys are preserved
by `MimeBundle`; unknown frame types parse to `Frame::Unknown` and do not kill
the session.

Stdin requests: `{"op":"eval"|"introspect"|"complete"|"doc"|"set_display"|"ping"}`.
`set_display` carries `mode`: `block`|`inline`. `complete` and `doc` carry the
cell `id`, the whole cell `source`, and the caret `offset` in characters; the
driver extracts the dotted identifier ending there. `doc` returns the plain
docstring plus name/kind/signature; the UI renders it (no HTML/WebView).

## Framing rationale

A JSON line cannot contain a raw newline, so user data cannot collide with the
delimiter and no sentinel is needed. The raw-REPL sentinel
`===SAGE_OUTPUT_BEGIN_<session_nonce>===` is kept only as a documented debug
fallback; it would base64-encode payloads. It is not used.

## Driver design

### Why not drive the interactive Sage REPL?

The Sage REPL is IPython-based. Over a pipe it emits a banner, `In [n]:` prompt
echo, ANSI colour and `...` continuation prompts; stdout is block-buffered when
not a tty; `%display latex` is IPython-magic global state the user can switch
off; and pasting a multi-line block requires continuation handling.

### What the driver does instead

`sage/driver.py` instead:

- `from sage.all import *`, then snapshots `_baseline = set(globals())`
  immediately, so the inspector sees user variables rather than ~3000 Sage
  builtins. Driver internals are excluded.
- Dups fd 1 for frames and redirects fd 1 to `/dev/null` **before** importing
  Sage, then replaces `sys.stdout`/`sys.stderr` with capped `StringIO`
  buffers drained into `stream` frames. A `print` loop cannot corrupt framing
  or allocate 50 MB.
- Applies `sage.repl.preparse.preparse`, so `^`, `R.<x> =` and friends keep
  Sage semantics.
- Executes the preparsed AST body, then `eval`s a trailing `ast.Expr` (there is
  no display hook; bare `exec` prints nothing). A trailing `;` suppresses
  display, as in the REPL.
- Probes output: `latex(obj)` → Sage rich-output machinery (`_rich_output_formatter`
  / `_rich_repr_`) for SVG/PNG/HTML/scene → `repr(obj)`. Many Sage objects
  define `_latex_` but not `_repr_latex_`, so `latex()` is canonical. A
  graphical object whose `latex()` is a matplotlib PGF dump or
  `Graphics3d Object` is routed to the rich machinery instead; that text is
  not KaTeX math.
- Catches `KeyboardInterrupt` and emits `error` then `end`, never swallowing it.
- Caps every payload at ~1 MB with an explicit marker before it crosses the
  pipe. Oversized images become a visible text placeholder rather than
  truncated base64.
- Sources `$SAGE_STARTUP_FILE` / `~/.sage/init.sage` explicitly, because
  `sage -python` does not.
- Answers `complete` with prefix/attribute candidates from the live namespace
  (`rlcompleter`, capped at 200). Driver internals and Sage dunders are hidden
  unless the user is explicitly completing a private name. Completion is
  advisory: exceptions produce an empty list, never an error frame.
- Answers `doc` with `inspect.getdoc` for the dotted name at the caret, plus
  the object's type and signature. Resolution and docstring access are wrapped;
  a hostile descriptor or missing name produces a normal text answer, never a
  crashed driver. `DisplayManager` objects (the `dm` that `init.sage` often
  creates) are filtered from introspection and completion.

### Sage CLI compatibility

The primary launch is exactly `sage -python -u driver.py`. Sage 10.10's new
`sage.cli` rejects `-python`. If the primary launch exits before `ready`,
`backend/driver.rs` re-executes the driver through Sage's own Python with
`sage -c 'os.execv(sys.executable, [...])'`. This keeps the environment the
Sage wrapper set up, which is required inside the macOS .app framework, and the
launch is cached for restarts. The fallback is a compatibility branch, not a
second supported product.

## Discovery

Order, first **validated** hit wins:

1. `SAGE_BIN`
2. `~/.config/sagenote/config.toml`, key `sage` (alias `sage_bin`), written by
   the picker
3. `PATH`
4. macOS `/Applications/SageMath*.app`, `~/Applications/SageMath*.app`,
   `Contents/Frameworks/Sage.framework/Versions/{Current,*}/local/bin/sage`.
   `Current` may dangle; versioned directories are globbed and sorted
   naturally (10.10 > 9.8).
5. Browse button, which accepts any executable and saves the config.

Validation is always `<candidate> --version`, never `which` or a file test.
The oracle distinguishes "missing" from "broken install" and supplies the
status-bar version. Failures are accumulated as
`DiscoveryFailure { stage, candidate, reason }`; the error dialog shows all
five stages.

macOS Finder launches inherit `/usr/bin:/bin:/usr/sbin:/sbin`. If step 3 fails,
re-derive PATH once via `$SHELL -lic 'echo "$PATH"'`, retry, and splice that
PATH into the driver child's environment. The resolved absolute Sage path is
shown in the status bar. Signed .app cold start is 10–20 s; the ready handshake
allows 90 s and the UI shows a visible "Starting Sage…" state.

## Known limitation

SIGINT on this stdio prototype interrupts the entire `sage -python` process
group, not an individual cell. Sage evaluation is single-threaded and the
driver's `run()` is the only thing on the stack, so an interrupt can only abort
whatever is currently executing; cells queued behind it are dropped by the
client and marked `Interrupted`. Per-cell cancellation arrives with the Jupyter
kernel backend (`interrupt_request`), which is why `interrupt()` already takes
`InterruptScope::{Cell, Kernel}`. Both currently mean "SIGINT the group".
