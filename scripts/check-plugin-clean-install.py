#!/usr/bin/env python3
"""Check signed marketplace artifacts on Linux in a network namespace with no uplink.

Input is the publisher's test mirror, plus one installed operation per plugin.
The controller needs Python and bubblewrap; the GeWu process inherits no builder environment.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
from urllib.parse import unquote, urlsplit


def mirror_server(root, port=48874):
    """Serve one signed mirror with byte ranges; loopback-only by construction."""
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def _target(self):
            path = unquote(urlsplit(self.path).path).lstrip("/")
            target = (root / path).resolve()
            if target != root and root not in target.parents:
                return None
            return target if target.is_file() else None

        def do_GET(self):
            target = self._target()
            if target is None:
                self.send_error(404)
                return
            size = target.stat().st_size
            start, end = 0, size - 1
            header = self.headers.get("Range")
            if header:
                match = re.fullmatch(r"bytes=(\d+)-(\d*)", header)
                if not match:
                    self.send_error(416)
                    return
                start = int(match[1])
                end = int(match[2]) if match[2] else size - 1
                if start > end or end >= size:
                    self.send_error(416)
                    return
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            else:
                self.send_response(200)
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            with target.open("rb") as stream:
                stream.seek(start)
                remaining = end - start + 1
                while remaining:
                    chunk = stream.read(min(1024 * 1024, remaining))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    remaining -= len(chunk)

    for attempt in range(40):
        try:
            server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
            break
        except OSError as error:
            if error.errno != 98 or attempt == 39:
                raise
            time.sleep(0.25)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gewu", required=True, type=Path)
    parser.add_argument("--mirror", required=True, type=Path)
    parser.add_argument("--public-key", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--plugin", action="append")
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--linux-rootfs", type=Path, help="Test inside an extracted clean Linux userspace")
    parser.add_argument("--download-first", action="store_true",
                        help="Download from a loopback Range mirror, then test with it stopped")
    parser.add_argument("--isolated", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.download_first and args.linux_rootfs:
        parser.error("--linux-rootfs uses an isolated netns; stage the signed cache there or run --download-first on the host")
    if not args.isolated and not args.linux_rootfs:
        return subprocess.call(["bwrap", "--unshare-net", "--die-with-parent", "--bind", "/", "/",
                                "--dev-bind", "/dev", "/dev", "--proc", "/proc",
                                sys.executable, __file__, *sys.argv[1:], "--isolated"])
    if args.linux_rootfs and args.isolated:
        args.linux_rootfs = None
    if not args.linux_rootfs and [name for _, name in socket.if_nameindex()] != ["lo"]:
        parser.error("acceptance requires a network namespace with only loopback")
    entries = json.loads(args.manifest.read_text())
    if not isinstance(entries, list) or not entries or any(
        not isinstance(e, dict) or not isinstance(e.get("id"), str)
        or not re.fullmatch(r"[a-z][a-z0-9.-]{0,127}", e["id"]) for e in entries
    ):
        parser.error("expected a nonempty list of plugin entries")
    ids = {e["id"] for e in entries}
    if len(ids) != len(entries) or not set(args.plugin or ()) <= ids:
        parser.error("duplicate or unknown plugin ID")
    if args.linux_rootfs:
        # bwrap cannot create a mount point inside its read-only root bind.
        gewu_mount = args.linux_rootfs / "usr/bin/gewu"
        gewu_mount.parent.mkdir(parents=True, exist_ok=True)
        gewu_mount.touch(exist_ok=True)
    for entry in entries:
        if args.plugin and entry["id"] not in args.plugin:
            continue
        if args.linux_rootfs:
            (args.linux_rootfs / args.out.resolve().relative_to("/") / entry["id"]).mkdir(
                parents=True, exist_ok=True)
        artifact = entry["packageSha256"]
        runtime = entry["runtimeSha256"]
        if not all(re.fullmatch(r"[0-9a-f]{64}", s) for s in (artifact, runtime)):
            parser.error("invalid artifact SHA-256")
        out = args.out.resolve() / entry["id"]
        home = out / "home 空格"
        workspace = out / "workspace 空格"
        home.mkdir(parents=True)
        shutil.copytree(args.manifest.resolve().parent / entry["fixture"], workspace)
        # Stage the two original downloads and signed catalog; the host performs all verification.
        cache = home / ".gewu/marketplace/official"
        cache.mkdir(parents=True)
        store = home / ".gewu/store/sha256"
        store.mkdir(parents=True)
        if not args.download_first:
            for name in ("index.v1.json", "index.v1.json.sig"):
                shutil.copyfile(args.mirror / name, cache / name)
            for digest in (artifact, runtime):
                files = list((args.mirror / "releases/blobs/sha256" / digest).glob("*.zip"))
                if len(files) != 1:
                    parser.error(f"expected one ZIP for {digest}")
                shutil.copyfile(files[0], store / digest)
        path = out / "bin"
        path.mkdir()
        for name in ("python", "python3", "node", "uv", "pixi", "micromamba", "npm", "ghc"):
            executable = path / name
            executable.write_text("#!/bin/sh\necho unexpected host executable: \"$0\" >&2\nexit 99\n")
            executable.chmod(0o755)
        env = {"PATH": str(path) + ":/usr/bin:/bin", "HOME": str(home), "LANG": "C.UTF-8",
               "GEWU_MARKETPLACE_PUBLIC_KEY": args.public_key.read_text().strip()}
        server = mirror_server(args.mirror.resolve()) if args.download_first else None
        if server:
            env["GEWU_MARKETPLACE_BASE_URL"] = f"http://127.0.0.1:{server.server_port}"
        gewu = str(args.gewu.resolve())
        start = time.monotonic()
        with (out / "acceptance.log").open("w") as log:
            def run(*argv):
                command = [gewu, "plugin", *argv]
                if args.linux_rootfs:
                    command = ["/usr/bin/bwrap", "--unshare-net", "--die-with-parent",
                               "--ro-bind", str(args.linux_rootfs.resolve()), "/",
                               "--dev", "/dev", "--proc", "/proc", "--bind", "/tmp", "/tmp",
                               "--tmpfs", "/dev/shm",
                               "--bind", str(out), str(out), "--ro-bind", gewu, "/usr/bin/gewu",
                               "--chdir", str(workspace), "/usr/bin/gewu", "plugin", *argv]
                subprocess.run(command, cwd=workspace, env=env,
                               stdout=log, stderr=subprocess.STDOUT, check=True, timeout=1200)
            try:
                run("marketplace-install", entry["id"], "--channel", entry.get("channel", "beta"),
                    "--version", entry["version"], *(() if args.download_first else ("--offline",)))
            finally:
                if server:
                    server.shutdown()
                    server.server_close()
                    env.pop("GEWU_MARKETPLACE_BASE_URL", None)
            check = ["check", entry["id"], "--probe", "--workspace", str(workspace), "--session", "acceptance"]
            if "resource" in entry:
                check += ["--resource", entry["resource"], "--resource-type", entry["resourceType"]]
            run(*check)
        (out / "result.json").write_text(json.dumps({"id": entry["id"], "version": entry["version"],
            "seconds": round(time.monotonic() - start, 2), "network": "isolated", "packageSha256": artifact,
            "runtimeSha256": runtime}, indent=2) + "\n")
        print(f"PASS: {entry['id']} ({out})", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
