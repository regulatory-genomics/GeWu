"""Import the descriptor validator from the release CLI."""

from pathlib import Path
import runpy

_namespace = runpy.run_path(str(Path(__file__).with_name("plugin-release.py")))
validate_descriptor = _namespace["validate_descriptor"]
