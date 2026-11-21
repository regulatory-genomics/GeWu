import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class AdapterConformance(unittest.TestCase):
    def test_wire_with_real_subprocess(self):
        root = Path(__file__).resolve().parents[2]
        demo = root / "gewu-server/tests/fixtures/plugin-adapter"
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([
                sys.executable, str(root / "scripts/plugin-conformance.py"), "--package", str(demo),
                "--settings", str(demo / "settings.json"), "--scenario", str(demo / "scenario.json"),
                "--workspace", directory, "--schema-root", str(root / "gewu-protocol/schema"),
            ], capture_output=True, text=True, timeout=15,
               env={**os.environ, "PYTHONPATH": str(root / "sdk/python"), "PYTHONDONTWRITEBYTECODE": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PASS: demo (11 steps)", result.stdout)
