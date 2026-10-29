"""Release safeguards, archive integrity and the actual Linux wheel packaging path."""
import hashlib
import io
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import package_client_release as release

VERSION = "2.2.0"
PLATFORMS = {
    "x86_64-unknown-linux-gnu": "manylinux_2_34_x86_64",
    "aarch64-apple-darwin": "macosx_11_0_arm64",
    "x86_64-apple-darwin": "macosx_10_12_x86_64",
    "x86_64-pc-windows-msvc": "win_amd64",
}


def fixture(directory, target):
    executable, extension, _, _ = release.TARGETS[target]
    binary = f"fixture for {target}".encode()
    wheel = directory / f"temba_client-{VERSION}-py3-none-{PLATFORMS[target]}.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(f"temba_client-{VERSION}.dist-info/METADATA", f"Name: temba-client\nVersion: {VERSION}\n")
        archive.writestr(f"temba_client-{VERSION}.data/scripts/{executable}", binary)
    native = directory / f"temba-{VERSION}-{target}{extension}"
    if extension == ".zip":
        with zipfile.ZipFile(native, "w") as archive:
            archive.writestr(executable, binary)
    else:
        with tarfile.open(native, "w:gz") as archive:
            member = tarfile.TarInfo(executable)
            member.size = len(binary)
            member.mode = 0o755
            archive.addfile(member, io.BytesIO(binary))
    return wheel, native


class ReleaseTests(unittest.TestCase):
    def test_versions_and_tag_must_agree(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "crates/temba-client").mkdir(parents=True)
            (root / "pyproject.toml").write_text(f'[project]\nversion="{VERSION}"')
            (root / "crates/temba-client/Cargo.toml").write_text(f'[package]\nversion="{VERSION}"')
            (root / "Cargo.lock").write_text(f'[[package]]\nname="temba-client"\nversion="{VERSION}"')
            with patch.dict(os.environ, {"RELEASE_TAG": VERSION}):
                self.assertEqual(release.client_version(root), VERSION)
            for tag in ["0.0.1", f"v{VERSION}", f"client-v{VERSION}", f"{VERSION}-rc.1"]:
                with patch.dict(os.environ, {"RELEASE_TAG": tag}):
                    with self.assertRaises(ValueError):
                        release.client_version(root)
            with patch.dict(os.environ, {"RELEASE_TAG": ""}):
                (root / "Cargo.lock").write_text('[[package]]\nname="temba-client"\nversion="0.0.1"')
                with self.assertRaises(ValueError):
                    release.client_version(root)

    def test_all_platforms_are_required_and_checksums_match(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for target in release.TARGETS:
                fixture(directory, target)
            release.checksums(directory, VERSION)
            lines = (directory / "SHA256SUMS").read_text().splitlines()
            self.assertEqual(len(lines), 8)
            for line in lines:
                digest, name = line.split("  ")
                self.assertEqual(digest, hashlib.sha256((directory / name).read_bytes()).hexdigest())
            (directory / f"temba-{VERSION}-x86_64-pc-windows-msvc.zip").unlink()
            with self.assertRaises(FileNotFoundError):
                release.checksums(directory, VERSION)

    def test_native_archive_cannot_differ_from_its_wheel(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for target in release.TARGETS:
                fixture(directory, target)
            with zipfile.ZipFile(directory / f"temba-{VERSION}-x86_64-pc-windows-msvc.zip", "w") as archive:
                archive.writestr("temba.exe", b"different binary")
            with self.assertRaises(ValueError):
                release.checksums(directory, VERSION)

    def test_unexpected_assets_and_ambiguous_wheels_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for target in release.TARGETS:
                fixture(directory, target)
            unexpected = directory / "private-config.yaml"
            unexpected.write_text("not a release asset")
            with self.assertRaises(ValueError):
                release.checksums(directory, VERSION)
            unexpected.unlink()
            (directory / f"temba_client-{VERSION}-py3-none-manylinux_2_28_x86_64.whl").touch()
            with self.assertRaises(ValueError):
                release.select_wheel(directory, "x86_64-unknown-linux-gnu", VERSION)


if __name__ == "__main__":
    unittest.main()
