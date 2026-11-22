#!/usr/bin/env python3
"""Validate release descriptors without building an artifact."""

import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from plugin_release import validate_descriptor  # type: ignore


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("descriptors", nargs="+", type=Path)
    args = parser.parse_args()
    for path in args.descriptors:
        try:
            validate_descriptor(json.loads(path.read_text()))
        except (OSError, ValueError, json.JSONDecodeError) as error:
            parser.error(f"{path}: {error}")
        print(f"valid\t{path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
