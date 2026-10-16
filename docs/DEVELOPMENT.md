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
├── .github/workflows/    # Checks and wheel builds
├── Dockerfile            # Central catalog image
├── catalog.yaml          # Example for the catalog binary outside Helm
└── temba.example.toml    # Starting configuration for the local client
```

The workspace requires **Rust 1.99 or later**, as specified in `Cargo.toml`.
Helm can check the chart; `uv` provides the `uvx` launcher used for distribution.

## Checking changes

From the repository root:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
helm lint chart/temba-catalog
python3 scripts/check_chart.py
```

The chart checks require Python 3 and Helm; no Python dependencies are needed.

Tool search runs locally, using BM25 ranking.
The MCP protocol uses stdout; client diagnostics must stay on stderr.

## Building the client

```sh
uvx --from 'maturin>=1.9,<2' maturin build --release --locked --out dist
```

The result is a wheel in `dist/`, containing the native Rust binary.
The backend is configured in [pyproject.toml](../pyproject.toml) with `bindings = "bin"`.
Python handles packaging and launching; the Rust binary handles MCP execution.

[CI](../.github/workflows/ci.yaml) currently builds for:

| System | Architecture |
| --- | --- |
| Linux | x86_64 |
| macOS | Apple Silicon and Intel |
| Windows | x86_64 |

It also checks that the wheels launch with `uvx`, then stores them as artifacts.
Other architectures require an additional build target and wheel.
Publishing to a registry is not configured.

## Releasing a new version

The client and catalog can have different versions.
For the client, keep the version consistent across `crates/temba-client/Cargo.toml`,
`pyproject.toml`, and `Cargo.lock`.

Each release must produce a wheel with a **new name and version**.
Overwriting a wheel at the same path may cause `uvx` to reuse its old cached binary.
Users must update the path in their AI client, then restart
the Temba process actually in use, including when it runs behind a persistent server.

For the catalog, the [deployment guide](DEPLOYMENT.md) covers building
the image and loading it into Minikube. The CI configuration does not publish a catalog image.

## Security and future development

Memory protection limits and credential behavior are described
in [SECURITY_CLIENT.md](../SECURITY_CLIENT.md). The catalog runtime and chart are
described in [SECURITY_CONTAINER.md](../SECURITY_CONTAINER.md).

The [V2 specification](../SPECIFICATION_TEMBA_V2.md) also contains planned developments.
Before documenting an available feature, check the code and current guides.
