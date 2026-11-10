# Third-party licenses

Temba's own code is licensed under Apache-2.0 (`LICENSE`). Rust dependencies
retain their upstream licenses. `THIRD_PARTY_LICENSES` lists every registry
package in `Cargo.lock` and reproduces its published license, copyright, author
and notice files, including nested notices for bundled code. Identical texts
are stored once and referenced by SHA-256. The inventory includes build-time
and platform-specific packages, so not every listed package is linked into
each executable.

The native archives contain the executable, `LICENSE` and
`THIRD_PARTY_LICENSES`. Wheels carry both documents under
`temba_client-<version>.dist-info/licenses/`; the catalog image carries them
under `/usr/share/licenses/temba/`. This document covers Rust dependencies,
not the operating-system packages in the container's base image.

After changing dependencies, regenerate and commit the notices:

```sh
cargo fetch --locked
python3 scripts/third_party_licenses.py
python3 scripts/third_party_licenses.py --check
```

Use Python 3.11 or newer. If Cargo's sources are cached elsewhere, pass
`--registry /path/to/registry/src`. Generation does not download anything.
CI regenerates the inventory from cached sources and rejects differences.
Packaging also checks the lockfile fingerprint and rejects missing or altered
legal documents in wheels and native archives.

Some published crates declare Apache-2.0 but omit the license text from their
source archive. For these reviewed exceptions, the generator selects the
Apache-2.0 option and supplies its standard text from
`scripts/licenses/Apache-2.0.txt`, retaining any author or notice files supplied
by the crate. Each exception is pinned to a package version and exact license
expression. A new exception or changed expression requires review; it must not
be added without checking the upstream licensing terms.
