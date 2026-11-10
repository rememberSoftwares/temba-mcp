"""Release safeguards, archive integrity and the actual Linux wheel packaging path."""
import hashlib
import io
import os
from pathlib import Path
import tarfile
import tempfile
from types import SimpleNamespace
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
        for name, content in release.license_assets().items():
            archive.writestr(f"temba_client-{VERSION}.dist-info/licenses/{name}", content)
    native = directory / f"temba-{VERSION}-{target}{extension}"
    if extension == ".zip":
        with zipfile.ZipFile(native, "w") as archive:
            archive.writestr(executable, binary)
            for name, content in release.license_assets().items():
                archive.writestr(name, content)
    else:
        with tarfile.open(native, "w:gz") as archive:
            member = tarfile.TarInfo(executable)
            member.size = len(binary)
            member.mode = 0o755
            archive.addfile(member, io.BytesIO(binary))
            for name, content in release.license_assets().items():
                member = tarfile.TarInfo(name)
                member.size = len(content)
                member.mode = 0o644
                archive.addfile(member, io.BytesIO(content))
    return wheel, native


class ReleaseTests(unittest.TestCase):
    def test_packaging_includes_licenses_on_every_platform(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            output = directory / "output"
            with patch.object(release.subprocess, "run", return_value=SimpleNamespace(stdout=f"temba {VERSION}\n")):
                for target in release.TARGETS:
                    fixture(directory, target)
                    release.package(directory, output, target, VERSION)
            release.checksums(output, VERSION)

    def test_missing_or_tampered_native_licenses_are_rejected(self):
        for target in ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"]:
            for replacement in [None, b"incorrect notice"]:
                with self.subTest(target=target, replacement=replacement), tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    for platform in release.TARGETS:
                        fixture(directory, platform)
                    name, extension, _, _ = release.TARGETS[target]
                    native = directory / f"temba-{VERSION}-{target}{extension}"
                    contents = {name: f"fixture for {target}".encode(), **release.license_assets()}
                    if replacement is None:
                        del contents["THIRD_PARTY_LICENSES"]
                    else:
                        contents["THIRD_PARTY_LICENSES"] = replacement
                    if extension == ".zip":
                        with zipfile.ZipFile(native, "w") as archive:
                            for filename, content in contents.items():
                                archive.writestr(filename, content)
                    else:
                        with tarfile.open(native, "w:gz") as archive:
                            for filename, content in contents.items():
                                member = tarfile.TarInfo(filename)
                                member.size = len(content)
                                member.mode = 0o755 if filename == name else 0o644
                                archive.addfile(member, io.BytesIO(content))
                    with self.assertRaises(ValueError):
                        release.checksums(directory, VERSION)

    def test_wheel_without_correct_notices_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            target = "x86_64-unknown-linux-gnu"
            wheel, _ = fixture(directory, target)
            with zipfile.ZipFile(wheel) as archive:
                files = {name: archive.read(name) for name in archive.namelist()}
            notice = f"temba_client-{VERSION}.dist-info/licenses/THIRD_PARTY_LICENSES"
            for replacement in [None, b"incorrect notice"]:
                with zipfile.ZipFile(wheel, "w") as archive:
                    for name, content in files.items():
                        if name != notice:
                            archive.writestr(name, content)
                    if replacement is not None:
                        archive.writestr(notice, replacement)
                with self.assertRaises(ValueError):
                    release.executable(wheel, VERSION, "temba")

    def test_stale_license_inventory_blocks_packaging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ["LICENSE", "THIRD_PARTY_LICENSES", "Cargo.lock"]:
                (root / name).write_text("outdated")
            with self.assertRaisesRegex(ValueError, "does not match Cargo.lock"):
                release.license_assets(root)

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
