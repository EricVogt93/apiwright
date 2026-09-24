# Release Guide

ApiWright packages are built by `.github/workflows/release.yml`. A release tag
produces the desktop GUI and standalone CLI for x86_64 Linux, x86_64 Windows,
and Apple Silicon macOS, plus `SHA256SUMS.txt`, then publishes them to a GitHub
Release.

## Version source of truth

All workspace crates inherit `[workspace.package].version` from the root
`Cargo.toml`. Release tags must be exactly `v<version>`; for version `0.2.0`,
the only publishable tag is `v0.2.0`. The publish job checks this equality
before creating a release.

Changing the version normally updates:

1. `Cargo.toml` workspace version.
2. `Cargo.lock` package entries via a normal Cargo build/check.
3. User-visible release notes supplied through commits/PRs; GitHub generates
   the final notes from the tag range.

Do not move an existing release tag. Publish a new patch version when a
released artifact needs correction.

## Pre-release gate

Run the same checks as repository CI from a clean working tree:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo check --release --locked -p forge-gui --bin apiwright-ide
cargo check --release --locked -p forge-cli --bin apiwright
git status --short
```

Also open the GUI and run the offline demo for changes touching packaging,
startup, updates or execution:

```sh
cargo run --release -p forge-gui --bin apiwright-ide
cargo run -p forge-cli -- ci requests \
  --root examples/demo-workspace --env demo --mock --allow-project-code
```

## Trigger and workflow behavior

```sh
git tag v0.2.0
git push origin v0.2.0
```

Tags matching `v*` start three package jobs in parallel. The publish job runs
only for a tag and waits for every platform. `workflow_dispatch` is useful for
testing package jobs, but intentionally uploads Actions artifacts without
creating a GitHub Release.

| Job | Runner | GUI output | CLI output |
| --- | --- | --- | --- |
| Linux | Ubuntu 22.04 | `ApiWright-<version>-linux-x86_64.AppImage` | `ApiWright-<version>-cli-linux-x86_64` |
| Windows | Windows 2025 | `ApiWright-<version>-windows-x86_64.exe` | `ApiWright-<version>-cli-windows-x86_64.exe` |
| macOS | macOS 15 | `ApiWright-<version>-macOS-arm64.dmg` | `ApiWright-<version>-cli-macOS-arm64` |

The final job downloads all three artifacts into one directory, runs
`sha256sum * > SHA256SUMS.txt`, and publishes all six platform files and the
checksum manifest with generated release notes.

## Platform packaging

Linux packaging uses `packaging/linux/build-appimage.sh`. It performs a locked
release build, stages the executable, desktop entry, icon, AppStream metadata
and README in an AppDir, then calls `linuxdeploy`. Set `LINUXDEPLOY` to an
already downloaded executable to avoid the script's default continuous-build
download.

```sh
./packaging/linux/build-appimage.sh 0.2.0
```

macOS packaging uses `packaging/macos/build-dmg.sh`. It creates `ApiWright.app`,
sets the plist version, generates an ICNS from the project logo, applies an
ad-hoc signature and creates a compressed DMG containing an Applications
shortcut.

```sh
./packaging/macos/build-dmg.sh 0.2.0
```

Windows packaging is intentionally simple: CI builds
`target/release/apiwright-ide.exe` and copies it to the versioned GUI release
name. Every platform job also builds `forge-cli` and copies the `apiwright`
binary to the corresponding versioned CLI release name.

Current packages are not publisher-signed or notarized. Expect Windows
SmartScreen and macOS Gatekeeper warnings; do not describe the artifacts as
trusted/signed installers until real signing and notarization are added.

## Standalone CLI and MCP

The CLI release asset runs request projects without the desktop package and
also contains the local MCP stdio server. On Linux and macOS, mark a downloaded
CLI asset executable before the first run:

```sh
chmod +x ./ApiWright-*-cli-linux-x86_64
./ApiWright-*-cli-linux-x86_64 --version
./ApiWright-*-cli-linux-x86_64 mcp
```

For an MCP client, use the absolute path of the downloaded CLI asset as
`command` and `["mcp"]` as `args`. Project roots are passed to individual MCP
tools. See [MCP and AI automation](mcp.md) for the client configuration,
available tools, and safe AI workflow.

## Auto-update compatibility

The desktop updater reads the latest release from the repository's GitHub API
and compares semantic versions with the version compiled into the app. Its
asset selector depends on the exact platform suffixes listed above. Renaming
release files requires a matching change in `forge-gui/src/updater.rs`.

Before staging a download, the updater verifies SHA-256 using the GitHub asset
digest when present or the matching line in `SHA256SUMS.txt`. It rejects a
missing or mismatched checksum.

Installation differs by platform:

- Linux can replace and restart only when ApiWright is running from an AppImage
  and the `APPIMAGE` path is available.
- Windows waits for the current process, replaces the executable and restarts
  it through PowerShell.
- macOS opens the downloaded DMG and leaves the operating-system installation
  step to the user.

The user can inspect release notes, download, open the release page or skip one
specific tag. Skipping does not disable checks for later versions.

## Post-release verification

After the workflow completes:

1. Confirm the release title/tag, all six platform files, and
   `SHA256SUMS.txt`.
2. Verify one package against `SHA256SUMS.txt` with `sha256sum -c` or the
   platform equivalent.
3. Launch each GUI package and run the matching CLI with `--version` on its
   target OS.
4. Start `apiwright mcp` through an MCP client and list its tools.
5. Confirm **Check for updates** reports the installed version as current.
6. If any platform job failed, fix the source and publish a new version; do
   not manually assemble a partial release under the original tag.
