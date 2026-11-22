#!/usr/bin/env python3
"""Exercise marketplace install failure and lifecycle races against a signed catalog.

The controller needs the RC3 host, a staged catalog, Python and a writable temp
root. It never installs into the caller's HOME.
"""
import argparse
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from urllib.parse import unquote, urlsplit


class FaultMirror:
    def __init__(self, root: Path, mode: str = "normal", target: str | None = None):
        self.root = root
        self.mode = mode
        self.target = target
        self.fired = False
        self.requests: list[str] = []

    def start(self):
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def target_path(self):
                path = unquote(urlsplit(self.path).path).lstrip("/")
                target = (outer.root / path).resolve()
                if outer.root not in target.parents and target != outer.root:
                    return None
                return target if target.is_file() else None

            def do_GET(self):
                target = self.target_path()
                if target is None:
                    self.send_error(404)
                    return
                size = target.stat().st_size
                header = self.headers.get("Range")
                start, end = 0, size - 1
                if header:
                    match = re.fullmatch(r"bytes=(\d+)-(\d*)", header)
                    if not match:
                        self.send_error(416)
                        return
                    start = int(match[1])
                    end = int(match[2]) if match[2] else size - 1
                outer.requests.append(f"{self.path} {header or ''}".strip())
                fault = (
                    outer.mode != "normal"
                    and outer.target is not None
                    and outer.target in self.path
                )
                if fault and outer.mode == "interrupt" and not outer.fired:
                    outer.fired = True
                    self.send_response(206)
                    self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                    self.send_header("Content-Length", str(end - start + 1))
                    self.end_headers()
                    with target.open("rb") as source:
                        source.seek(start)
                        self.wfile.write(source.read(min(1024 * 1024, end - start + 1)))
                    self.wfile.flush()
                    self.close_connection = True
                    self.connection.shutdown(socket.SHUT_WR)
                    return
                self.send_response(206 if header else 200)
                if header:
                    self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                self.send_header("Content-Length", str(end - start + 1))
                self.end_headers()
                with target.open("rb") as source:
                    source.seek(start)
                    remaining = end - start + 1
                    while remaining:
                        chunk = source.read(min(1024 * 1024, remaining))
                        if not chunk:
                            break
                        if fault and outer.mode == "corrupt" and start == 0 and not outer.fired:
                            outer.fired = True
                            chunk = bytes([chunk[0] ^ 1]) + chunk[1:]
                        self.wfile.write(chunk)
                        remaining -= len(chunk)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gewu", type=Path, required=True)
    parser.add_argument("--catalog", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--plugin", default="paper")
    return parser.parse_args()


def env_for(home: Path, catalog: Path, mirror: FaultMirror):
    return {
        "HOME": str(home),
        "PATH": "/usr/bin:/bin",
        "LANG": "C.UTF-8",
        "GEWU_MARKETPLACE_BASE_URL": mirror.url,
        "GEWU_MARKETPLACE_ASSET_ORIGIN": mirror.url,
        "GEWU_MARKETPLACE_PUBLIC_KEY": (catalog / "key.b64").read_text().strip(),
    }


def run_install(args, home, catalog, mirror, version):
    return subprocess.run(
        [str(args.gewu), "plugin", "marketplace-install", args.plugin,
         "--version", version, "--channel", "beta"],
        cwd=home, env=env_for(home, catalog, mirror), capture_output=True, text=True,
        timeout=300,
    )


def state(home):
    path = home / ".gewu/plugins/state.json"
    return json.loads(path.read_text()) if path.exists() else {"plugins": {}}


def main():
    args = parse_args()
    manifest = json.loads((args.catalog / "manifest.json").read_text())
    entry = next(item for item in manifest if item["id"] == args.plugin)
    args.out.mkdir(parents=True, exist_ok=True)
    results = {}

    def record(name, **values):
        results[name] = values
        (args.out / f"{name}.json").write_text(json.dumps(values, indent=2) + "\n")

    with tempfile.TemporaryDirectory(prefix="gewu-faults-", dir="/tmp") as base:
        base = Path(base)
        normal = FaultMirror((args.catalog / "mirror").resolve()).start()
        try:
            home = base / "interrupted"
            home.mkdir()
            fault = FaultMirror((args.catalog / "mirror").resolve(), "interrupt", entry["runtimeSha256"]).start()
            try:
                result = run_install(args, home, args.catalog, fault, entry["version"])
                record("interrupted_resume", exit=result.returncode, fired=fault.fired,
                       active=state(home)["plugins"].get(args.plugin, {}).get("activeGeneration"),
                       stderr=result.stderr[-500:])
            finally:
                fault.close()

            home = base / "corrupt"
            home.mkdir()
            fault = FaultMirror((args.catalog / "mirror").resolve(), "corrupt", entry["runtimeSha256"]).start()
            try:
                result = run_install(args, home, args.catalog, fault, entry["version"])
                record("bad_digest", exit=result.returncode, fired=fault.fired,
                       active=state(home)["plugins"].get(args.plugin, {}).get("activeGeneration"),
                       stderr=result.stderr[-500:])
            finally:
                fault.close()

            same = base / "same-plugin"
            same.mkdir()
            command = [str(args.gewu), "plugin", "marketplace-install", args.plugin,
                       "--version", entry["version"], "--channel", "beta"]
            first = subprocess.Popen(command, cwd=same, env=env_for(same, args.catalog, normal),
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            second = subprocess.Popen(command, cwd=same, env=env_for(same, args.catalog, normal),
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            first_out, first_err = first.communicate(timeout=300)
            second_out, second_err = second.communicate(timeout=300)
            record("concurrent_same_plugin", exits=[first.returncode, second.returncode],
                   active=state(same)["plugins"].get(args.plugin, {}).get("activeGeneration"),
                   errors=[first_err[-300:], second_err[-300:]])

            project = base / "preserve"
            project.mkdir()
            (project / "keep.txt").write_text("keep\n")
            result = run_install(args, project, args.catalog, normal, entry["version"])
            before = (project / "keep.txt").read_text()
            uninstall = subprocess.run(
                [str(args.gewu), "plugin", "uninstall", args.plugin],
                cwd=project, env=env_for(project, args.catalog, normal),
                capture_output=True, text=True, timeout=120,
            )
            record("uninstall_preserves_project", installExit=result.returncode,
                   uninstallExit=uninstall.returncode, file=before,
                   pluginAfter=state(project)["plugins"].get(args.plugin))

            race = base / "race"
            race.mkdir()
            install = subprocess.Popen(command, cwd=race, env=env_for(race, args.catalog, normal),
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            time.sleep(0.4)
            remove = subprocess.run(
                [str(args.gewu), "plugin", "uninstall", args.plugin],
                cwd=race, env=env_for(race, args.catalog, normal),
                capture_output=True, text=True, timeout=60,
            )
            install_out, install_err = install.communicate(timeout=300)
            record("install_uninstall_race", installExit=install.returncode,
                   uninstallExit=remove.returncode,
                   active=state(race)["plugins"].get(args.plugin, {}) if state(race)["plugins"].get(args.plugin) else None,
                   errors=[install_err[-300:], remove.stderr[-300:]])
        finally:
            normal.close()

    (args.out / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
