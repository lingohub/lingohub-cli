# LingoHub CLI

`lingohub` is the command-line client, terminal dashboard and MCP server for the
[LingoHub](https://lingohub.com) API V2 (public beta). It uses a bundled V2 OpenAPI
contract and needs no separate SDK.

## Install

macOS and Linux with [Homebrew](https://brew.sh):

```sh
brew install lingohub/cli/lingohub
```

macOS and Linux without Homebrew:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/lingohub/lingohub-cli/releases/latest/download/lingohub-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/lingohub/lingohub-cli/releases/latest/download/lingohub-installer.ps1 | iex"
```

The installers place `lingohub` in `$XDG_BIN_HOME` or `~/.local/bin` and add that directory
to PATH when needed. Linux binaries are statically linked and run on any distribution,
including Alpine. Archives for macOS (Apple silicon and Intel), Linux (x86_64 and ARM64) and
Windows (x86_64), with checksums and build attestations, are on the
[releases page](https://github.com/lingohub/lingohub-cli/releases).

## Upgrade

```sh
brew upgrade lingohub      # installed with Homebrew
lingohub upgrade           # installed with the installer or by hand
lingohub upgrade --check   # only report whether a newer release exists
```

`lingohub upgrade` runs the new release's installer into the directory that holds the current
binary and leaves PATH alone. A Homebrew installation is pointed to `brew upgrade`.

At most once a day, an interactive command that calls the API prints one line on stderr when a
newer release exists. The lookup runs alongside the command and never delays it. There is no
notice with `--json`, when `CI` is set, for offline commands, `login` and `logout`, in the
dashboard or in the MCP server; set `LINGOHUB_NO_UPDATE_CHECK=1` to turn it off. The lookup is an unauthenticated
request to the GitHub releases API: the LingoHub API key is never sent to it.

## Build from source

Use Rust 1.98.1, pinned in `rust-toolchain.toml`:

```sh
cargo build --locked --release
./target/release/lingohub --version
./target/release/lingohub --help
```

Contributors: `cargo fmt -- --check`, `cargo test --locked`,
`cargo clippy --locked --all-targets -- -D warnings` and
`python3 -m unittest discover -s scripts -p 'test_*.py'` run the local checks. The
[release guide](docs/distribution.md) describes how releases are published.

## Configuration

Sign in once on each computer:

```sh
lingohub login
lingohub workspace
```

`lingohub login` opens a confirmation page in the LingoHub app and prints its link with a
verification code; check that the page shows the same code before you confirm. `--no-browser`
only prints the link. The CLI then stores a new personal API key as `LINGOHUB_API_KEY` in
`~/.lingohub`, readable only by you (mode 600 on macOS and Linux), and leaves every other line of
that file unchanged. The key is named after the computer (`--name` replaces the host name), is
valid for 90 days and can be revoked in the LingoHub app at any time; run `lingohub login` again
for a new one. The key it replaces stays active, because it may be in use elsewhere: revoke it in
the app if you no longer use it. The key is never printed.

`lingohub logout` revokes the stored key and removes it from `~/.lingohub`. A key that is no
longer valid is only removed, and so is a workspace-managed key, which a workspace administrator
revokes in the app. Logout never touches a key from `--api-key` or the environment.

For a key created in the app, or in CI, set the key in the environment instead:

```sh
export LINGOHUB_API_KEY=lh-api_your_key_here
lingohub workspace --json
lingohub --base-url http://localhost:8080 workspace --json
```

The default origin is `https://api.lingohub.com`. Do not append `/v2`. Remote origins
require HTTPS; HTTP is supported for `localhost` and literal loopback addresses.

Configuration precedence is **command-line flag → process environment → user config
→ default**. `--api-key` overrides `LINGOHUB_API_KEY`; `--base-url` overrides
`LINGOHUB_BASE_URL`. Prefer environment variables to secrets in shell history.
Optional dotenv-format user configuration is read from `$HOME/.lingohub`, falling
back to `%USERPROFILE%/.lingohub` when `HOME` is absent or empty on Windows.
A `.env` in the current directory or its parents is never loaded: a checked-out
repository must not redirect a stored API credential to another host. Because of this
precedence, a key from `--api-key` or the environment is used instead of the one `lingohub login`
stores; login and logout point it out.

Running without a command prints help. `--help`, `--version`, `completion`,
`api operations` and `api mcp-tools` work offline without a key; `upgrade`, `login` and `logout` need no key. `lingohub tui` opens
the dashboard and prompts if the key is missing.

## Commands and output

| Command | Purpose |
|---|---|
| `login [--name NAME] [--no-browser]` | Sign in through the browser and store a personal API key |
| `logout` | Revoke the stored API key and remove it from `~/.lingohub` |
| `workspace` | Read the authenticated workspace |
| `stack`, `project`, `style-guide` | `list`, `get`, `create`, `update`, `delete` |
| `api operations` | List the bundled operation IDs, methods and paths |
| `api call OPERATION` | Send one JSON operation by its operation ID |
| `api request METHOD /v2/PATH` | Send one request on the configured API origin |
| `api upload OPERATION` | Send one file and typed multipart form fields |
| `api wait OPERATION` | Poll a supported asynchronous resource |
| `api download-export EXPORT_ID` | Download a completed export archive |
| `api download-export-entry EXPORT_ID ENTRY_ID` | Download one successful export entry |
| `api download-distribution-release RELEASE_ID` | Download a distribution release archive |
| `api mcp-tools`, `api mcp-serve` | Inspect tool schemas offline or serve MCP over stdio |
| `tui` | Browse workspace, stacks and projects interactively |
| `completion [SHELL]` | Generate completion; defaults to Bash |
| `upgrade [--check]` | Upgrade to the latest release, or only report it |

Use `lingohub COMMAND --help` for command-specific arguments. Common flags:

| Flag | Meaning |
|---|---|
| `--json` | Preserve response JSON on stdout; structured errors on stderr |
| `--include-headers` | Print HTTP status and selected workflow headers on stderr |
| `--color auto\|always\|never` | Choose terminal color policy |
| `--file PATH` or `--file -` | JSON request body from a file or stdin |
| `--field NAME=JSON` | Set a top-level field; repeat for multiple fields |
| `--if-match '"ETAG"'` | Conditional write using the server's quoted validator |
| `--idempotency-key VALUE` | Explicit key for a supported mutation |

`--file` and `--field` cannot be combined. Strings in fields need JSON quotes:
`--field 'name="Example"'`; `--field 'description=null'` sends a real null.
PATCH uses JSON merge-patch: omitted fields stay unchanged, null is explicit.
Read-only resource commands and offline commands reject unsupported body/write flags.

Exit **0** means success, **1** means API/network/protocol failure or an incomplete
bulk/translation/job outcome, and **2** means invalid input. A partial operation
can print its full response and still exit 1; inspect every item before retrying.
Empty successful responses produce no invented JSON. Human resource views use aligned labels, colors and `—` for null values. Collection
headings show the number returned, not an invented total or page number. Other
operations retain generic field output; use `--json` for complete wire values in scripts.

## Resource and generic operations

Examples use placeholders such as `stack_UUID` and `pr_UUID`; replace them with
public IDs returned by the API.

```sh
lingohub stack list --limit 20 --json
lingohub stack list --cursor 'OPAQUE_CURSOR' --json
lingohub project get pr_UUID --include-headers --json
lingohub project update pr_UUID --field 'name="Updated"' --if-match '"ETAG"' --json
lingohub api call getText --path textId=txt_UUID --json
lingohub api call listTexts --path branchId=prb_UUID --query limit=20 --json
lingohub api request GET /v2/capabilities --json
```

Resource lists return one cursor page by default; `--all` combines pages into an
array. Generic `api call` and `api request` always send one request: follow a
`next_cursor` with `--query cursor=VALUE`. Interactive selection loads the next
page only when requested.

Create requests use the bundled schemas. For example, a stack needs `name`,
`color` and `type`; a project needs `name`, `type`, `source_language_code` and
`target_language_codes`. Supply JSON with `--file` or use terminal prompts.

The [operation inventory](docs/operations.md) covers every bundled route. The
[workflow cookbook](docs/cookbook.md) has import/export, shared content, Git,
permissions and bulk examples. The contract itself is [contract/v2.json](contract/v2.json);
[provenance](contract/provenance.json) pins its backend revision and checksum.
Operation addressability does not imply every provider or workflow was tested live.

## Upload, poll and download

```sh
lingohub api upload createImport --path branchId=prb_UUID \
  --upload ./de.json --form language_code=de --json
lingohub api wait getImport --path importId=rin_UUID --interval 2 --timeout 120 --json
lingohub api wait getExport --path exportId=reg_UUID --json
lingohub api download-export reg_UUID --output ./export.zip --json
```

`api wait` supports `getImport`, `getExport`, `getBranch`, `getWebhookDelivery`,
`getBackup`, `getBackupRestore`, `getProjectMove` and `getLinaExecution`. It prints
the terminal state, or the last pending state on timeout, with a nonzero exit for
failure/incomplete outcomes. It does not submit or restart a job.

Downloads refresh API metadata, then use an unauthenticated artifact client. They
never forward the API token, follow redirects or overwrite an existing destination.
Use `--max-bytes` to bound downloads and `--max-pages` for export-entry lookup.
On an expired link, rerun to fetch fresh metadata.

## MCP

```sh
lingohub api mcp-tools
lingohub api mcp-tools --operation getProject --operation setProjectKeySettings
lingohub api mcp-tools --tag 'Project Settings'
lingohub api mcp-serve
```

The default is a small read-only discovery preset. Select operations or tags, or
use `--all` to expose all supported JSON operations, including mutations. The
catalog reports multipart operations as unsupported; use CLI upload commands for
those. Tool schemas carry typed path/query/body/header inputs and local schema
definitions. `api mcp-tools` only prints descriptors; `api mcp-serve` starts the
stdio server. Supply credentials in the MCP host's environment:

```json
{"mcpServers":{"lingohub":{"command":"lingohub","args":["api","mcp-serve","--tag","Git Integrations"]}}}
```

Tool arguments cannot change the API origin or credential. Only `If-Match` and
`Idempotency-Key` headers are accepted. Each tool call sends one API request and
returns status, selected headers and the full body. Partial bulk/translation
outcomes and API failures set `isError`. Poll jobs explicitly. Selected tools
remain subject to the API key's current permissions.

## Request boundaries

Requests are never retried automatically. Idempotency is best-effort server
behavior; expiry or cache loss can permit re-execution. Reconcile an uncertain
outcome through reads before resubmitting a mutation.

JSON input and responses are limited to 16 MiB. Regular file opens/reads have a
ten-second deadline. Multipart files are bounded to 64 MiB and form fields to
64 KiB. Automatic list traversal stops explicitly above 1,000 pages, 100,000 items,
64 MiB of received JSON or five minutes; use explicit cursor pages for larger lists.

## License

Apache License 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
