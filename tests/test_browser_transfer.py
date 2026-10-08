import importlib.util
import io
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("browser_worker", Path(__file__).resolve().parents[1] / "scripts/browser-worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class TransferPaths(unittest.TestCase):
    def test_escape_missing_directory_and_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as outside:
            root, outside = Path(root), Path(outside)
            secret = outside / "secret"
            secret.write_bytes(b"secret")
            (root / "alias").symlink_to(secret)
            for path in [secret, root / "alias", root / "missing"]:
                with self.assertRaises((ValueError, FileNotFoundError)):
                    worker.workspace_file(root, str(path))
            with self.assertRaises(ValueError):
                worker.workspace_file(root, str(root / "missing" / "file"), writing=True)
            with self.assertRaises(ValueError):
                worker.workspace_file(root, str(root))

    def test_exclusive_write_and_size_cleanup(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "artifact"
            self.assertEqual(worker.save_artifact(path, io.BytesIO(b"PDF\x00bytes")), 9)
            with self.assertRaises(FileExistsError):
                worker.save_artifact(path, io.BytesIO(b"overwrite"))
            self.assertEqual(path.read_bytes(), b"PDF\x00bytes")
            limit = worker.MAX_TRANSFER_BYTES
            try:
                worker.MAX_TRANSFER_BYTES = 2
                path = Path(root) / "large"
                with self.assertRaises(ValueError):
                    worker.save_artifact(path, io.BytesIO(b"123"))
                self.assertFalse(path.exists())
            finally:
                worker.MAX_TRANSFER_BYTES = limit
