#!/usr/bin/env python3
"""Validate a GeWu adapter with an ordered send/expect scenario (no LLM)."""

import argparse
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time

MAX_FRAME = 1024 * 1024
RESULT = {
    "activate": "Activated", "call": "ToolOutput", "cancel": "Cancelled",
    "resolve": "Source", "deactivate": "Deactivated",
}


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def substitute(value, variables):
    if isinstance(value, dict):
        return {k: substitute(v, variables) for k, v in value.items()}
    if isinstance(value, list):
        return [substitute(v, variables) for v in value]
    return variables.get(value, value) if isinstance(value, str) else value


def matches(actual, expected):
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(k in actual and matches(actual[k], v) for k, v in expected.items())
    if isinstance(expected, list):
        return isinstance(actual, list) and len(actual) == len(expected) and all(matches(a, b) for a, b in zip(actual, expected))
    return type(actual) is type(expected) and actual == expected


class Pipe:
    def __init__(self, stream, timeout):
        self.fd = stream.fileno()
        self.timeout = timeout
        self.buffer = bytearray()

    def read_frame(self):
        deadline = time.monotonic() + self.timeout
        with selectors.DefaultSelector() as selector:
            selector.register(self.fd, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                if len(self.buffer) > MAX_FRAME:
                    raise ValueError("adapter frame exceeds 1 MiB")
                if not selector.select(max(0, deadline - time.monotonic())):
                    raise TimeoutError("timed out waiting for adapter stdout")
                chunk = os.read(self.fd, 8192)
                if not chunk:
                    if self.buffer:
                        raise ValueError("unterminated adapter frame")
                    return None
                self.buffer.extend(chunk)
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = rest
        if len(line) > MAX_FRAME:
            raise ValueError("adapter frame exceeds 1 MiB")
        return json.loads(line.decode("utf-8"))


def write_frame(stream, frame, timeout):
    data = memoryview(json.dumps(frame, allow_nan=False, separators=(",", ":")).encode() + b"\n")
    if len(data) > MAX_FRAME + 1:
        raise ValueError("host frame exceeds 1 MiB")
    deadline = time.monotonic() + timeout
    with selectors.DefaultSelector() as selector:
        selector.register(stream.fileno(), selectors.EVENT_WRITE)
        while data:
            if not selector.select(max(0, deadline - time.monotonic())):
                raise TimeoutError("timed out writing adapter stdin")
            try:
                data = data[os.write(stream.fileno(), data):]
            except BlockingIOError:
                continue


def run(args):
    try:
        from jsonschema import Draft202012Validator
    except ImportError as exc:
        raise ValueError("install jsonschema (Python 3.11+) to run conformance") from exc

    schema = read_json(args.schema_root / "plugin.v1.schema.json")
    Draft202012Validator.check_schema(schema)

    def validate(definition, value):
        validator = Draft202012Validator({**schema, "$ref": f"#/$defs/{definition}"})
        error = next(validator.iter_errors(value), None)
        if error:
            raise ValueError(f"invalid {definition} at {error.json_path} ({error.validator})")

    package = args.package.resolve(strict=True)
    workspace = args.workspace.resolve(strict=True)
    manifest = read_json(package / "plugin.json")
    validate("Manifest", manifest)
    settings = read_json(args.settings)
    steps = substitute(read_json(args.scenario), {
        "${workspace}": str(workspace), "${package}": str(package), "${settings}": settings,
    })
    backend = manifest["backend"]["config"]
    entry = (package / backend["entry"]).resolve(strict=True)
    if not entry.is_relative_to(package) or not entry.is_file():
        raise ValueError("adapter entry must be a file inside the package")
    if not workspace.is_dir() or not isinstance(steps, list) or not steps:
        raise ValueError("workspace must be a directory and scenario a non-empty step array")

    pending = {}
    with subprocess.Popen([str(entry), *backend.get("args", [])], cwd=workspace,
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          start_new_session=True) as process:
        os.set_blocking(process.stdin.fileno(), False)
        reader = Pipe(process.stdout, args.timeout)
        try:
            shutdown_ok = False
            for i, step in enumerate(steps, 1):
                try:
                    if set(step) == {"send"}:
                        frame = step["send"]
                        validate("HostFrame", frame)
                        if "op" in frame:
                            if frame["id"] in pending:
                                raise ValueError("duplicate pending request id")
                            pending[frame["id"]] = frame
                        write_frame(process.stdin, frame, args.timeout)
                    elif set(step) == {"expect"}:
                        frame = reader.read_frame()
                        validate("AdapterFrame", frame)
                        request = pending.get(frame["id"])
                        if request is None:
                            raise ValueError("unsolicited or repeated response")
                        if frame["type"] == "event":
                            event = frame["event"]
                            if event["type"] == "progress" and (request["op"] != "call" or event["callId"] != request["params"]["context"]["callId"]):
                                raise ValueError("progress belongs to another call")
                        else:
                            if frame["type"] == "result":
                                validate(RESULT[request["op"]], frame["result"])
                                shutdown_ok |= request["op"] == "deactivate"
                            del pending[frame["id"]]
                        if not matches(frame, step["expect"]):
                            raise ValueError("adapter response did not match expected fields")
                    else:
                        raise ValueError("scenario step must contain exactly one of send or expect")
                except (KeyError, TypeError, ValueError, OSError) as exc:
                    raise ValueError(f"step {i}: {exc}") from exc
            if pending or not shutdown_ok:
                raise ValueError("scenario must settle all requests and confirm deactivate")
            process.stdin.close()
            if reader.read_frame() is not None or process.wait(timeout=args.timeout) != 0:
                raise ValueError("adapter emitted an extra frame or failed to exit cleanly")
        except BaseException:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            raise

    print(f"PASS: {manifest['id']} ({len(steps)} steps)")


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog="Exit 0 on pass, 1 on failed conformance, 2 on usage errors.")
    for name in ("package", "settings", "workspace", "scenario", "schema-root"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=10, help="seconds per I/O step (default: 10)")
    args = parser.parse_args()
    if not 0 < args.timeout < float("inf"):
        parser.error("--timeout must be positive and finite")
    try:
        run(args)
    except (ValueError, TypeError, OSError, subprocess.TimeoutExpired) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
