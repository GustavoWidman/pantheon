import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("version", Path(__file__).parents[1] / ".github/scripts/version.py")
version = importlib.util.module_from_spec(spec)
spec.loader.exec_module(version)


class ReleaseMetadata(unittest.TestCase):
    def test_semver_compares_numerically_and_rejects_invalid_versions(self):
        self.assertGreater(version.semver("0.10.0"), version.semver("0.9.12"))
        for text in ("01.2.3", "1.02.3", "1.2", "1.2.3junk", "1.2.3.4", "1.2.3-rc.1"):
            with self.assertRaises(ValueError):
                version.semver(text)

    def test_conventional_subjects_include_scopes_and_breaking_changes(self):
        for text in ("feat: share browser logins", "fix(browser): retain tab ownership", "feat!: coordinate workers", "ci(release): publish tags"):
            version.conventional(text)
        for text in ("Build Pantheon", "feat: ", "feat(browser) share logins", "feat: add\ncommands"):
            with self.assertRaises(ValueError):
                version.conventional(text)

    def test_cargo_metadata_agrees(self):
        self.assertEqual(len(version.semver(version.current())), 3)
