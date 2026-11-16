#!/usr/bin/env python3
"""Canned NDJSON driver for tests.

Speaks exactly the sagenote protocol but needs no Sage.  The Rust supervisor can
spawn it with `DriverLaunch::Python` (or a shell wrapper) so parser, process
control, interrupt and watchdog paths are testable on a Sage-less machine.

Source directives (substring based, deliberately crude):
  latex, svg, png, stream, stderr, raise, hang, ignore-sigint, die, ping
"""

import base64
import json
import os
import signal
import sys
import time

_display = "block"
_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk"
    "+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
)


def send(frame):
    sys.stdout.write(json.dumps(frame, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(cell, run, bundle):
    send(
        {
            "type": "result",
            "id": cell,
            "run": run,
            "bundle": bundle,
            "display": _display,
        }
    )


def evaluate(cell, run, source):
    send({"type": "start", "id": cell, "run": run})
    lower = source.lower()

    if "die" in lower:
        # Simulate the driver dying mid-cell: no error, no end.
        os._exit(9)

    if "ignore-sigint" in lower:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        # Watchdog should SIGKILL the group and restart.
        while True:
            time.sleep(0.05)

    if "hang" in lower:
        try:
            while True:
                time.sleep(0.05)
        except KeyboardInterrupt:
            send(
                {
                    "type": "error",
                    "id": cell,
                    "run": run,
                    "traceback": "KeyboardInterrupt: fake driver interrupted",
                }
            )
            send({"type": "end", "id": cell, "run": run})
            return

    if "raise" in lower:
        send(
            {
                "type": "error",
                "id": cell,
                "run": run,
                "traceback": 'Traceback (most recent call last):\n  File "<fake>", line 1, in <module>\nValueError: fake failure',
            }
        )
        send({"type": "end", "id": cell, "run": run})
        return

    if "stderr" in lower:
        send({"type": "stream", "name": "stderr", "text": "fake stderr line\n"})

    if "stream" in lower or "print" in lower:
        send({"type": "stream", "name": "stdout", "text": "hello from fake driver\n"})

    if "latex" in lower:
        result(cell, run, {"text/latex": r"\frac{1}{2}"})
    elif "svg" in lower:
        result(
            cell,
            run,
            {
                "image/svg+xml": '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>'
            },
        )
    elif "png" in lower:
        result(cell, run, {"image/png": base64.b64encode(_PNG).decode("ascii")})
    elif "html" in lower:
        result(cell, run, {"text/html": "<b>fake html</b>"})
    else:
        result(cell, run, {"text/plain": "fake result: " + source.strip()})

    send({"type": "end", "id": cell, "run": run})


def introspect(request_id):
    send(
        {
            "type": "introspect",
            "id": request_id,
            "vars": [
                {"name": "x", "type": "int", "value": "42"},
                {"name": "name", "type": "str", "value": "'sagenote'"},
            ],
        }
    )


_COMPLETIONS = ["print(", "println(", "property("]


def complete(cell_id, source, offset):
    """Canned prefix completion, enough to exercise Tab end to end."""
    text = source[:offset]
    token = []
    for ch in reversed(text):
        if ch.isalnum() or ch == "_" or ch == ".":
            token.append(ch)
        else:
            break
    token = "".join(reversed(token))
    if token and not token.startswith("."):
        items = [item for item in _COMPLETIONS if item.startswith(token)]
    else:
        items = []
    send({"type": "complete", "id": cell_id, "offset": offset, "items": items})


def docs(cell_id, source, offset):
    """Canned documentation for the name at the caret."""
    text = source[:offset]
    token = []
    for ch in reversed(text):
        if ch.isalnum() or ch == "_" or ch == ".":
            token.append(ch)
        else:
            break
    token = "".join(reversed(token))
    send(
        {
            "type": "doc",
            "id": cell_id,
            "offset": offset,
            "name": token,
            "kind": "function",
            "signature": "print(value, ..., sep=' ', end='\\n')",
            "text": "Print `value` to the text stream.\n\nCanned documentation from the fake driver.",
        }
    )


def main():
    global _display
    send({"type": "ready", "sage_version": "fake-1.0"})
    while True:
        line = sys.stdin.readline()
        if not line:
            return
        try:
            message = json.loads(line)
        except Exception:
            continue
        op = message.get("op")
        if op == "eval":
            evaluate(message["id"], message["run"], message.get("source", ""))
        elif op == "introspect":
            introspect(message["id"])
        elif op == "complete":
            complete(message["id"], message.get("source", ""), message.get("offset", 0))
        elif op == "doc":
            docs(message["id"], message.get("source", ""), message.get("offset", 0))
        elif op == "set_display":
            mode = message.get("mode", "block")
            if mode in ("block", "inline"):
                _display = mode
        elif op == "ping":
            send({"type": "pong"})


if __name__ == "__main__":
    main()
