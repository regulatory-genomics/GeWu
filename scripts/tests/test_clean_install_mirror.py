import importlib.util
from pathlib import Path
import tempfile
import unittest
import urllib.error
import urllib.request

SCRIPT = Path(__file__).resolve().parents[1] / "check-plugin-clean-install.py"
spec = importlib.util.spec_from_file_location("clean_install", SCRIPT)
clean_install = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clean_install)


class MirrorServerTests(unittest.TestCase):
    def test_serves_ranges_and_refuses_paths_outside_the_mirror(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "artifact.zip").write_bytes(b"0123456789")
            server = clean_install.mirror_server(root, 0)
            base = f"http://127.0.0.1:{server.server_port}"
            try:
                request = urllib.request.Request(base + "/artifact.zip", headers={"Range": "bytes=2-5"})
                with urllib.request.urlopen(request, timeout=5) as response:
                    self.assertEqual(response.status, 206)
                    self.assertEqual(response.headers["Content-Range"], "bytes 2-5/10")
                    self.assertEqual(response.read(), b"2345")
                with self.assertRaises(urllib.error.HTTPError) as error:
                    urllib.request.urlopen(base + "/../etc/passwd", timeout=5)
                self.assertEqual(error.exception.code, 404)
            finally:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
