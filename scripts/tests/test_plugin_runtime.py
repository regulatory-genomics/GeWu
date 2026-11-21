import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).parents[1] / "plugin-runtime.py"
SPEC = importlib.util.spec_from_file_location("gewu_plugin_runtime", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(MODULE)


class PluginRuntimeTests(unittest.TestCase):
    def test_runtime_archive_is_deterministic_and_preserves_modes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "runtime"
            (root / "bin").mkdir(parents=True)
            (root / "runtime.json").write_text(json.dumps({
                "schemaVersion": "gewu.runtime/v1", "id": "demo-runtime", "version": "1.0.0",
                "plugin": "demo", "target": {"os": "linux", "arch": "x86_64", "libc": "glibc"},
                "launcher": "bin/launch", "probe": "bin/probe", "capabilities": [],
            }))
            (root / "bin/launch").write_text("#!/bin/sh\n")
            (root / "bin/probe").write_text("#!/bin/sh\n")
            (root / "bin/launch").chmod(0o755)
            (root / "bin/probe").chmod(0o755)
            first = MODULE.build(root, Path(directory) / "a")
            second = MODULE.build(root, Path(directory) / "b")
            self.assertEqual(first["runtime"]["sha256"], second["runtime"]["sha256"])
            with zipfile.ZipFile(Path(directory) / "a" / first["runtime"]["path"]) as archive:
                info = archive.getinfo("bin/probe")
                self.assertEqual((info.external_attr >> 16) & 0o777, 0o755)


if __name__ == "__main__":
    unittest.main()
