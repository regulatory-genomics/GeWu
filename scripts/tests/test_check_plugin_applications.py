import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-plugin-applications.py"


class ApplicationGate(unittest.TestCase):
    def check(self, entries, *args):
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "manifest.json"
            manifest.write_text(json.dumps(entries))
            return subprocess.run(
                [sys.executable, SCRIPT, manifest, "--validate-only", *args],
                text=True, capture_output=True, check=False,
            )

    def test_legacy_and_new_release_sets(self):
        for ids in ["paper,tachyon,manimble", "gewu-browser,paper,manimble,inkscape,hyperframes"]:
            run = self.check([{"id": id} for id in ids.split(",")], "--require-plugins", ids)
            self.assertEqual(run.returncode, 0, run.stderr)

    def test_duplicate_missing_and_empty_sets_are_rejected(self):
        for ids in [[], ["paper", "paper"], ["manimble"]]:
            run = self.check([{"id": id} for id in ids], "--require-plugins", "paper,manimble")
            self.assertEqual(run.returncode, 2, run.stderr)
