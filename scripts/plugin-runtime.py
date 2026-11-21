#!/usr/bin/env python3
"""Pack a deterministic GeWu packed-runtime archive from a prepared tree."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import zipfile


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build(root: Path, output: Path) -> dict:
    root = root.resolve()
    output = output.resolve()
    manifest_path = root / "runtime.json"
    if not manifest_path.is_file():
        raise ValueError("runtime tree has no runtime.json")
    manifest = json.loads(manifest_path.read_text())
    for field in ("schemaVersion", "id", "version", "plugin", "target", "launcher", "probe", "capabilities"):
        if field not in manifest:
            raise ValueError(f"runtime.json is missing {field}")
    if output.exists():
        raise ValueError(f"output already exists: {output}")
    files = sorted(path for path in root.rglob("*") if path.is_file())
    for path in files:
        if path.is_symlink():
            raise ValueError(f"runtime contains a symlink: {path}")
    output.mkdir(parents=True)
    archive_path = output / f"{manifest['plugin']}-{manifest['version']}.runtime.zip"
    entries: list[dict[str, object]] = []
    with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in files:
            name = PurePosixPath(path.relative_to(root).as_posix())
            info = zipfile.ZipInfo(name.as_posix(), date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = ((path.stat().st_mode & 0o777) | 0o100000) << 16
            contents = path.read_bytes()
            archive.writestr(info, contents)
            entries.append({"path": name.as_posix(), "sha256": hashlib.sha256(contents).hexdigest(), "bytes": len(contents)})
    metadata = {
        "schemaVersion": "gewu.runtime-artifact/v1",
        "plugin": manifest["plugin"],
        "version": manifest["version"],
        "id": manifest["id"],
        "target": manifest["target"],
        "files": entries,
        "runtime": {"path": archive_path.name, "sha256": sha256(archive_path), "bytes": archive_path.stat().st_size},
        "manifest": {"sha256": sha256(manifest_path), "bytes": manifest_path.stat().st_size},
    }
    (output / f"{manifest['plugin']}-{manifest['version']}.runtime.json").write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n")
    return metadata


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        metadata = build(args.root, args.out)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
