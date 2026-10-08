"""Clipboard cleanup is display-wide, without requiring a live X server."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, call

spec = importlib.util.spec_from_file_location(
    "browser_worker", Path(__file__).resolve().parents[1] / "scripts/browser-worker.py"
)
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class ClipboardCleanupTests(unittest.TestCase):
    def test_drops_all_selection_owners_and_legacy_cut_buffers_before_sync(self):
        windows = worker.XWindows.__new__(worker.XWindows)
        windows.display, windows.root = 100, 200
        windows.x = Mock()
        names = [b"CLIPBOARD", b"PRIMARY", b"SECONDARY"]
        names.extend(f"CUT_BUFFER{index}".encode() for index in range(8))
        windows.x.XInternAtom.side_effect = range(1, len(names) + 1)

        windows.clear_clipboard()

        expected = []
        for atom, name in enumerate(names, 1):
            expected.append(call.XInternAtom(100, name, 0))
            if atom <= 3:
                expected.append(call.XSetSelectionOwner(100, atom, 0, 0))
            else:
                expected.append(call.XDeleteProperty(100, 200, atom))
        expected.append(call.XSync(100, 0))
        self.assertEqual(windows.x.mock_calls, expected)


if __name__ == "__main__":
    unittest.main()
