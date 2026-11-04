// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

//! `lingohub login` and `lingohub logout`.
//!
//! Login starts a client authentication on the API, lets the user confirm it in the web app
//! and stores the personal API key it returns in the user configuration file that `main.rs`
//! loads at startup. Both login endpoints are public, so they are called with a client of their
//! own that never carries an API key. Logout revokes the stored key with that key and removes it
//! from the file.

use crate::{
    cli::{output, parse::Cli},
    colors::styled,
    error::Error,
    transport::{ApiClient, RequestOptions, api_origin, is_loopback, network_error},
};
use crossterm::style::Stylize;
use reqwest::{Client, Method, Response, Url, header::ACCEPT};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::time::{Instant, sleep_until};

/// The variable the CLI reads its API key from.
pub const API_KEY_VARIABLE: &str = "LINGOHUB_API_KEY";
const CONFIG_FILE_NAME: &str = ".lingohub";
/// The API measures the device name in UTF-16 code units.
const MAX_NAME_LENGTH: usize = 100;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_KEY_LENGTH: usize = 8 * 1024;
const MAX_POLL_INTERVAL_SECONDS: u64 = 60;
const MAX_EXPIRY_SECONDS: u64 = 60 * 60;
const LOGIN_AGAIN: &str = "Run `lingohub login` again.";

/// A key that takes precedence over the stored one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOverride {
    /// `--api-key` on the command line.
    Flag,
    /// `LINGOHUB_API_KEY` in the process environment, not loaded from the user configuration.
    Environment,
}

/// The trusted user configuration: `$HOME/.lingohub`, or `%USERPROFILE%/.lingohub` when HOME
/// is absent or empty.
pub fn config_file() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(|home| PathBuf::from(home).join(CONFIG_FILE_NAME))
}

fn require_config_file() -> Result<PathBuf, Error> {
    config_file().ok_or_else(|| {
        Error::Input("Cannot locate the user configuration file: set HOME (or USERPROFILE on Windows)".into())
    })
}

// ~~~~~~~~~~ Login <<<

/// A started login, as the API returned it.
struct Started {
    token: String,
    secret: String,
    code: String,
    url: Url,
    expires: Duration,
    interval: Duration,
}

/// The key of a confirmed login, returned exactly once.
struct Issued {
    api_key: String,
    api_key_name: String,
    workspace_id: String,
    workspace_name: String,
    expiration_date: Option<String>,
}

/// `lingohub login [--name NAME] [--no-browser]`.
pub async fn login(
    name: Option<String>,
    open_browser: bool,
    cli: &Cli,
    key_override: Option<KeyOverride>,
) -> Result<(), Error> {
    let path = require_config_file()?;
    // Fail before the API creates a key that could not be stored.
    read_config(&path)?;
    write_target(&path)?;
    let name = match name {
        Some(name) => checked_name(&name)?,
        None => device_name(),
    };
    let base = api_origin(&cli.base_url)?;
    let client = public_client()?;
    let started = start(&client, &base, &name).await?;
    let begin = Instant::now();
    eprintln!(
        "Open this link and check that it shows the code {}:\n\n  {}\n\nWaiting for the confirmation (press Ctrl-C to cancel)...",
        printable(&started.code),
        started.url
    );
    if open_browser {
        open_in_browser(&started.url);
    }
    let issued = wait_for_key(&client, &base, &started, begin).await?;
    store_key(&path, &issued)?;
    if cli.json {
        output::print_value(
            &json!({
                "workspace_id": issued.workspace_id,
                "workspace_name": issued.workspace_name,
                "api_key_name": issued.api_key_name,
                "expiration_date": issued.expiration_date,
                "config_file": path.to_string_lossy(),
            }),
            true,
        )?;
    } else {
        let validity = match &issued.expiration_date {
            Some(date) => format!(", valid until {}", printable(date)),
            None => " without an expiration date".to_owned(),
        };
        println!(
            "Signed in to {} with the personal API key \"{}\"{validity}. Stored in {}.",
            printable(&issued.workspace_name),
            printable(&issued.api_key_name),
            path.display()
        );
    }
    if let Some(key_override) = key_override {
        let source = match key_override {
            KeyOverride::Flag => "--api-key",
            KeyOverride::Environment => "LINGOHUB_API_KEY in the environment",
        };
        eprintln!(
            "{}",
            styled(format!("Warning: {source} takes precedence over the stored key.").yellow())
        );
    }
    Ok(())
}

fn checked_name(name: &str) -> Result<String, Error> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) || name.encode_utf16().count() > MAX_NAME_LENGTH
    {
        return Err(Error::Input(
            "--name must have 1 to 100 characters and no control characters".into(),
        ));
    }
    Ok(name.to_owned())
}

/// The host name, from the `hostname` command or the environment, else a generic name.
fn device_name() -> String {
    let command = Command::new("hostname")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
    let name = command
        .into_iter()
        .chain(
            ["COMPUTERNAME", "HOSTNAME"]
                .into_iter()
                .filter_map(|variable| std::env::var(variable).ok()),
        )
        .map(|name| name.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|name| truncate_name(name.trim()))
        .find(|name| !name.is_empty());
    name.unwrap_or_else(|| "this device".to_owned())
}

fn truncate_name(name: &str) -> String {
    let mut length = 0;
    name.chars()
        .take_while(|c| {
            length += c.len_utf16();
            length <= MAX_NAME_LENGTH
        })
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// A client without credentials for the public login endpoints.
fn public_client() -> Result<Client, Error> {
    Client::builder()
        .user_agent(concat!("lingohub-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(network_error)
}

fn login_url(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("an HTTP(S) origin has a path")
        .clear()
        .extend(segments);
    url
}

async fn start(client: &Client, base: &Url, name: &str) -> Result<Started, Error> {
    let response = client
        .post(login_url(base, &["public", "cli", "clientAuthentications"]))
        .header(ACCEPT, "application/json")
        .json(&json!({"name": name}))
        .send()
        .await
        .map_err(network_error)?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = read_bounded(response).await?;
    match status {
        200 => parse_started(&body)
            .ok_or_else(|| Error::Protocol("The API returned an invalid login; no login was started".into())),
        400.. => Err(Error::from_response(status, &body, &headers)),
        _ => Err(Error::Protocol(format!(
            "Starting the login returned HTTP {status}"
        ))),
    }
}

fn parse_started(body: &[u8]) -> Option<Started> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let text = |field: &str| -> Option<String> {
        value
            .get(field)?
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    // The token becomes one path segment; a dot segment would address another endpoint.
    let token = text("clientAuthenticationToken")
        .filter(|token| token.bytes().all(|b| b.is_ascii_graphic()) && !token.trim_matches('.').is_empty())?;
    let seconds = |field: &str| value.get(field)?.as_u64();
    Some(Started {
        token,
        secret: text("secret")?,
        code: text("verificationCode")?,
        url: verification_url(&text("verificationUrl")?)?,
        expires: Duration::from_secs(seconds("expiresInSeconds")?.min(MAX_EXPIRY_SECONDS)),
        interval: Duration::from_secs(seconds("pollIntervalSeconds")?.clamp(1, MAX_POLL_INTERVAL_SECONDS)),
    })
}

/// The confirmation link is shown and opened, so it must be a plain web URL.
fn verification_url(text: &str) -> Option<Url> {
    let url = Url::parse(text).ok()?;
    (url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && (url.scheme() == "https" || url.scheme() == "http" && is_loopback(&url)))
    .then_some(url)
}

/// Best effort: when no browser opens, the printed link is the way in.
fn open_in_browser(url: &Url) {
    let _ = browser_command(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(target_os = "macos")]
fn browser_command(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(windows)]
fn browser_command(url: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut command = Command::new("cmd");
    // `start` reads the first quoted argument as the window title. The quotes around the link
    // keep cmd from reading an `&` in it as a command separator; a parsed URL has no quotes.
    command.raw_arg(format!("/C start \"\" \"{url}\""));
    command
}

#[cfg(not(any(target_os = "macos", windows)))]
fn browser_command(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

/// Polls until the login is confirmed, finally refused, or expired. The secret travels only in
/// the request body.
async fn wait_for_key(
    client: &Client,
    base: &Url,
    started: &Started,
    begin: Instant,
) -> Result<Issued, Error> {
    let url = login_url(
        base,
        &["public", "cli", "clientAuthentications", &started.token, "apiKey"],
    );
    let body = json!({"secret": started.secret});
    let deadline = begin + started.expires;
    let mut next = begin;
    loop {
        let response = client
            .post(url.clone())
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let received = read_bounded(response).await?;
        match status {
            200 => {
                return parse_issued(&received).ok_or_else(|| {
                    Error::Protocol(format!(
                        "The login was confirmed, but the API returned no valid API key. Revoke the new key in LingoHub and {}",
                        LOGIN_AGAIN.to_lowercase()
                    ))
                });
            }
            202 => {}
            400..=499 => {
                return Err(Error::Protocol(format!(
                    "The login was denied, expired or already used. {LOGIN_AGAIN}"
                )));
            }
            500.. => return Err(Error::from_response(status, &received, &headers)),
            _ => {
                return Err(Error::Protocol(format!(
                    "Waiting for the login confirmation returned HTTP {status}"
                )));
            }
        }
        next += started.interval;
        if next >= deadline {
            sleep_until(deadline).await;
            return Err(Error::Protocol(format!("The login expired. {LOGIN_AGAIN}")));
        }
        sleep_until(next).await;
    }
}

fn parse_issued(body: &[u8]) -> Option<Issued> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let text = |field: &str| value.get(field)?.as_str().map(str::to_owned);
    Some(Issued {
        api_key: text("apiKey").filter(|key| storable_key(key))?,
        api_key_name: text("apiKeyName")?,
        workspace_id: text("workspaceId")?,
        workspace_name: text("workspaceName")?,
        expiration_date: match value.get("expirationDate") {
            None | Some(Value::Null) => None,
            Some(date) => Some(date.as_str()?.to_owned()),
        },
    })
}

/// The key is written unquoted, so it must not contain anything dotenv would interpret:
/// whitespace, quotes, `#`, `$` or `\`.
fn storable_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LENGTH
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~+/=:".contains(&b))
}

async fn read_bounded(mut response: Response) -> Result<Vec<u8>, Error> {
    let too_large = || Error::Protocol("The login response exceeds 64 KiB".into());
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn store_key(path: &Path, issued: &Issued) -> Result<(), Error> {
    // Read again: the file may have changed while the user confirmed the login.
    update_config(path, Some(&issued.api_key)).map_err(|error| {
        Error::Io(io::Error::other(format!(
            "The API key \"{}\" was created, but could not be stored in {} ({}). Revoke it in LingoHub and {}",
            printable(&issued.api_key_name),
            path.display(),
            cause(&error),
            LOGIN_AGAIN.to_lowercase()
        )))
    })
}

/// Server text shown in the terminal never carries control sequences.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

// >>>

// ~~~~~~~~~~ Logout <<<

/// What logout did with the stored key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Revocation {
    Revoked,
    AlreadyInvalid,
    WorkspaceManaged,
}

/// `lingohub logout`: revokes and removes the key in the user configuration. A key from
/// `--api-key` or the environment is neither used nor changed.
pub async fn logout(cli: &Cli, key_override: Option<KeyOverride>) -> Result<(), Error> {
    let path = require_config_file()?;
    let Some(key) = stored_api_key(&read_config(&path)?, &path)? else {
        if cli.json {
            output::print_value(
                &json!({"config_file": path.to_string_lossy(), "outcome": "no_stored_key", "revoked": false, "removed": false}),
                true,
            )?;
        } else {
            println!("No API key is stored in {}.", path.display());
        }
        note_unaffected(key_override);
        return Ok(());
    };
    let client = ApiClient::new(&cli.base_url, &key)?;
    let revocation = match client
        .request(
            Method::DELETE,
            "/v2/api-key",
            &[],
            None,
            &RequestOptions::default(),
        )
        .await
    {
        Ok(_) => Revocation::Revoked,
        Err(Error::Api { status: 401, .. }) => Revocation::AlreadyInvalid,
        Err(Error::Api { status: 403, .. }) => Revocation::WorkspaceManaged,
        Err(error) => return Err(error),
    };
    // Read again: the file may have changed while the request ran.
    update_config(&path, None).map_err(|error| {
        Error::Io(io::Error::other(format!(
            "{} it could not be removed from {} ({})",
            if revocation == Revocation::Revoked {
                "The key was revoked, but"
            } else {
                "The key was not revoked, and"
            },
            path.display(),
            cause(&error)
        )))
    })?;
    let (outcome, message) = match revocation {
        Revocation::Revoked => (
            "revoked",
            format!(
                "Signed out; the key was revoked and removed from {}.",
                path.display()
            ),
        ),
        Revocation::AlreadyInvalid => (
            "already_invalid",
            format!(
                "The stored key was no longer valid and has been removed from {}.",
                path.display()
            ),
        ),
        Revocation::WorkspaceManaged => (
            "workspace_managed",
            format!(
                "The stored key was removed from {}, but not revoked: it is managed by the workspace, and only a workspace administrator can revoke it.",
                path.display()
            ),
        ),
    };
    if cli.json {
        output::print_value(
            &json!({
                "config_file": path.to_string_lossy(),
                "outcome": outcome,
                "revoked": revocation == Revocation::Revoked,
                "removed": true,
            }),
            true,
        )?;
    } else {
        println!("{message}");
    }
    note_unaffected(key_override);
    Ok(())
}

fn note_unaffected(key_override: Option<KeyOverride>) {
    match key_override {
        Some(KeyOverride::Flag) => eprintln!("Note: the key passed with --api-key is not affected."),
        Some(KeyOverride::Environment) => {
            eprintln!("Note: LINGOHUB_API_KEY in the environment is not affected.")
        }
        None => {}
    }
}

/// The value of the first assignment, which is the one the startup loading uses, parsed the
/// way it parses it. An empty value counts as no key.
fn stored_api_key(content: &str, path: &Path) -> Result<Option<String>, Error> {
    let Some(line) = lines(content).find(|line| assigns_api_key(line)) else {
        return Ok(None);
    };
    match dotenvy::from_read_iter(line.trim_start_matches('\u{feff}').as_bytes()).next() {
        Some(Ok((_, value))) => Ok(Some(value).filter(|value| !value.trim().is_empty())),
        _ => Err(Error::Input(format!(
            "The {API_KEY_VARIABLE} line in {} cannot be parsed; correct or remove it",
            path.display()
        ))),
    }
}

// >>>

// ~~~~~~~~~~ User configuration file <<<

/// The current configuration; a missing file is empty.
fn read_config(path: &Path) -> Result<String, Error> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|_| Error::Input(format!("{} is not valid UTF-8", path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(Error::Io(io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        ))),
    }
}

/// Sets (`Some`) or removes (`None`) the stored key in the current file.
fn update_config(path: &Path, key: Option<&str>) -> Result<(), Error> {
    let content = read_config(path)?;
    write_config(&write_target(path)?, &with_api_key(&content, key)).map_err(Error::Io)
}

/// An error as the cause within another message, without a second `I/O error:` prefix.
fn cause(error: &Error) -> String {
    match error {
        Error::Io(error) => error.to_string(),
        error => error.to_string(),
    }
}

/// The file to replace. A symlinked configuration is updated where it points, so the link
/// stays in place.
fn write_target(path: &Path) -> Result<PathBuf, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path).map_err(|error| {
            Error::Io(io::Error::new(
                error.kind(),
                format!("cannot resolve {}: {error}", path.display()),
            ))
        }),
        _ => Ok(path.to_path_buf()),
    }
}

/// Replaces `target` with `content` through a new file in the same directory, so a reader
/// never sees half a file. The file is readable only by its owner on Unix, also when the
/// replaced one was not.
fn write_config(target: &Path, content: &str) -> io::Result<()> {
    let directory = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target.file_name().map_or_else(
        || CONFIG_FILE_NAME.into(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut attempt = 0;
    let (temporary, file) = loop {
        let temporary = directory.join(format!("{name}.{}-{attempt}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 32 => attempt += 1,
            Err(error) => return Err(error),
        }
    };
    let written = fill(file, content).and_then(|()| fs::rename(&temporary, target));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

fn fill(mut file: File, content: &str) -> io::Result<()> {
    // The creation mode is reduced by the umask; set it exactly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

/// Physical lines without their line endings.
fn lines(content: &str) -> impl Iterator<Item = &str> {
    content
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches(['\n', '\r']))
}

/// True for a dotenv assignment of `LINGOHUB_API_KEY`, with or without `export`.
fn assigns_api_key(line: &str) -> bool {
    let line = line.trim_start_matches('\u{feff}').trim_start();
    let line = match line.strip_prefix("export") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => line,
    };
    line.strip_prefix(API_KEY_VARIABLE)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// Sets the key in the first assignment (keeping its `export`) and drops any later ones, or
/// appends an assignment; `None` drops every assignment. Every other line stays as it is.
fn with_api_key(content: &str, key: Option<&str>) -> String {
    let mut updated = String::with_capacity(content.len() + key.map_or(0, str::len) + 32);
    let mut stored = false;
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if !assigns_api_key(body) {
            updated.push_str(line);
            continue;
        }
        if let Some(key) = key
            && !stored
        {
            let bom = if body.starts_with('\u{feff}') {
                "\u{feff}"
            } else {
                ""
            };
            let export = if body
                .trim_start_matches('\u{feff}')
                .trim_start()
                .starts_with("export")
            {
                "export "
            } else {
                ""
            };
            let ending = &line[body.len()..];
            updated.push_str(&format!("{bom}{export}{API_KEY_VARIABLE}={key}{ending}"));
            stored = true;
        }
    }
    if let Some(key) = key
        && !stored
    {
        let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push_str(newline);
        }
        updated.push_str(&format!("{API_KEY_VARIABLE}={key}{newline}"));
    }
    updated
}

// >>>

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignments_are_recognized_like_dotenv_reads_them() {
        for line in [
            "LINGOHUB_API_KEY=x",
            "LINGOHUB_API_KEY =x",
            "  LINGOHUB_API_KEY= x",
            "export LINGOHUB_API_KEY=x",
            "export\tLINGOHUB_API_KEY = 'x'",
            "\u{feff}LINGOHUB_API_KEY=x",
            "LINGOHUB_API_KEY=",
        ] {
            assert!(assigns_api_key(line), "{line:?}");
        }
        for line in [
            "# LINGOHUB_API_KEY=x",
            "LINGOHUB_API_KEY_OLD=x",
            "XLINGOHUB_API_KEY=x",
            "exportLINGOHUB_API_KEY=x",
            "LINGOHUB_BASE_URL=http://localhost",
            "LINGOHUB_API_KEY",
            "",
        ] {
            assert!(!assigns_api_key(line), "{line:?}");
        }
    }

    #[test]
    fn storing_replaces_the_first_assignment_and_keeps_everything_else() {
        let content =
            "# keys\nFOO=bar\nexport LINGOHUB_API_KEY=old # comment\nLINGOHUB_API_KEY=older\n\n# end";
        assert_eq!(
            with_api_key(content, Some("new")),
            "# keys\nFOO=bar\nexport LINGOHUB_API_KEY=new\n\n# end"
        );
        assert_eq!(with_api_key(content, None), "# keys\nFOO=bar\n\n# end");
        assert_eq!(
            with_api_key("FOO=bar", Some("new")),
            "FOO=bar\nLINGOHUB_API_KEY=new\n"
        );
        assert_eq!(
            with_api_key("FOO=bar\r\n", Some("new")),
            "FOO=bar\r\nLINGOHUB_API_KEY=new\r\n"
        );
        assert_eq!(
            with_api_key("LINGOHUB_API_KEY=old\r\nFOO=bar\r\n", Some("new")),
            "LINGOHUB_API_KEY=new\r\nFOO=bar\r\n"
        );
        assert_eq!(with_api_key("", Some("new")), "LINGOHUB_API_KEY=new\n");
        assert_eq!(with_api_key("", None), "");
        assert_eq!(
            with_api_key("\u{feff}LINGOHUB_API_KEY=old\n", Some("new")),
            "\u{feff}LINGOHUB_API_KEY=new\n"
        );
    }

    #[test]
    fn the_stored_key_is_the_first_assignment_parsed_like_dotenv() {
        let path = Path::new("/home/user/.lingohub");
        let key = |content: &str| stored_api_key(content, path).unwrap();
        assert_eq!(key("FOO=bar\n"), None);
        assert_eq!(key("LINGOHUB_API_KEY=\n"), None);
        assert_eq!(
            key("LINGOHUB_API_KEY=first\nLINGOHUB_API_KEY=second\n"),
            Some("first".into())
        );
        assert_eq!(
            key("export LINGOHUB_API_KEY='quoted' # note\n"),
            Some("quoted".into())
        );
        assert_eq!(key("LINGOHUB_API_KEY=\"double\"\r\n"), Some("double".into()));
        assert!(stored_api_key("LINGOHUB_API_KEY='unterminated\n", path).is_err());
    }

    #[test]
    fn device_names_are_bounded_like_the_api_measures_them() {
        assert_eq!(truncate_name(&"a".repeat(120)), "a".repeat(100));
        // A character outside the Basic Multilingual Plane counts twice.
        assert_eq!(truncate_name(&"😀".repeat(60)), "😀".repeat(50));
        assert_eq!(checked_name("  Laptop  ").unwrap(), "Laptop");
        for invalid in ["", "   ", "a\u{1b}[31m", &"a".repeat(101), &"😀".repeat(51)] {
            assert!(checked_name(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn only_plain_web_links_are_opened() {
        assert!(verification_url("https://app.lingohub.com/?authentication-token=abc").is_some());
        assert!(verification_url("http://localhost:3000/?authentication-token=abc").is_some());
        for invalid in [
            "http://app.lingohub.com/",
            "https://user:pass@app.lingohub.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(verification_url(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn only_keys_that_dotenv_reads_back_unchanged_are_stored() {
        assert!(storable_key("lh-api_eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln-_"));
        for invalid in ["", "a b", "a#b", "a$b", "a\"b", "a'b", "a\\b", "a\nb"] {
            assert!(!storable_key(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn configuration_is_replaced_atomically_and_privately() {
        let directory = std::env::temp_dir().join(format!("lingohub-config-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "FOO=bar\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_config(&path, "FOO=bar\nLINGOHUB_API_KEY=new\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "FOO=bar\nLINGOHUB_API_KEY=new\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            fs::read_dir(&directory).unwrap().count(),
            1,
            "no temporary file is left"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

// vim: foldmethod=marker foldmarker=<<<,>>>
