# Temba releases

GitHub Actions builds the client on Linux, macOS and Windows. You can trigger
all builds from your Linux machine; no local cross-compilation is required.

## Install the native binary

Open [GitHub Releases](https://github.com/rememberSoftwares/temba-mcp/releases)
and download the archive for your system, plus `SHA256SUMS` if you want to verify it.
Replace `X.Y.Z` below with the downloaded version.

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `temba-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `temba-X.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `temba-X.Y.Z-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `temba-X.Y.Z-x86_64-pc-windows-msvc.zip` |

Each archive contains one executable (`temba` or `temba.exe`), `LICENSE` and
`THIRD_PARTY_LICENSES`. Keep the legal documents when redistributing the binary;
see [third-party licenses](LICENSES.md). No Python, uv,
Rust compiler or package manager is needed on the user's machine.
Other architectures, including Linux ARM64 and Windows ARM64, are not built yet.

### Linux

From the directory containing the downloaded files:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf temba-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz
sudo install -m 0755 temba /usr/local/bin/temba
temba --version
```

### macOS

Check the archive's SHA-256 against its line in `SHA256SUMS`, then extract it.
For Apple Silicon:

```sh
shasum -a 256 temba-X.Y.Z-aarch64-apple-darwin.tar.gz
tar -xzf temba-X.Y.Z-aarch64-apple-darwin.tar.gz
sudo mkdir -p /usr/local/bin
sudo install -m 0755 temba /usr/local/bin/temba
temba --version
```

Use the `x86_64-apple-darwin` archive on Intel Macs.

### Windows

In PowerShell, compare the archive's SHA-256 with `SHA256SUMS`, then extract it:

```powershell
Get-FileHash .\temba-X.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
Expand-Archive .\temba-X.Y.Z-x86_64-pc-windows-msvc.zip .\temba-download
```

Move `temba-download\temba.exe` to a directory on your user `PATH`.
Reopen your terminal and AI client after changing `PATH`, then run `temba --version`.
You can also use the executable's absolute path directly in your AI configuration.

## Configure and connect

```sh
temba add catalog
temba catalog
```

The assistant saves `~/.temba/temba.yaml` on Linux/macOS, or
`%USERPROFILE%\.temba\temba.yaml` on Windows. Use `--config FILE` or `TEMBA_CONFIG`
to select another file. See [client setup](CLIENT.md#create-and-update-the-configuration).

Your AI client starts Temba directly. For OpenCode:

```json
{
  "mcp": {
    "temba": {
      "type": "local",
      "command": ["temba"],
      "enabled": true
    }
  }
}
```

Running `temba` without a subcommand starts the MCP stdio process and waits for
requests; use `temba catalog` for a terminal listing. `temba --help` explains the modes.

## Update Temba

Download the new native archive and replace the installed executable using the
same installation steps. Then restart the Temba process in your AI client.
Your configuration stays in the home directory; the client command stays `temba`.

## Optional installation with uv

Wheels remain available for users who already use uv. They contain the same Rust
executable as the native archives. Copy the full URL of the wheel for your platform:

```sh
uv tool install 'COPIED_WHEEL_URL'
temba add catalog
```

The uv tools directory must be on `PATH`; `uv tool update-shell` can configure it.
For an ephemeral launch, use `uvx --from 'COPIED_WHEEL_URL' temba`.
PyPI publishing is not configured. Each release includes four native archives,
four optional wheels and `SHA256SUMS`.

## Publish a version

1. Keep the client version identical in `crates/temba-client/Cargo.toml`,
   `pyproject.toml`, and the `temba-client` entry in `Cargo.lock`. After any
   lockfile change, run `cargo fetch --locked` followed by
   `python3 scripts/third_party_licenses.py` and commit `THIRD_PARTY_LICENSES`.
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
