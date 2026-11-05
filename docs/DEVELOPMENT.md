# Developing and distributing Temba

## Repository layout

```text
.
├── crates/
│   ├── temba-client/      # stdio MCP server, local search, and OAuth sessions
│   ├── temba-catalog/     # REST API, access control, and definition storage
│   └── temba-core/        # Shared schemas and validation
├── chart/temba-catalog/   # Kubernetes deployment with Helm
├── docs/                 # User and administrator guides
├── .github/workflows/    # Checks, builds and GitHub releases
├── Dockerfile            # Central catalog image
├── catalog.yaml          # Example for the catalog binary outside Helm
└── temba.example.yaml    # Starting configuration for the local client
```

The workspace requires **Rust 1.99 or later**, as specified in `Cargo.toml`.
CI explicitly installs Rust **1.99.1** for checks and every platform build,
including Maturin's Linux container. The shared `RUST_TOOLCHAIN` workflow setting
controls this version; preinstalled runner toolchains are not used.
Helm can check the chart. Users install the native binary; uv is optional.

## Checking changes

From the repository root:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
helm lint chart/temba-catalog
python3 scripts/check_chart.py
python3 -m unittest discover -s scripts -p test_release.py
```

The chart checks require Python 3 and Helm; no Python dependencies are needed.
Gateway integration tests also use Python 3 (standard library only) to run a real
MCP child process. CI installs Python before running the Rust tests.

Tool search runs locally, using BM25 ranking.
The MCP protocol uses stdout; client diagnostics must stay on stderr.

## Building the native binary

```sh
cargo build --release --locked -p temba-client
./target/release/temba --help
```

The executable is `target/release/temba` (`target/release/temba.exe` on Windows).
It runs directly, without a Python launcher. Use an absolute binary path in your
AI client while developing, and rebuild it after source changes.

### Optional wheel packaging

```sh
uvx --from 'maturin>=1.9,<2' maturin build --release --locked --out dist
```

This produces a wheel in `dist/`, using `bindings = "bin"` in
[pyproject.toml](../pyproject.toml). The CI pipeline also extracts its native
executable into the standalone release archive. Python is a packaging dependency,
not a runtime dependency of the native executable.

[CI](../.github/workflows/ci.yaml) currently builds for:

| System | Architecture |
| --- | --- |
| Linux | x86_64 |
| macOS | Apple Silicon and Intel |
| Windows | x86_64 |

It checks that wheels launch with `uvx`, then extracts and tests the same native
executable for `.tar.gz` / `.zip` downloads. Both formats are stored as artifacts.
On a matching `X.Y.Z` tag, all four platforms are verified and published
to a GitHub Release with `SHA256SUMS`. Other architectures require an additional
build target. PyPI publishing is not configured.

The packaging script requires Python 3.11 or later; CI uses Python 3.12.

## Releasing a new version

The client and catalog can have different versions.
For the client, keep the version consistent across `crates/temba-client/Cargo.toml`,
`pyproject.toml`, and `Cargo.lock`.

Each release must have a **new version**. Native users replace their installed
executable and restart the Temba process in their AI client.
For optional wheel testing, rebuilding the same version at the same path can leave
a stale uvx environment; use `uvx --no-cache --from ./dist/your-wheel.whl temba`.

See [publishing and installing Temba releases](RELEASES.md) for the exact tag
commands and download options. The tag must point to committed code containing
the workflow and packaging scripts. Published assets are never overwritten by a retry.

For the catalog, the [deployment guide](DEPLOYMENT.md) covers building
the image and loading it into Minikube. The CI configuration does not publish a catalog image.

## Security and future development

Memory protection limits and credential behavior are described
in [SECURITY_CLIENT.md](../SECURITY_CLIENT.md). The catalog runtime and chart are
described in [SECURITY_CONTAINER.md](../SECURITY_CONTAINER.md).

The [V2 specification](../SPECIFICATION_TEMBA_V2.md) also contains planned developments.
Before documenting an available feature, check the code and current guides.
