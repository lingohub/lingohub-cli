// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::colors::{BLUE_DARK_RGB, BLUE_LIGHT_RGB};
use clap::{Parser, Subcommand, ValueEnum};

use clap::builder::styling::{Effects, RgbColor, Styles};
use clap_complete::aot::Shell;

fn styles() -> Styles {
    let dark = RgbColor(BLUE_DARK_RGB.0, BLUE_DARK_RGB.1, BLUE_DARK_RGB.2);
    let light = RgbColor(BLUE_LIGHT_RGB.0, BLUE_LIGHT_RGB.1, BLUE_LIGHT_RGB.2);
    Styles::styled()
        .header(light.on_default() | Effects::BOLD)
        .usage(light.on_default() | Effects::BOLD)
        .literal(dark.on_default() | Effects::BOLD)
        .placeholder(RgbColor(180, 180, 180).on_default())
        .error(clap::builder::styling::AnsiColor::Red.on_default() | Effects::BOLD)
        .valid(light.on_default() | Effects::BOLD)
        .invalid(clap::builder::styling::AnsiColor::Yellow.on_default() | Effects::BOLD)
}

/// LingoHub command-line interface.
#[derive(Parser)]
#[command(name="lingohub", version, about="LingoHub V2 CLI", styles=styles())]
pub struct Cli {
    /// API token; prefer LINGOHUB_API_KEY to keep secrets out of shell history.
    #[arg(long, env = "LINGOHUB_API_KEY", global = true, hide_env_values = true)]
    pub api_key: Option<String>,
    /// API origin, e.g. http://localhost:8080 for a disposable backend.
    #[arg(long, env="LINGOHUB_BASE_URL", global=true, default_value=crate::transport::DEFAULT_BASE_URL)]
    pub base_url: String,
    /// Terminal color policy: auto, always or never.
    #[arg(value_enum,long,global=true,default_value_t=ColorMode::Auto)]
    pub color: ColorMode,
    /// Preserve wire JSON on stdout and emit structured errors on stderr.
    #[arg(long, global = true)]
    pub json: bool,
    /// JSON request body; use - for stdin.
    #[arg(long, global = true, conflicts_with = "fields")]
    pub file: Option<String>,
    /// Set a top-level request field as NAME=JSON, e.g. website=null. Repeat for multiple fields.
    #[arg(long="field",global=true,value_parser=parse_pair)]
    pub fields: Vec<(String, String)>,
    /// Conditional write validator, including the server's quotes.
    #[arg(long, global = true)]
    pub if_match: Option<String>,
    /// Explicit idempotency key for a supported mutation; requests are never retried automatically.
    #[arg(long, global = true)]
    pub idempotency_key: Option<String>,
    /// Print HTTP status and selected response headers to stderr.
    #[arg(long, global = true)]
    pub include_headers: bool,
    #[command(subcommand)]
    pub command: Option<Commands>,
}
#[derive(Debug, Clone, ValueEnum)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}
#[derive(Subcommand)]
pub enum Commands {
    /// Open the interactive workspace dashboard.
    Tui,
    /// Show the authenticated workspace.
    Workspace,
    /// List, inspect and edit stacks.
    Stack {
        #[command(subcommand)]
        action: ResourceAction,
    },
    /// List, inspect and edit projects.
    Project {
        #[command(subcommand)]
        action: ResourceAction,
    },
    /// List, inspect and edit style guides.
    StyleGuide {
        #[command(subcommand)]
        action: ResourceAction,
    },
    /// Inspect and invoke the bundled V2 operation contract.
    Api {
        #[command(subcommand)]
        action: ApiAction,
    },
    /// Generate shell completion (Bash by default).
    Completion { shell: Option<Shell> },
    /// Upgrade to the latest release; Homebrew installations are told to use brew.
    Upgrade {
        /// Only report whether a newer release exists.
        #[arg(long)]
        check: bool,
    },
}
#[derive(Subcommand)]
pub enum ResourceAction {
    /// Fetch one cursor page, or explicitly follow all pages.
    List {
        /// Raw JSON filter expression (the client URL-encodes it).
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        cursor: Option<String>,
        #[arg(long,value_parser=clap::value_parser!(u16).range(1..=100))]
        limit: Option<u16>,
        /// Follow every cursor and emit the combined data array.
        #[arg(long)]
        all: bool,
    },
    /// Inspect one resource; prompts for its ID in a terminal.
    Get { id: Option<String> },
    /// Create a resource using fields, a JSON body or interactive prompts.
    Create,
    /// Apply a JSON merge patch to one resource.
    Update { id: Option<String> },
    /// Delete one resource using existing API behavior.
    Delete { id: Option<String> },
}
#[derive(Subcommand)]
pub enum ApiAction {
    /// List published operations; this inventory does not imply workflow verification.
    Operations,
    /// Preview generated MCP tools offline; defaults to a small read-only discovery preset.
    McpTools {
        /// Include all JSON operations; unsupported multipart adapters are reported separately.
        #[arg(long, conflicts_with_all = ["operations", "tags"])]
        all: bool,
        #[arg(long = "operation")]
        operations: Vec<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Serve MCP over stdin/stdout using the selected V2 tools (read-only discovery by default).
    McpServe {
        /// Expose all supported JSON operations, including mutations.
        #[arg(long, conflicts_with_all = ["operations", "tags"])]
        all: bool,
        #[arg(long = "operation")]
        operations: Vec<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Call a JSON operation by its OpenAPI operation ID.
    Call {
        operation: String,
        #[arg(long="path",value_parser=parse_pair)]
        parameters: Vec<(String, String)>,
        #[arg(long="query",value_parser=parse_pair)]
        query: Vec<(String, String)>,
    },
    /// Upload one file with typed form fields to a published multipart operation.
    Upload {
        operation: String,
        #[arg(long="path",value_parser=parse_pair)]
        parameters: Vec<(String, String)>,
        #[arg(long="query",value_parser=parse_pair)]
        query: Vec<(String, String)>,
        /// File to send in the file part (regular file, at most 64 MiB).
        #[arg(long, conflicts_with_all = ["file", "fields"])]
        upload: String,
        /// Multipart field; arrays/objects use JSON, e.g. connections=[].
        #[arg(long="form",value_parser=parse_pair)]
        form: Vec<(String, String)>,
        /// Read a JSON array/object part from a regular UTF-8 file (all form values at most 64 KiB).
        #[arg(long="form-file",value_parser=parse_pair)]
        form_files: Vec<(String, String)>,
    },
    /// Save a completed export to a new file using a fresh signed download link.
    DownloadExport {
        export_id: String,
        /// New destination file. Existing files are never overwritten; archives are not extracted.
        #[arg(long, conflicts_with_all = ["file", "fields", "if_match", "idempotency_key"])]
        output: String,
        /// Maximum artifact size in bytes (default 512 MiB).
        #[arg(long, default_value_t = 536_870_912, value_parser = clap::value_parser!(u64).range(1..))]
        max_bytes: u64,
    },
    /// Save one successful export entry using a fresh signed link from its export's entries.
    DownloadExportEntry {
        export_id: String,
        entry_id: String,
        /// New destination file. Existing files are never overwritten; content is not extracted.
        #[arg(long, conflicts_with_all = ["file", "fields", "if_match", "idempotency_key"])]
        output: String,
        /// Maximum artifact size in bytes (default 512 MiB).
        #[arg(long, default_value_t = 536_870_912, value_parser = clap::value_parser!(u64).range(1..))]
        max_bytes: u64,
        /// Maximum entries pages to inspect (100 entries requested per page).
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=10_000))]
        max_pages: u64,
    },
    /// Save a completed distribution release archive using a fresh signed link.
    DownloadDistributionRelease {
        distribution_release_id: String,
        /// New destination file. Existing files are never overwritten; archives are not extracted.
        #[arg(long, conflicts_with_all = ["file", "fields", "if_match", "idempotency_key"])]
        output: String,
        /// Maximum artifact size in bytes (default 512 MiB).
        #[arg(long, default_value_t = 536_870_912, value_parser = clap::value_parser!(u64).range(1..))]
        max_bytes: u64,
    },
    /// Wait for a known V2 job resource to finish.
    Wait {
        #[arg(value_parser = ["getImport", "getExport", "getWebhookDelivery", "getBranch", "getBackup", "getBackupRestore", "getProjectMove", "getLinaExecution"])]
        operation: String,
        #[arg(long="path", value_parser=parse_pair, conflicts_with_all=["file", "fields", "if_match", "idempotency_key"])]
        parameters: Vec<(String, String)>,
        /// Seconds between pending reads.
        #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u64).range(1..=60))]
        interval: u64,
        /// Total deadline in seconds, including requests (maximum one day).
        #[arg(long, default_value_t=300, value_parser=clap::value_parser!(u64).range(1..=86400))]
        timeout: u64,
    },
    /// Send one JSON request to a V2 path on the configured origin.
    Request {
        #[arg(value_parser=["GET","POST","PUT","PATCH","DELETE"])]
        method: String,
        path: String,
        #[arg(long="query",value_parser=parse_pair)]
        query: Vec<(String, String)>,
    },
}
pub fn parse_pair(value: &str) -> Result<(String, String), String> {
    let (key, value) = value.split_once('=').ok_or("Expected NAME=VALUE")?;
    if key.is_empty() {
        return Err("Parameter name cannot be empty".into());
    }
    Ok((key.into(), value.into()))
}
