#!/usr/bin/env python3
"""sagenote's stdio driver.

Spawned as `sage -python -u driver.py` by the Rust supervisor.  One JSON frame
per line on a dup'd fd 1; stdin is a stream of `{"op": ...}` requests.

Why not drive the interactive Sage REPL over a pipe?  The REPL is IPython-based:
over a non-tty it prints a banner, `In [n]:` prompt echo, ANSI colour and `...`
continuation prompts; Python block-buffers stdout; `%display latex` is global
IPython magic state the user can turn off; and pasting a multi-line block needs
continuation handling.  A JSON line, by contrast, cannot contain a raw newline,
so framing is unambiguous and user output can never be mistaken for a frame.
The `===SAGE_OUTPUT_BEGIN_<nonce>===` sentinel is kept only as a documented
debug fallback for a hypothetical raw-REPL path, with base64 payloads.  This
file is that replacement: exec/eval in a private namespace and emit typed data.

Prototype boundary: SIGINT interrupts the whole `sage -python` process group.
Only the currently executing cell can be aborted; the Rust side drops queued
cells and marks them Interrupted.  Per-cell cancellation needs a Jupyter kernel
(`interrupt_request`), which is why `InterruptScope` exists.
"""

import ast
import base64
import inspect
import io
import json
import os
import reprlib
import rlcompleter
import sys
import traceback

# Framing fd.  Do this before importing Sage: import-time C-level writes to fd 1
# must never be mistaken for protocol lines.  Python-level print() is captured
# separately with _CappedBuffer below.
_FD = os.dup(1)
_DEVNULL = os.open(os.devnull, os.O_WRONLY)
os.dup2(_DEVNULL, 1)
os.close(_DEVNULL)

from sage.all import *

_baseline = set(globals())
# The set is created after the assignment, so `_baseline` itself is not in it.
_baseline.add("_baseline")

# The user gets a copy of the Sage globals, so a stray `_send = ...` in a cell
# cannot corrupt the framing helpers.  Everything the inspector should not list
# is in _baseline by construction.
_user_ns = dict(globals())

# The driver itself lives in a per-pid temp directory, so Python's sys.path[0]
# is that directory.  Notebook cells should import from the launch directory.
try:
    sys.path.insert(0, os.getcwd())
except Exception:
    pass

_PAYLOAD_LIMIT = 1_000_000
_STREAM_LIMIT = 1_000_000
_display_mode = "block"
_startup_error = None


def _cap(text, limit=_PAYLOAD_LIMIT):
    """Bound every payload before it crosses the pipe."""
    if not isinstance(text, str):
        text = str(text)
    if len(text) <= limit:
        return text
    omitted = len(text) - limit
    return (
        text[:limit] + "\n[... sagenote driver: truncated %d characters ...]" % omitted
    )


def _write_all(data):
    view = memoryview(data)
    while view:
        try:
            written = os.write(_FD, view)
        except (BrokenPipeError, OSError):
            # Parent is gone; do not spin in a user print loop.
            os._exit(0)
        view = view[written:]


def _send(frame):
    try:
        line = json.dumps(frame, ensure_ascii=False, separators=(",", ":"))
        _write_all((line + "\n").encode("utf-8", "replace"))
    except (TypeError, ValueError):
        # A non-JSON-serialisable payload should become a diagnostic, not kill
        # the session.  (All call sites pass strings; this is a safety net.)
        fallback = {
            "type": frame.get("type", "unknown"),
            "id": frame.get("id"),
            "run": frame.get("run"),
            "traceback": "sagenote: frame serialisation failed",
        }
        _write_all((json.dumps(fallback) + "\n").encode("utf-8"))


class _CappedBuffer(io.StringIO):
    """StringIO that refuses to grow past the stream cap.

    A `while True: print(...)` cell must not allocate 50 MB before the end of
    the cell; this keeps the drain bounded as well.
    """

    def __init__(self, limit):
        super().__init__()
        self._limit = limit
        self._omitted = 0

    def write(self, s):
        if not isinstance(s, str):
            s = str(s)
        room = self._limit - self.tell()
        if room <= 0:
            self._omitted += len(s)
            return len(s)
        if len(s) > room:
            super().write(s[:room])
            self._omitted += len(s) - room
            return len(s)
        return super().write(s)

    def take(self):
        text = self.getvalue()
        self.seek(0)
        self.truncate(0)
        if self._omitted:
            text += (
                "\n[... sagenote driver: truncated %d stream characters ...]"
                % self._omitted
            )
            self._omitted = 0
        return text


_stdout = _CappedBuffer(_STREAM_LIMIT)
_stderr = _CappedBuffer(_STREAM_LIMIT)


def _use_capture_buffers():
    sys.stdout = _stdout
    sys.stderr = _stderr


def _drain_streams():
    for name, buffer in (("stdout", _stdout), ("stderr", _stderr)):
        text = buffer.take()
        if text:
            _send({"type": "stream", "name": name, "text": _cap(text)})


def _version_string():
    try:
        import sage.version as sage_version

        return str(sage_version.version)
    except Exception:
        try:
            return str(version())
        except Exception:
            return "unknown"


def _source_init():
    """Source ~/.sage/init.sage (or $SAGE_STARTUP_FILE) explicitly.

    `sage -python` does not read it; the interactive REPL does.  It is sourced
    once per process, so a restart re-sources it as part of the preamble.
    """
    global _startup_error
    try:
        from sage.env import SAGE_STARTUP_FILE

        if not SAGE_STARTUP_FILE or not os.path.isfile(SAGE_STARTUP_FILE):
            return
        with open(SAGE_STARTUP_FILE, "rb") as handle:
            source = handle.read().decode("utf-8", "replace")
        # init.sage is Sage-flavoured source, not necessarily plain Python.
        exec(compile(_preparse(source), SAGE_STARTUP_FILE, "exec"), _user_ns)
    except Exception:
        _startup_error = traceback.format_exc()


def _setup_rich_output():
    """Advertise the outputs we can encode, then let Sage choose the container.

    `latex()` is tried before this (many Sage objects implement `_latex_` but
    not `_repr_latex_`).  This hook is what produces SVG/PNG for plots.
    """
    try:
        from sage.repl.rich_output import get_display_manager, output_catalog
        from sage.repl.rich_output.backend_doctest import BackendDoctest

        class _SagenoteBackend(BackendDoctest):
            def supported_output(self):
                # Only advertise container types _rich_probe actually handles.
                # BackendDoctest also supports JPG/GIF/PDF/DVI/video, none of
                # which the UI can render; letting Sage choose one would turn a
                # plot into a source fallback.
                names = (
                    "OutputPlainText",
                    "OutputAsciiArt",
                    "OutputUnicodeArt",
                    "OutputLatex",
                    "OutputHtml",
                    "OutputImageSvg",
                    "OutputImagePng",
                    "OutputSceneCanvas3d",
                    "OutputSceneJmol",
                    "OutputSceneWavefront",
                )
                supported = {
                    getattr(output_catalog, name)
                    for name in names
                    if getattr(output_catalog, name, None) is not None
                }
                return supported or set(super().supported_output())

        manager = get_display_manager()
        manager.switch_backend(_SagenoteBackend())
        try:
            # Vector first: SVG scales in the UI without a Retina blur.
            manager.preferences.graphics = "vector"
        except Exception:
            pass
        return manager, output_catalog
    except Exception:
        return None, None


_dm, _oc = _setup_rich_output()


def _latex_probe(obj):
    try:
        value = latex(obj)
    except Exception:
        return None
    if not isinstance(value, str) or not value.strip():
        return None
    return value


def _image_bundle(data, mime):
    # Base64 keeps the line valid JSON.  An oversized image is replaced by a
    # visible text placeholder: a truncated base64 blob is not decodable and
    # would render as a blank/broken image.
    if len(data) > (_PAYLOAD_LIMIT * 3) // 4:
        return {
            "text/plain": "[%s omitted: %d bytes exceeds the 1 MB frame cap]"
            % (mime, len(data))
        }
    return {mime: base64.b64encode(data).decode("ascii")}


def _rich_probe(obj):
    if _dm is None or _oc is None:
        return None
    # Sage's rich machinery is driven by `_rich_repr_`.  Checking first avoids
    # computing `repr` twice for ordinary objects whose `latex()` failed.
    if not hasattr(obj, "_rich_repr_"):
        return None
    try:
        _, rich = _dm._rich_output_formatter(obj, {})
    except Exception:
        return None
    if rich is None:
        return None
    if isinstance(rich, _oc.OutputImagePng):
        return _image_bundle(rich.png.get(), "image/png")
    if isinstance(rich, _oc.OutputImageSvg):
        return {"image/svg+xml": _cap(rich.svg.get_str())}
    if isinstance(rich, _oc.OutputLatex):
        return {"text/latex": _cap(rich.latex.get_str())}
    if isinstance(rich, _oc.OutputHtml):
        return {"text/html": _cap(rich.html.get_str())}
    if isinstance(rich, _oc.OutputAsciiArt):
        return {"text/plain": _cap(rich.ascii_art.get_str())}
    if isinstance(rich, _oc.OutputUnicodeArt):
        return {"text/plain": _cap(rich.unicode_art.get_str())}
    name = type(rich).__name__
    if name.startswith("OutputScene"):
        return {
            "text/plain": _cap(
                "[interactive 3D plot not supported in this prototype: %s]" % name
            )
        }
    if isinstance(rich, _oc.OutputPlainText):
        return {"text/plain": _cap(rich.text.get_str())}
    return None


def _needs_rich_output(obj, latex_text):
    """Sage 10.x `latex(Graphics)` returns a raw matplotlib PGF dump and
    `latex(Graphics3d)` returns the words "Graphics3d Object".  Those are not
    KaTeX math, so for graphical objects the rich-output probe is the useful
    path.  Ordinary mathematical values still take the canonical `latex()`
    route first."""
    if latex_text and (
        "\\begin{pgfpicture}" in latex_text
        or "%% Creator: Matplotlib" in latex_text
        or "Graphics3d" in latex_text
    ):
        return True
    name = type(obj).__name__.lower()
    return any(word in name for word in ("graphics", "plot", "animation"))


def _format_value(obj):
    """Probe order: latex -> rich SVG/PNG -> repr, with the graphics exception
    above.  Never raise."""
    latex_text = _latex_probe(obj)
    if latex_text is not None and not _needs_rich_output(obj, latex_text):
        return {"text/latex": _cap(latex_text)}
    rich = _rich_probe(obj)
    if rich is not None:
        return rich
    if latex_text is not None:
        return {"text/latex": _cap(latex_text)}
    try:
        text = repr(obj)
    except Exception as exc:
        text = "<sagenote: repr() failed: %r>" % (exc,)
    return {"text/plain": _cap(text)}


def _preparse(source):
    try:
        from sage.repl.preparse import preparse as sage_preparse

        return sage_preparse(source)
    except Exception:
        # Keep the driver usable on a Sage too old/new to expose preparse; the
        # frame protocol still works, only Sage syntax sugar is lost.
        return source


def _parse_cell(source):
    """Return (tree, trailing_expression_or_None, suppress_output)."""
    prepared = _preparse(source)
    tree = ast.parse(prepared, filename="<sagenote-cell>", mode="exec")
    trailing = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        trailing = tree.body.pop()
    suppress = source.rstrip().endswith(";")
    return tree, trailing, suppress


def _run(cell_id, run_id, source):
    global _startup_error
    _use_capture_buffers()
    _send({"type": "start", "id": cell_id, "run": run_id})
    value = None
    try:
        tree, trailing, suppress = _parse_cell(source)
        body = compile(tree, "<sagenote-cell>", "exec")
        exec(body, _user_ns)
        if trailing is not None and not suppress:
            expression = ast.Expression(trailing.value)
            ast.fix_missing_locations(expression)
            value = eval(compile(expression, "<sagenote-cell>", "eval"), _user_ns)
        _drain_streams()
        if trailing is not None and not suppress and value is not None:
            bundle = _format_value(value)
            if bundle:
                _send(
                    {
                        "type": "result",
                        "id": cell_id,
                        "run": run_id,
                        "bundle": bundle,
                        "display": _display_mode,
                    }
                )
    except KeyboardInterrupt:
        _drain_streams()
        _send(
            {
                "type": "error",
                "id": cell_id,
                "run": run_id,
                "traceback": _cap(traceback.format_exc()),
            }
        )
    except BaseException:
        _drain_streams()
        _send(
            {
                "type": "error",
                "id": cell_id,
                "run": run_id,
                "traceback": _cap(traceback.format_exc()),
            }
        )
    finally:
        # A user `sys.stdout = file` should not break the next cell's capture.
        _use_capture_buffers()
        _drain_streams()
        _send({"type": "end", "id": cell_id, "run": run_id})


def _is_display_manager(value):
    """Sage's `dm` is machinery set up by init.sage, not a user variable.

    It is a `DisplayManager` and only confuses the inspector/completion, so it
    is filtered from both.  Ordinary user variables of other types are kept.
    """
    return type(value).__name__ == "DisplayManager"


def _introspect(request_id):
    _use_capture_buffers()
    formatter = reprlib.Repr()
    formatter.maxstring = 60
    formatter.maxother = 60
    formatter.maxlist = 20
    formatter.maxdict = 20
    formatter.maxlevel = 3
    variables = []
    for name in sorted(set(_user_ns) - _baseline):
        if name.startswith("__") or name in {"_", "__builtins__"}:
            continue
        try:
            value = _user_ns[name]
            if _is_display_manager(value):
                continue
            type_name = type(value).__name__
            shown = formatter.repr(value)
        except Exception as exc:
            type_name = "?"
            shown = "<sagenote: repr failed: %r>" % (exc,)
        variables.append({"name": name, "type": type_name, "value": _cap(shown, 4000)})
        if len(variables) >= 1000:
            break
    _drain_streams()
    _send({"type": "introspect", "id": request_id, "vars": variables})


def _set_display(message):
    global _display_mode
    mode = message.get("mode", message.get("display", "block"))
    if mode in ("block", "inline"):
        _display_mode = mode
    # `%display latex` is deliberately not touched: this driver emits a typed
    # text/latex bundle, so the UI does not depend on IPython magic state.


def _completion_token(text):
    """The dotted identifier ending at the end of `text`.

    Mirrors `completion_token_start` in src/ui.rs: identifiers may contain
    letters, digits, underscores and dots, so `a.bi` is completed as a whole.
    """
    token = []
    for ch in reversed(text):
        if ch.isalnum() or ch == "_" or ch == ".":
            token.append(ch)
        else:
            break
    return "".join(reversed(token))


def _complete(message):
    """Prefix/attribute completion for one Tab press.

    rlcompleter gives us the IDE feel for free: globals, keywords, Python
    builtins and attributes, with `(` appended to callables.  It is advisory
    and must never raise, so every probe is wrapped.
    """
    cell_id = message.get("id")
    source = message.get("source", "") or ""
    _use_capture_buffers()
    try:
        offset = int(message.get("offset", 0))
    except (TypeError, ValueError):
        offset = 0
    offset = max(0, min(offset, len(source)))
    token = _completion_token(source[:offset])

    items = []
    if token and not token.startswith("."):
        # Hide driver internals and Sage's dunder soup unless the user is
        # explicitly completing a private name.
        private = token.rsplit(".", 1)[-1].startswith("_")
        namespace = {
            name: value
            for name, value in _user_ns.items()
            if (private or not name.startswith("_")) and not _is_display_manager(value)
        }
        try:
            completer = rlcompleter.Completer(namespace)
            state = 0
            while len(items) < 200:
                try:
                    item = completer.complete(token, state)
                except Exception:
                    break
                if item is None:
                    break
                if item and item not in items:
                    items.append(item)
                state += 1
        except Exception:
            items = []

    items.sort()
    # Attribute probing can technically print (a hostile __getattr__); that is
    # not cell output, so discard it rather than leaking it into the next run.
    _stdout.take()
    _stderr.take()
    _send({"type": "complete", "id": cell_id, "offset": offset, "items": items})


def _docs(message):
    """Documentation for the dotted identifier ending at the caret.

    The UI renders the plain docstring; keeping the transport text-only means
    no HTML/WebView is involved.  Resolution and docstring access are wrapped:
    a user object with a hostile descriptor must not break the driver.
    """
    cell_id = message.get("id")
    source = message.get("source", "") or ""
    _use_capture_buffers()
    try:
        offset = int(message.get("offset", 0))
    except (TypeError, ValueError):
        offset = 0
    offset = max(0, min(offset, len(source)))
    token = _completion_token(source[:offset])
    name = token
    type_name = ""
    signature = ""

    if not token:
        text = "Place the caret on a name to see its documentation."
    else:
        try:
            if "." in token:
                obj = eval(token, _user_ns)
            else:
                obj = _user_ns.get(token)
        except Exception as exc:
            text = "Could not resolve `%s`: %r" % (token, exc)
        else:
            if obj is None:
                text = "No documentation found for `%s`." % token
            else:
                type_name = type(obj).__name__
                try:
                    text = inspect.getdoc(obj) or ""
                except Exception:
                    text = ""
                if not text:
                    try:
                        text = "No docstring.\n\nrepr:\n" + repr(obj)
                    except Exception:
                        text = "No documentation found for `%s`." % token
                try:
                    signature = str(inspect.signature(obj))
                except Exception:
                    signature = ""

    _stdout.take()
    _stderr.take()
    _send(
        {
            "type": "doc",
            "id": cell_id,
            "offset": offset,
            "name": name,
            "kind": type_name,
            "signature": signature,
            "text": _cap(text, 40000),
        }
    )


def _dispatch(message):
    op = message.get("op")
    if op == "eval":
        _run(message["id"], message["run"], message.get("source", ""))
    elif op == "introspect":
        _introspect(message["id"])
    elif op == "complete":
        _complete(message)
    elif op == "doc":
        _docs(message)
    elif op == "set_display":
        _set_display(message)
    elif op == "ping":
        _send({"type": "pong"})
    # Unknown ops are ignored on purpose: the Rust side can probe capabilities.


def main():
    _use_capture_buffers()
    _source_init()
    startup_out = _stdout.take()
    startup_err = _stderr.take()
    _send({"type": "ready", "sage_version": _version_string()})
    if startup_out:
        _send({"type": "stream", "name": "stdout", "text": _cap(startup_out)})
    if startup_err:
        _send({"type": "stream", "name": "stderr", "text": _cap(startup_err)})
    if _startup_error:
        _send(
            {
                "type": "stream",
                "name": "stderr",
                "text": _cap("init.sage failed:\n" + _startup_error),
            }
        )

    while True:
        try:
            line = sys.stdin.readline()
        except KeyboardInterrupt:
            # Ctrl-C while idle: the interactive REPL shows a fresh prompt.  Here
            # there is nothing to cancel, so just keep reading.
            continue
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except Exception:
            _send(
                {
                    "type": "stream",
                    "name": "stderr",
                    "text": "[sagenote driver: malformed request line ignored]\n",
                }
            )
            continue
        try:
            _dispatch(message)
        except KeyboardInterrupt:
            # A Ctrl-C between operations must not kill the driver.
            continue
        except Exception:
            _send(
                {
                    "type": "stream",
                    "name": "stderr",
                    "text": _cap(
                        "[sagenote driver: request failed]\n" + traceback.format_exc()
                    ),
                }
            )


if __name__ == "__main__":
    main()
