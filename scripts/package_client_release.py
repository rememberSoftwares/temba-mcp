#!/usr/bin/env python3
"""Validate client versions and package the tested wheel's native executable."""

import argparse
import hashlib
import io
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
from email.parser import BytesParser
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "x86_64-unknown-linux-gnu": ("temba", ".tar.gz", "manylinux", "x86_64"),
    "aarch64-apple-darwin": ("temba", ".tar.gz", "macosx", "arm64"),
    "x86_64-apple-darwin": ("temba", ".tar.gz", "macosx", "x86_64"),
    "x86_64-pc-windows-msvc": ("temba.exe", ".zip", "win", "amd64"),
}


def license_assets(root=ROOT):
    assets = {name: (root / name).read_bytes() for name in ("LICENSE", "THIRD_PARTY_LICENSES")}
    fingerprint = hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest()
    if f"Cargo.lock SHA256: {fingerprint}\n".encode() not in assets["THIRD_PARTY_LICENSES"]:
        raise ValueError("THIRD_PARTY_LICENSES does not match Cargo.lock; regenerate it before packaging")
    return assets


def client_version(root=ROOT):
    project = tomllib.loads((root / "pyproject.toml").read_text())["project"]["version"]
    crate = tomllib.loads((root / "crates/temba-client/Cargo.toml").read_text())["package"]["version"]
    locked = [p["version"] for p in tomllib.loads((root / "Cargo.lock").read_text())["package"] if p["name"] == "temba-client"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", project) or crate != project or locked != [project]:
        raise ValueError("Client versions must match in pyproject.toml, Cargo.toml and Cargo.lock (stable X.Y.Z)")
    tag = os.environ.get("RELEASE_TAG", "")
    if tag and tag != project:
        raise ValueError(
            f"Release tag {tag!r} does not match declared client version {project!r}. "
            "Update pyproject.toml, crates/temba-client/Cargo.toml, Cargo.lock, "
            "chart version and image tag before committing and creating the release tag."
        )
    return project


def select_wheel(directory, target, version):
    _, _, platform, arch = TARGETS[target]
    wheels = [p for p in directory.glob(f"temba_client-{version}-*.whl")
              if any(t.startswith(platform) and t.endswith(f"_{arch}")
                     for t in p.stem.rsplit("-", 1)[1].split("."))]
    if len(wheels) != 1:
        raise ValueError(f"Expected exactly one compatible wheel for {target}; found {len(wheels)}")
    return wheels[0]


def executable(wheel, version, name):
    with zipfile.ZipFile(wheel) as archive:
        metadata = BytesParser().parsebytes(archive.read(f"temba_client-{version}.dist-info/METADATA"))
        if metadata["Name"] != "temba-client" or metadata["Version"] != version:
            raise ValueError("Wheel metadata does not match the client version")
        prefix = f"temba_client-{version}.dist-info/licenses/"
        for filename, content in license_assets().items():
            if archive.namelist().count(prefix + filename) != 1 or archive.read(prefix + filename) != content:
                raise ValueError(f"Wheel is missing or has incorrect {filename}")
        return archive.read(f"temba_client-{version}.data/scripts/{name}")


def package(directory, output, target, version):
    wheel = select_wheel(directory, target, version)
    name, extension, _, _ = TARGETS[target]
    binary = executable(wheel, version, name)
    assets = {name: binary, **license_assets()}
    # Exercise the exact binary that will be distributed, on its native CI runner.
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / name
        path.write_bytes(binary)
        path.chmod(0o755)
        result = subprocess.run([str(path), "--version"], check=True, capture_output=True, text=True)
        if result.stdout.strip() != f"temba {version}":
            raise ValueError("Native executable reports the wrong version")
    output.mkdir(parents=True, exist_ok=True)
    shutil.copy2(wheel, output / wheel.name)
    destination = output / f"temba-{version}-{target}{extension}"
    if extension == ".zip":
        with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for filename, content in assets.items():
                archive.writestr(filename, content)
    else:
        with tarfile.open(destination, "w:gz") as archive:
            for filename, content in assets.items():
                member = tarfile.TarInfo(filename)
                member.size = len(content)
                member.mode = 0o755 if filename == name else 0o644
                archive.addfile(member, io.BytesIO(content))
    print(f"Packaged {wheel.name} and {destination.name}")


def checksums(directory, version):
    expected = set()
    for target, (name, extension, _, _) in TARGETS.items():
        wheel = select_wheel(directory, target, version)
        binary = executable(wheel, version, name)
        assets = {name: binary, **license_assets()}
        native = directory / f"temba-{version}-{target}{extension}"
        if extension == ".zip":
            with zipfile.ZipFile(native) as archive:
                if (sorted(archive.namelist()) != sorted(assets)
                        or any(archive.read(filename) != content for filename, content in assets.items())):
                    raise ValueError("Native archive differs from its wheel")
        else:
            with tarfile.open(native, "r:gz") as archive:
                members = archive.getmembers()
                if (sorted(m.name for m in members) != sorted(assets)
                        or any(not m.isfile() or m.mode != (0o755 if m.name == name else 0o644) for m in members)):
                    raise ValueError("Invalid native archive contents or executable permissions")
                if any(archive.extractfile(filename).read() != content for filename, content in assets.items()):
                    raise ValueError("Native archive differs from its wheel")
        expected.update((wheel.name, native.name))
    actual = {p.name for p in directory.iterdir() if p.name != "SHA256SUMS"}
    if actual != expected:
        raise ValueError("Release must contain exactly four wheels and four native archives")
    lines = []
    for name in sorted(expected):
        with (directory / name).open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        lines.append(f"{digest}  {name}\n")
    (directory / "SHA256SUMS").write_text("".join(lines), encoding="utf-8")
    print("Validated all four platforms; wrote SHA256SUMS")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check-version")
    pack = sub.add_parser("package")
    pack.add_argument("--target", required=True, choices=TARGETS)
    pack.add_argument("--wheel-dir", type=Path, default=Path("dist"))
    pack.add_argument("--output-dir", type=Path, default=Path("release-assets"))
    sums = sub.add_parser("checksums")
    sums.add_argument("--directory", type=Path, default=Path("release-assets"))
    args = parser.parse_args()
    version = client_version()
    if args.command == "check-version":
        print(version)
    elif args.command == "package":
        package(args.wheel_dir, args.output_dir, args.target, version)
    else:
        checksums(args.directory, version)


if __name__ == "__main__":
    main()
