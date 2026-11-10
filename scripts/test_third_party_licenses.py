"""License inventory safeguards using small, isolated Cargo registry fixtures."""

from pathlib import Path
import tempfile
import unittest

import third_party_licenses as licenses


class LicenseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.registry = self.root / "registry"
        self.lock = []

    def crate(self, name, version="1.0.0", expression="MIT", files=None):
        self.lock.append(f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
                         'source = "registry+https://github.com/rust-lang/crates.io-index"\n')
        (self.root / "Cargo.lock").write_text("\n".join(self.lock))
        directory = self.registry / "index" / f"{name}-{version}"
        directory.mkdir(parents=True)
        (directory / "Cargo.toml").write_text(
            f'[package]\nname = "{name}"\nversion = "{version}"\nlicense = "{expression}"\n')
        for name, content in (files or {}).items():
            destination = directory / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(content)
        return directory

    def test_nested_notices_are_preserved_and_identical_texts_are_shared(self):
        self.crate("example-a", files={"LICENSE": "shared license", "vendor/NOTICE": "vendor attribution"})
        self.crate("example-b", files={"LICENSE-MIT": "shared license", "AUTHORS": "Original Authors"})
        result = licenses.generate(self.root, self.registry)
        self.assertIn("example-a 1.0.0", result)
        self.assertIn("example-b 1.0.0", result)
        self.assertIn("vendor/NOTICE: text", result)
        self.assertIn("vendor attribution", result)
        self.assertIn("Original Authors", result)
        self.assertEqual(result.count("shared license"), 1)
        self.assertEqual(result, licenses.generate(self.root, self.registry))

    def test_missing_source_or_license_text_blocks_generation(self):
        directory = self.crate("example")
        with self.assertRaisesRegex(ValueError, "No upstream license text"):
            licenses.generate(self.root, self.registry)
        (directory / "Cargo.toml").unlink()
        with self.assertRaisesRegex(ValueError, "cargo fetch"):
            licenses.generate(self.root, self.registry)

    def test_reviewed_apache_fallback_is_pinned_to_version_and_expression(self):
        self.crate("rmcp", "3.5.1", "Apache-2.0")
        canonical = self.root / "scripts/licenses/Apache-2.0.txt"
        canonical.parent.mkdir(parents=True)
        canonical.write_text("Canonical Apache license")
        result = licenses.generate(self.root, self.registry)
        self.assertIn("Selected license: Apache-2.0", result)
        self.assertIn("Canonical Apache license", result)
        self.crate("rmcp", "3.5.2", "Apache-2.0")
        with self.assertRaisesRegex(ValueError, "No upstream license text"):
            licenses.generate(self.root, self.registry)

    def test_changed_fallback_expression_requires_review(self):
        self.crate("rmcp", "3.5.1", "Apache-2.0 AND MIT")
        with self.assertRaisesRegex(ValueError, "No upstream license text"):
            licenses.generate(self.root, self.registry)


if __name__ == "__main__":
    unittest.main()
