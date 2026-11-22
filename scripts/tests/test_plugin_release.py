import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "plugin-release.py"
SPEC = importlib.util.spec_from_file_location("gewu_plugin_release", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(MODULE)


class PluginReleaseTests(unittest.TestCase):
    def test_validator_cli_accepts_safe_descriptors_and_rejects_parent_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            for files, code in [(["plugin.json"], 0), (["../outside"], 2)]:
                path.write_text(json.dumps({"plugin":"demo", "version":"1.0.0", "packageFiles":files}))
                result = subprocess.run([sys.executable, str(SCRIPT.with_name("validate-release.py")), str(path)], capture_output=True, text=True)
                self.assertEqual(result.returncode, code, result.stderr)

    def test_identical_clean_inputs_are_byte_identical_and_dirty_inputs_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "source"
            sdk = Path(directory) / "sdk"
            out_a = Path(directory) / "out-a"
            out_b = Path(directory) / "out-b"
            root.mkdir()
            sdk.mkdir()
            (root / "plugin.json").write_text("plugin")
            (root / "bin").mkdir()
            (root / "bin/adapter").write_text("#!/bin/sh\n")
            (sdk / "gewu_plugin.py").write_text("sdk\n")
            descriptor = {"plugin": "demo", "version": "1.0.0", "packageFiles": ["plugin.json"], "packageDirs": ["bin"]}
            (root / "release.json").write_text(json.dumps(descriptor))
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.name", "test"], cwd=root, check=True)
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "fixture"], cwd=root, check=True)
            first = MODULE.build(root / "release.json", root, sdk, out_a, False)
            second = MODULE.build(root / "release.json", root, sdk, out_b, False)
            self.assertEqual(first["package"]["sha256"], second["package"]["sha256"])
            self.assertEqual((out_a / first["package"]["path"]).read_bytes(), (out_b / second["package"]["path"]).read_bytes())
            (root / "plugin.json").write_text("changed")
            with self.assertRaises(ValueError):
                MODULE.build(root / "release.json", root, sdk, Path(directory) / "dirty", False)

    def test_absolute_and_parent_paths_are_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.validate_descriptor({"plugin": "demo", "version": "1.0.0", "packageFiles": ["../outside"]})
        with self.assertRaises(ValueError):
            MODULE.validate_descriptor({"plugin": "demo", "version": "1.0.0", "packageFiles": ["/absolute"]})


if __name__ == "__main__":
    unittest.main()
