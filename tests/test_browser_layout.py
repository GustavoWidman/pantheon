import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("browser_worker", Path(__file__).parents[1] / "scripts/browser-worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class BrowserLayout(unittest.TestCase):
    def test_full_hd_clips_and_window_insets_share_one_grid(self):
        # Includes adjacent columns, the next row, and the hidden keeper slot.
        for slot in range(17):
            x, y = worker.cell_origin(slot, 5)
            self.assertEqual((x, y), (slot % 5 * 1920, slot // 5 * 1080))
            self.assertEqual(worker.viewer_clip(slot, 5), f"1920x1080+{x}+{y}")
            wx, wy, width, height = worker.window_geometry(slot, 5)
            self.assertEqual((wx, wy, width, height), (x + 16, y + 16, 1888, 1048))
            self.assertLess(wx + width, x + 1920)
            self.assertLess(wy + height, y + 1080)

    def test_single_column_cells_do_not_overlap(self):
        self.assertEqual(worker.viewer_clip(1, 1), "1920x1080+0+1080")
        self.assertEqual(worker.window_geometry(1, 1), (16, 1096, 1888, 1048))
