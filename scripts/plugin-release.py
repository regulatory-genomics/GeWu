#!/usr/bin/env python3
"""Build a deterministic GeWu plugin package from a release descriptor."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import subprocess
import zipfile


def safe_relative(value: str) -> PurePosixPath:
    path = PurePosixPath(value)
    if path.is_absolute() or not path.parts or any(part in ("", ".", "..") for part in path.parts):
        raise ValueError(f"path is not safely relative: {value!r}")
    if "\\" in value or "\x00" in value:
        raise ValueError(f"path contains an unsafe character: {value!r}")
    return path


def git_commit(root: Path) -> str:
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()


def git_dirty(root: Path) -> bool:
    return bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root, text=True).strip())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def collect_tree(directory: Path, base: PurePosixPath, selected: dict[str, Path]) -> None:
    for path in sorted(directory.rglob("*")):
        if path.is_dir():
            continue
        if path.is_symlink():
            raise ValueError(f"package contains a symlink: {path}")
        relative = base / PurePosixPath(path.relative_to(directory).as_posix())
        if "__pycache__" in relative.parts or relative.suffix == ".pyc":
            raise ValueError(f"package contains generated Python bytecode: {relative}")
        selected[relative.as_posix()] = path


def files_from_descriptor(source: Path, descriptor: dict, additions: list[tuple[str, Path]]) -> list[tuple[PurePosixPath, Path]]:
    selected: dict[str, Path] = {}
    for value in descriptor["packageFiles"]:
        relative = safe_relative(value)
        raw = source / Path(*relative.parts)
        path = raw.resolve()
        if "__pycache__" in relative.parts or relative.suffix == ".pyc":
            raise ValueError(f"package contains generated Python bytecode: {relative}")
        if raw.is_symlink() or not path.is_file() or not path.is_relative_to(source.resolve()):
            raise ValueError(f"package file is not a regular source file: {value}")
        selected[relative.as_posix()] = path
    for value in descriptor.get("packageDirs", []):
        relative = safe_relative(value)
        directory = (source / Path(*relative.parts)).resolve()
        if not directory.is_dir() or directory.is_symlink() or not directory.is_relative_to(source.resolve()):
            raise ValueError(f"package directory is invalid: {value}")
        collect_tree(directory, relative, selected)
    for destination, raw_path in additions:
        destination_path = safe_relative(destination)
        path = raw_path.resolve()
        if not path.exists():
            raise ValueError(f"generated input is missing: {path}")
        if path.is_dir():
            collect_tree(path, destination_path, selected)
        elif path.is_symlink():
            raise ValueError(f"generated input is a symlink: {path}")
        else:
            selected[destination_path.as_posix()] = path
    return sorted(dict((name, (PurePosixPath(name), path)) for name, path in selected.items()).values())


def write_zip(destination: Path, files: list[tuple[PurePosixPath, Path]], generated: dict[str, bytes]) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        entries = [(name, path.read_bytes(), path.stat().st_mode) for name, path in files]
        entries.extend((PurePosixPath(name), content, 0o100644) for name, content in generated.items())
        for name, content, mode in sorted(entries, key=lambda item: item[0].as_posix()):
            info = zipfile.ZipInfo(name.as_posix(), date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = ((mode & 0o777) | 0o100000) << 16
            archive.writestr(info, content)
            records.append({"path": name.as_posix(), "sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content)})
    return records


def parse_addition(value: str) -> tuple[str, Path]:
    destination, separator, source = value.partition("=")
    if not separator:
        raise ValueError(f"expected DEST=SOURCE, got {value!r}")
    return destination, Path(source)


def build(
    descriptor_path: Path,
    source_root: Path,
    sdk_root: Path | None,
    output: Path,
    allow_dirty: bool,
    additions: list[tuple[str, Path]] | None = None,
) -> dict:
    descriptor = json.loads(descriptor_path.read_text())
    validate_descriptor(descriptor)
    source_root = source_root.resolve()
    output = output.resolve()
    if output.exists():
        raise ValueError(f"output already exists: {output}")
    if output.is_relative_to(source_root):
        raise ValueError("output must be outside the source tree")
    if not allow_dirty and git_dirty(source_root):
        raise ValueError("source tree is dirty; use --allow-dirty only for local inspection")
    commit = git_commit(source_root)
    output.mkdir(parents=True)
    files = files_from_descriptor(source_root, descriptor, additions or [])
    if sdk_root is not None:
        sdk_file = sdk_root.resolve() / descriptor.get("sdkFile", "gewu_plugin.py")
        if not sdk_file.is_file() or sdk_file.is_symlink():
            raise ValueError(f"SDK file is missing: {sdk_file}")
        files.append((PurePosixPath(descriptor.get("sdkDestination", "lib/gewu_plugin.py")), sdk_file))
    files = sorted(dict((name.as_posix(), (name, path)) for name, path in files).values())
    metadata = {
        "schemaVersion": "gewu.release-artifact/v1",
        "plugin": descriptor["plugin"],
        "version": descriptor["version"],
        "apiVersion": "gewu.plugin/v1",
        "sourceCommit": commit,
        "files": [],
    }
    generated = {"compatibility.json": json.dumps({"apiVersion": "gewu.plugin/v1", "plugin": descriptor["plugin"], "version": descriptor["version"], "sourceCommit": commit}, sort_keys=True, indent=2).encode() + b"\n"}
    archive_path = output / f"{descriptor['plugin']}-{descriptor['version']}.package.zip"
    metadata["files"] = write_zip(archive_path, files, generated)
    metadata["package"] = {"path": archive_path.name, "sha256": sha256(archive_path), "bytes": archive_path.stat().st_size}
    metadata["provenance"] = {"builder": "gewu-plugin-release", "source": str(source_root.name), "sourceCommit": commit, "buildInputs": descriptor.get("buildInputs", [])}
    (output / f"{descriptor['plugin']}-{descriptor['version']}.package.json").write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n")
    return metadata


def validate_descriptor(descriptor: dict) -> None:
    required = {"plugin", "version", "packageFiles"}
    missing = required - descriptor.keys()
    if missing:
        raise ValueError(f"descriptor is missing: {', '.join(sorted(missing))}")
    if not isinstance(descriptor["plugin"], str) or not descriptor["plugin"] or "/" in descriptor["plugin"]:
        raise ValueError("plugin must be a non-empty identifier")
    if not isinstance(descriptor["packageFiles"], list):
        raise ValueError("packageFiles must be an array")
    for value in descriptor["packageFiles"] + descriptor.get("packageDirs", []):
        safe_relative(value)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--descriptor", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--sdk-root", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--allow-dirty", action="store_true")
    parser.add_argument("--add-file", action="append", default=[], metavar="DEST=SOURCE")
    parser.add_argument("--add-dir", action="append", default=[], metavar="DEST=SOURCE")
    args = parser.parse_args()
    try:
        additions = [parse_addition(value) for value in args.add_file + args.add_dir]
        metadata = build(args.descriptor, args.source_root, args.sdk_root, args.out, args.allow_dirty, additions)
    except (OSError, subprocess.CalledProcessError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
