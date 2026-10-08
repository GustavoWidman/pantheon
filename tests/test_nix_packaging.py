"""Evaluate real cache keys against source mutations, without building packages."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


@unittest.skipUnless(shutil.which("nix"), "Nix evaluator is not installed")
class NixPackagingTests(unittest.TestCase):
    # Evaluate every copied checkout in one process so Nix can share its nixpkgs
    # evaluation. Separate processes for every mutation add a minute to CI.
    mutations = {
        "README.md": "\n# cache-key regression\n",
        "docs/browser.md": "\n# cache-key regression\n",
        "tests/test_browser_worker.py": "\n# cache-key regression\n",
        "scripts/browser-worker.py": "\n# cache-key regression\n",
        "src/lib.rs": "\n// cache-key regression\n",
        "src/behavior.txt": "\ncache-key regression\n",
        "skills/engineering/SKILL.md": "\ncache-key regression\n",
        "tests/mcp_integration.rs": "\n// cache-key regression\n",
        "Cargo.toml": '\n[package.metadata.cache-regression]\nvalue = true\n',
        "Cargo.lock": "\n# cache-key regression\n",
    }

    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="pantheon-nix-source-")
        cls.addClassCleanup(cls.temp.cleanup)
        source = Path(__file__).resolve().parents[1]
        roots = {}
        for index, (relative, suffix) in enumerate({"baseline": "", **cls.mutations}.items()):
            root = Path(cls.temp.name) / f"checkout-{index}"
            shutil.copytree(source, root, ignore=shutil.ignore_patterns(
                ".git", "target", "result", "__pycache__",
            ))
            if relative != "baseline":
                path = root / relative
                path.write_text(path.read_text() + suffix)
            roots[relative] = root
        entries = []
        for relative, root in roots.items():
            entries.append(
                f'{json.dumps(relative)} = let f = builtins.getFlake "{root}"; '
                'p = f.packages.${builtins.currentSystem}.pantheon; '
                'in { rust = p.unwrapped.drvPath; package = p.drvPath; };'
            )
        baseline = roots["baseline"]
        expr = (
            f'let f = builtins.getFlake "{baseline}"; '
            'p = f.packages.${builtins.currentSystem}.pantheon.unwrapped; '
            'in { identities = { ' + " ".join(entries) + ' }; '
            'settings = { build = p.cargoBuildType; check = p.cargoCheckType; '
            'flags = p.cargoTestFlags; doCheck = p.doCheck; postCheck = p.postCheck; }; }'
        )
        result = json.loads(subprocess.check_output(
            ["nix", "eval", "--offline", "--impure", "--json", "--expr", expr],
            text=True, timeout=120,
        ))
        cls.identities = result["identities"]
        cls.settings = result["settings"]

    def test_release_binary_and_all_debug_test_targets_are_retained(self):
        self.assertIn("cargo test --doc --offline --target", self.settings["postCheck"])
        self.assertEqual({k: v for k, v in self.settings.items() if k != "postCheck"}, {
            "build": "release", "check": "debug",
            "flags": ["--all-targets"], "doCheck": True,
        })

    def test_non_rust_inputs_do_not_recompile_rust(self):
        for relative in ("README.md", "docs/browser.md", "tests/test_browser_worker.py"):
            with self.subTest(path=relative):
                self.assertEqual(self.identities["baseline"], self.identities[relative])

    def test_browser_worker_repackages_without_recompiling_rust(self):
        before, after = self.identities["baseline"], self.identities["scripts/browser-worker.py"]
        self.assertEqual(before["rust"], after["rust"])
        self.assertNotEqual(before["package"], after["package"])

    def test_rust_and_embedded_inputs_recompile_rust(self):
        for relative in (
            "src/lib.rs", "src/behavior.txt", "skills/engineering/SKILL.md",
            "tests/mcp_integration.rs", "Cargo.toml", "Cargo.lock",
        ):
            with self.subTest(path=relative):
                before, after = self.identities["baseline"], self.identities[relative]
                self.assertNotEqual(before["rust"], after["rust"])
                self.assertNotEqual(before["package"], after["package"])


if __name__ == "__main__":
    unittest.main()
