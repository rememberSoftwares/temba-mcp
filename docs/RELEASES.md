# Temba releases

GitHub Actions builds the client on Linux, macOS and Windows. You can trigger
all builds from your Linux machine; no local cross-compilation is required.

## Download and run

Open [GitHub Releases](https://github.com/rememberSoftwares/temba-mcp/releases)
and select your platform:

| Platform | Native archive suffix |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin.tar.gz` |
| macOS Intel | `x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc.zip` |

Each release contains four native archives, four wheels, and `SHA256SUMS`.
The archive and wheel for a platform contain the same executable.
Other architectures, including Linux ARM64 and Windows ARM64, are not built yet.

**With uvx:** copy the link to the compatible `.whl` asset and replace
`COPIED_WHEEL_URL` below with that full HTTPS URL:

```sh
uvx --from 'COPIED_WHEEL_URL' temba --config /absolute/path/to/temba.yaml
```

Or install it once:

```sh
uv tool install 'COPIED_WHEEL_URL'
temba --config /absolute/path/to/temba.yaml
```

Use that installed executable in your AI client as well. The uv tools directory
must be on `PATH`; `uv tool update-shell` can configure it for your terminal.
URL installation does not require PyPI. Keep the complete wheel filename in the
URL; it identifies the version, operating system and architecture.

**Without uv:** extract the native archive and run `./temba --config ./temba.yaml`
on Linux/macOS, or `.\temba.exe --config .\temba.yaml` in Windows PowerShell.
No Rust compiler or Python installation is needed for the native executable.
Use `./temba --version` (or `.\temba.exe --version`) to check the binary without
loading a configuration. Other commands read `temba.yaml` from the current
working directory unless `--config` or `TEMBA_CONFIG` specifies a different file.
The configuration is not included in the native archive; start from
[temba.example.yaml](../temba.example.yaml).

`temba catalog` lists accessible MCP definitions. The local catalog loads
automatically. Running without a subcommand starts the stdio MCP process used by your
AI client, rather than a terminal listing. The startup explanation is written to
stderr; stdout remains reserved for MCP protocol messages. `temba -h` explains
these modes without loading a configuration.

For downloaded assets, Linux users can check the published digests with
`sha256sum --check --ignore-missing SHA256SUMS`; macOS provides `shasum -a 256` and
PowerShell provides `Get-FileHash -Algorithm SHA256`.

## Publish a version

1. Keep the client version identical in `crates/temba-client/Cargo.toml`,
   `pyproject.toml`, and the `temba-client` entry in `Cargo.lock`.
2. Commit and push the source code, including `.github/workflows/ci.yaml`
   and the release packaging scripts. Tag the commit you want to release.
3. Push a tag matching **`X.Y.Z`**. For the current client version:

   ```sh
   git tag 2.3.0
   git push origin 2.3.0
   ```

4. Follow **Actions → Checks, binaries and releases**. After all checks
   and four platform builds succeed, the workflow creates and publishes the
   release with its downloads. The tag must exactly match the client version.

Use a new stable version and tag for each release. Tags with a prefix, such as `chart-1.3.3`, do not
publish a release. The catalog image and PyPI publishing are not part of
this workflow.

The workflow uses GitHub's automatic token, with write permission only in the
publishing job; no separate publishing secret is required. It uploads all assets
to a draft before publishing. If a job fails, fix the cause and rerun it. A draft
can be completed on a retry; an already published release's files are left unchanged.

On ordinary branch pushes and pull requests, the same checks and builds run but
only downloadable Actions artifacts are produced. No release is published.
