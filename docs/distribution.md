# Releases and distribution

Releases are GitHub releases of this repository, built and published by
[dist](https://axodotdev.github.io/cargo-dist/). Each release contains:

- archives for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-musl`,
  `x86_64-unknown-linux-musl` (`.tar.gz`) and `x86_64-pc-windows-msvc` (`.zip`), each with a
  `.sha256` file, plus `sha256.sum`;
- `lingohub-installer.sh` and `lingohub-installer.ps1`, which download the archive for the
  host over HTTPS and install it. The shell installer also checks the archive's SHA-256
  checksum when `sha256sum` is on PATH (coreutils on Linux; macOS 26 ships it in `/sbin`) and
  prints a message when it has to skip the check. The PowerShell installer does not check it;
- the Homebrew formula `lingohub.rb`, which is also pushed to
  [lingohub/homebrew-cli](https://github.com/lingohub/homebrew-cli);
- GitHub build attestations for every archive.

The configuration is [`dist-workspace.toml`](../dist-workspace.toml). The workflow
`.github/workflows/v-release.yml` is generated from it: change the configuration, run
`dist generate` and commit both. Pull requests run `dist plan`, which fails when the workflow
no longer matches the configuration.

## Publish a release

1. In a pull request, set `version` in `Cargo.toml` (semantic versioning), run `cargo build`
   so `Cargo.lock` follows, and add the release notes to `CHANGELOG.md` under a
   `## X.Y.Z - YYYY-MM-DD` heading. dist uses that section as the release description.
2. Merge the pull request.
3. Tag the merge commit on `main` with the same version and push the tag:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

The workflow builds every target on its own runner (Linux ARM64 on a native ARM runner),
creates the release with all files, pushes the formula to the tap and publishes the release.
Only tags that start with `v` start the workflow (`tag-namespace` in the configuration). A
tag that does not match the `Cargo.toml` version exactly fails in the plan step before anything
is published.

A prerelease needs the prerelease version in `Cargo.toml` too: set `version = "0.2.0-rc.1"`,
merge, then tag `v0.2.0-rc.1`. It publishes a GitHub prerelease and leaves the Homebrew formula
unchanged. `lingohub upgrade` and the update notice ignore prereleases, because GitHub's latest
release never is one. Use a prerelease to try the installers before a real release:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/lingohub/lingohub-cli/releases/download/v0.2.0-rc.1/lingohub-installer.sh | sh
```

Never move or reuse a published tag. Fix a broken release with a new patch version.

## One-time setup

- The repository and its releases must be public: installers, Homebrew and
  `lingohub upgrade` download anonymously.
- [lingohub/homebrew-cli](https://github.com/lingohub/homebrew-cli) must exist as a public
  repository. dist writes `Formula/lingohub.rb` there.
- The repository secret `HOMEBREW_TAP_TOKEN` holds a fine-grained token with
  *Contents: read and write* on `lingohub/homebrew-cli` only. The release itself uses the
  workflow's `GITHUB_TOKEN`.
- A tag ruleset limits who may create, move or delete tags. Pushing a `v*` tag publishes to
  every user and runs a job that holds `HOMEBREW_TAP_TOKEN`.

## How installation and upgrades work

| Installation | Install | Upgrade |
|---|---|---|
| Homebrew | `brew install lingohub/cli/lingohub` | `brew upgrade lingohub` |
| macOS/Linux installer | `curl … lingohub-installer.sh \| sh` | `lingohub upgrade` |
| Windows installer | `irm … lingohub-installer.ps1 \| iex` | `lingohub upgrade` |
| Archive extracted by hand | copy `lingohub` onto PATH | `lingohub upgrade` |

The installers place the binary in `$XDG_BIN_HOME`, `$XDG_DATA_HOME/../bin` or `~/.local/bin`,
whichever is set first, and add that directory to the shell profile unless it is already on
PATH. `LINGOHUB_INSTALL_DIR` overrides the directory and `LINGOHUB_NO_MODIFY_PATH=1` leaves
the profile alone.

`lingohub upgrade` asks the GitHub releases API for the latest release. A binary inside a
Homebrew `Cellar` is told to run `brew upgrade lingohub`, because Homebrew owns those files.
Any other binary is upgraded in place: it is moved aside as `lingohub.previous`, then that
release's installer runs with `LINGOHUB_INSTALL_DIR` set to the binary's directory and
`LINGOHUB_NO_MODIFY_PATH=1`, so the checksum check described above applies. The upgrade only
succeeds when the new binary reports the new version. If the installer fails, writes nothing or
installs another version, the previous binary is moved back and the command says the
installation is unchanged; if even that is impossible, it says where the previous binary was
kept. After a successful upgrade the previous binary is deleted; Windows cannot delete a
running executable, so there it is deleted on the next start. If the directory is not
writable, the command says so and changes nothing. `lingohub upgrade --check` only reports.

At most once a day, an interactive command that calls the API looks up the latest release in
the background and prints one line on stderr when a newer one exists. The lookup never delays
the command: if it has not finished when the command ends, it is dropped and tried again with
the next command. It is skipped for `--json`, when stderr is not a terminal, when `CI` is set,
for offline commands, the dashboard, the MCP server, and when `LINGOHUB_NO_UPDATE_CHECK` is
set. The time of the last lookup is stored in `~/Library/Caches/lingohub/update-check`
(macOS), `$XDG_CACHE_HOME/lingohub/update-check` or `~/.cache/lingohub/update-check` (Linux)
and `%LOCALAPPDATA%\lingohub\update-check` (Windows).

Release lookups and installer downloads use a separate unauthenticated client: the LingoHub
API key is never sent to GitHub. Requests identify themselves as `lingohub-cli/VERSION`, the
same User-Agent the CLI sends to the LingoHub API, so API logs show which versions are in use.

## Platform notes

- **Linux** binaries are statically linked against musl and use rustls, so they need no
  system libraries. TLS verification uses the system CA certificates (`/etc/ssl/certs`).
  Minimal container images without them, such as `scratch`, need the `ca-certificates`
  package or a mounted CA bundle.
- **macOS** binaries are not notarized. Homebrew and the installer do not quarantine what
  they download, so Gatekeeper does not block them. An archive downloaded in a browser is
  quarantined; remove the attribute with `xattr -d com.apple.quarantine lingohub`.
- **Windows** builds are x86_64 only.

Verify that an archive was built by this repository's release workflow:

```sh
gh attestation verify lingohub-x86_64-unknown-linux-musl.tar.gz --repo lingohub/lingohub-cli
```

## Pull request checks

Every pull request runs formatting, tests, clippy and the script tests on Linux, then builds
the static `x86_64-unknown-linux-musl` release binary with the release profile and runs
[`scripts/smoke.py`](../scripts/smoke.py) on it (version, help, operation inventory and static
linkage). The `native-packages` label, or a manual run of the *Native builds* workflow, does
the same on macOS (both architectures), Linux ARM64 and Windows.
