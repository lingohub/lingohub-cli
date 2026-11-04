// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

//! Release checks, the daily update notice and `lingohub upgrade`.
//!
//! Releases are the GitHub releases of the repository named in Cargo.toml. Homebrew owns
//! the files it installs, so a Homebrew installation is only told how to upgrade. Any
//! other installation is upgraded by running that release's installer into the directory
//! that holds the running executable, which is what the installer did originally.

use crate::{
    cli::{
        output,
        parse::{ApiAction, Cli, Commands},
    },
    colors::styled,
    error::Error,
    transport::is_loopback,
};
use crossterm::style::Stylize;
use reqwest::{Client, Response, Url, header::ACCEPT, redirect};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    ffi::OsString,
    fmt,
    fs::{self, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Set to any value to turn off the daily update notice.
pub const NO_UPDATE_CHECK_ENV: &str = "LINGOHUB_NO_UPDATE_CHECK";
/// Test seam: a loopback origin that serves both the release API and release downloads.
const RELEASES_URL_ENV: &str = "LINGOHUB_RELEASES_URL";
const CHECK_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_INSTALLER_BYTES: usize = 1024 * 1024;
#[cfg(windows)]
const INSTALLER: &str = "lingohub-installer.ps1";
#[cfg(not(windows))]
const INSTALLER: &str = "lingohub-installer.sh";

/// A release version; build metadata is ignored and a prerelease sorts before its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    numbers: (u64, u64, u64),
    prerelease: Option<String>,
}

impl Version {
    /// Accepts `1.2.3`, `v1.2.3` and `1.2.3-rc.1`. Anything else is rejected, so text from a
    /// release tag can never inject terminal sequences into a notice.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (core, prerelease) = match text.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (text, None),
        };
        let mut numbers = core.split('.').map(|part| {
            (!part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        });
        let parsed = (numbers.next()??, numbers.next()??, numbers.next()??);
        if numbers.next().is_some() {
            return None;
        }
        if let Some(prerelease) = prerelease
            && (prerelease.is_empty()
                || prerelease.len() > 64
                || !prerelease
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
        {
            return None;
        }
        Some(Self {
            numbers: parsed,
            prerelease: prerelease.map(str::to_owned),
        })
    }

    fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("the package version is a release version")
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers
            .cmp(&other.numbers)
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_prerelease(a, b),
            })
    }
}

/// SemVer precedence: identifiers compare left to right, numeric ones numerically and below
/// alphanumeric ones, and a shorter list sorts first when it is a prefix of the other.
fn compare_prerelease(left: &str, right: &str) -> Ordering {
    let numeric = |identifier: &str| identifier.bytes().all(|b| b.is_ascii_digit());
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        let order = match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => match (numeric(a), numeric(b)) {
                // Lengths first: digit strings of any size compare without overflow.
                (true, true) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => a.cmp(b),
            },
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (major, minor, patch) = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        if let Some(prerelease) = &self.prerelease {
            write!(f, "-{prerelease}")?;
        }
        Ok(())
    }
}

/// How the running executable was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installation {
    Homebrew,
    /// Installed by the release installer or placed manually; holds the executable path.
    Standalone(PathBuf),
}

impl Installation {
    fn detect() -> Result<Self, Error> {
        let executable = std::env::current_exe()?;
        // Resolve symlinks (Homebrew links bin/lingohub into its Cellar). Windows keeps the
        // reported path, because canonical Windows paths carry a \\?\ prefix installers reject.
        #[cfg(not(windows))]
        let executable = executable.canonicalize()?;
        Ok(Self::classify(executable))
    }

    pub fn classify(executable: PathBuf) -> Self {
        if executable
            .components()
            .any(|component| component.as_os_str() == "Cellar")
        {
            Self::Homebrew
        } else {
            Self::Standalone(executable)
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Homebrew => "homebrew",
            Self::Standalone(_) => "standalone",
        }
    }

    fn upgrade_command(&self) -> &'static str {
        match self {
            Self::Homebrew => "brew upgrade lingohub",
            Self::Standalone(_) => "lingohub upgrade",
        }
    }
}

/// Where release metadata and installers come from.
struct Releases {
    api: Url,
    downloads: Url,
    repository: String,
}

impl Releases {
    fn configured() -> Result<Self, Error> {
        let repository = env!("CARGO_PKG_REPOSITORY")
            .strip_prefix("https://github.com/")
            .filter(|path| {
                path.split('/').count() == 2
                    && path
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
            })
            .expect("Cargo.toml names a GitHub repository")
            .to_owned();
        match std::env::var(RELEASES_URL_ENV) {
            Ok(origin) if !origin.is_empty() => {
                let origin = Url::parse(&origin)
                    .ok()
                    .filter(|url| is_loopback(url) && url.path() == "/" && url.query().is_none())
                    .ok_or_else(|| Error::Input(format!("{RELEASES_URL_ENV} must be a loopback origin")))?;
                Ok(Self {
                    api: origin.clone(),
                    downloads: origin,
                    repository,
                })
            }
            _ => Ok(Self {
                api: Url::parse("https://api.github.com/").expect("valid URL"),
                downloads: Url::parse("https://github.com/").expect("valid URL"),
                repository,
            }),
        }
    }

    fn latest_url(&self) -> Url {
        self.api
            .join(&format!("repos/{}/releases/latest", self.repository))
            .expect("valid release URL")
    }

    fn installer_url(&self, version: &Version) -> Url {
        self.downloads
            .join(&format!(
                "{}/releases/download/v{version}/{INSTALLER}",
                self.repository
            ))
            .expect("valid installer URL")
    }

    fn page(&self) -> String {
        format!("https://github.com/{}/releases/latest", self.repository)
    }
}

/// An unauthenticated client: the LingoHub API key is never sent to the release host.
fn release_client(timeout: Duration) -> Result<Client, Error> {
    Client::builder()
        .user_agent(concat!("lingohub-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10).min(timeout))
        // GitHub serves release assets through an HTTPS redirect to its asset host.
        .redirect(redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .retry(reqwest::retry::never())
        .build()
        .map_err(release_error)
}

async fn latest_version(client: &Client, releases: &Releases) -> Result<Version, Error> {
    let response = client
        .get(releases.latest_url())
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(release_error)?;
    match response.status().as_u16() {
        200 => {}
        404 => return Err(Error::Protocol("No published lingohub release was found".into())),
        403 | 429 => {
            return Err(Error::Network(
                "GitHub refused the release lookup (rate limit); try again later".into(),
            ));
        }
        status => {
            return Err(Error::Protocol(format!(
                "The release lookup returned HTTP {status}"
            )));
        }
    }
    let body = read_bounded(response, MAX_METADATA_BYTES).await?;
    serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("tag_name")?.as_str().and_then(Version::parse))
        .ok_or_else(|| Error::Protocol("The latest release has no valid version tag".into()))
}

async fn read_bounded(mut response: Response, limit: usize) -> Result<Vec<u8>, Error> {
    if response.content_length().is_some_and(|size| size > limit as u64) {
        return Err(Error::Protocol("The release response is too large".into()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(release_error)? {
        if chunk.len() > limit - body.len() {
            return Err(Error::Protocol("The release response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn release_error(error: reqwest::Error) -> Error {
    Error::Network(
        if error.is_timeout() {
            "The release lookup timed out"
        } else if error.is_connect() {
            "Could not connect to GitHub to look up releases"
        } else {
            "The release request failed"
        }
        .into(),
    )
}

/// `lingohub upgrade [--check]`.
pub async fn run(check_only: bool, cli: &Cli) -> Result<(), Error> {
    let releases = Releases::configured()?;
    let client = release_client(Duration::from_secs(60))?;
    let installation = Installation::detect()?;
    let current = Version::current();
    let latest = latest_version(&client, &releases).await?;
    let available = latest > current;
    let report = |upgraded: bool| {
        json!({
            "installed_version": current.to_string(),
            "latest_version": latest.to_string(),
            "update_available": available && !upgraded,
            "installation": installation.name(),
            "upgraded": upgraded,
        })
    };
    if !available || check_only {
        if cli.json {
            return output::print_value(&report(false), true);
        }
        if available {
            println!(
                "lingohub {latest} is available (installed: {current}). Upgrade with: {}",
                installation.upgrade_command()
            );
        } else {
            println!("lingohub {current} is the latest version.");
        }
        return Ok(());
    }
    let Installation::Standalone(executable) = &installation else {
        return Err(Error::Input(format!(
            "lingohub {latest} is available, but this copy was installed with Homebrew. Upgrade with: brew upgrade lingohub"
        )));
    };
    let directory = executable
        .parent()
        .ok_or_else(|| Error::Input("Cannot determine the installation directory".into()))?;
    require_writable(directory, &releases)?;
    let response = client
        .get(releases.installer_url(&latest))
        .send()
        .await
        .map_err(release_error)?;
    if response.status().as_u16() != 200 {
        return Err(Error::Protocol(format!(
            "Downloading the {latest} installer returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let script = read_bounded(response, MAX_INSTALLER_BYTES).await?;
    if script.is_empty() {
        return Err(Error::Protocol(format!("The {latest} installer is empty")));
    }
    replace_installation(&script, directory, executable, &latest)?;
    if cli.json {
        output::print_value(&report(true), true)
    } else {
        println!("Upgraded lingohub {current} to {latest}.");
        Ok(())
    }
}

fn installed_version(executable: &Path) -> Option<Version> {
    let output = Command::new(executable).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .strip_prefix("lingohub ")
        .and_then(Version::parse)
}

fn require_writable(directory: &Path, releases: &Releases) -> Result<(), Error> {
    let probe = directory.join(format!(".lingohub-upgrade-{}", std::process::id()));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(probe);
            Ok(())
        }
        Err(_) => Err(Error::Input(format!(
            "Cannot write to {}; rerun the installer with the needed permissions or download lingohub from {}",
            directory.display(),
            releases.page()
        ))),
    }
}

/// Upgrades `executable` in place, or leaves it unchanged.
///
/// The running binary is first moved aside as `<name>.previous`: every platform allows
/// renaming a running executable, and the startup cleanup never touches that name, so the new
/// binary can be started for the version check without endangering the backup. Success
/// requires the new binary to report `latest`. An installer that fails (even after writing),
/// writes nothing or installs another version is undone by moving the backup back.
fn replace_installation(
    script: &[u8],
    directory: &Path,
    executable: &Path,
    latest: &Version,
) -> Result<(), Error> {
    let backup = sibling(executable, "previous");
    let _ = fs::remove_file(&backup);
    fs::rename(executable, &backup)?;
    let outcome = run_installer(script, directory).and_then(|()| match installed_version(executable) {
        Some(installed) if &installed == latest => Ok(()),
        installed => Err(format!(
            "The installer finished, but {} reported {} instead of {latest}",
            executable.display(),
            installed.map_or_else(|| "no version".to_owned(), |version| version.to_string())
        )),
    });
    match outcome {
        Ok(()) => {
            discard(executable, &backup);
            Ok(())
        }
        Err(problem) => {
            restore(executable, &backup).map_err(|failure| {
                Error::Protocol(format!(
                    "{problem}, and {failure}; the previous lingohub was kept as {}",
                    backup.display()
                ))
            })?;
            Err(Error::Protocol(format!(
                "{problem}; the installed version is unchanged"
            )))
        }
    }
}

/// Writes the installer to a newly created temporary file (an existing file or symlink at
/// that path is refused) and runs it into `directory` without changing PATH again. The shell
/// installer also checks the archive checksum where it can.
fn run_installer(script: &[u8], directory: &Path) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!(
        "lingohub-installer-{}-{}{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
        if cfg!(windows) { ".ps1" } else { ".sh" }
    ));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(script))
        .map_err(|error| format!("The installer could not be saved ({error})"))
        .and_then(|()| {
            installer_command(&path, directory)
                .status()
                .map_err(|error| format!("The installer could not be started ({error})"))
        })
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err("The installer failed".to_owned())
            }
        });
    let _ = fs::remove_file(&path);
    result
}

/// Moves the backup back over whatever the installer left. The error names what could not be
/// undone, so the message never claims an unchanged installation that is not.
fn restore(executable: &Path, backup: &Path) -> Result<(), String> {
    if fs::symlink_metadata(executable).is_ok() {
        fs::remove_file(executable).map_err(|_| format!("{} could not be removed", executable.display()))?;
    }
    fs::rename(backup, executable).map_err(|_| "the previous binary could not be moved back".to_owned())
}

/// Deletes the backup after a successful upgrade. Windows cannot delete the still running
/// previous binary, so there it is renamed for `remove_replaced_binary` on a later start.
fn discard(executable: &Path, backup: &Path) {
    if fs::remove_file(backup).is_err() {
        let _ = fs::rename(backup, sibling(executable, "old"));
    }
}

fn sibling(executable: &Path, suffix: &str) -> PathBuf {
    let mut name = executable.file_name().map(OsString::from).unwrap_or_default();
    name.push(".");
    name.push(suffix);
    executable.with_file_name(name)
}

/// An absolute interpreter path, so a manipulated PATH cannot substitute another program.
#[cfg(not(windows))]
fn installer_command(script: &Path, directory: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg(script);
    with_installer_environment(command, directory)
}

/// An absolute interpreter path, so a manipulated PATH cannot substitute another program.
#[cfg(windows)]
fn installer_command(script: &Path, directory: &Path) -> Command {
    let powershell = std::env::var_os("SystemRoot").map_or_else(
        || PathBuf::from("powershell.exe"),
        |root| PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
    );
    let mut command = Command::new(powershell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script);
    with_installer_environment(command, directory)
}

fn with_installer_environment(mut command: Command, directory: &Path) -> Command {
    command
        .env("LINGOHUB_INSTALL_DIR", directory)
        .env("LINGOHUB_NO_MODIFY_PATH", "1")
        // Installer progress goes to stderr so stdout stays reserved for this command's result.
        .stdout(std::io::stderr());
    command
}

/// Deletes the previous binary a Windows upgrade could not delete while it was running; a
/// no-op elsewhere, where the upgrade deletes it right away.
pub fn remove_replaced_binary() {
    #[cfg(windows)]
    if let Ok(executable) = std::env::current_exe() {
        let _ = fs::remove_file(sibling(&executable, "old"));
    }
}

/// A release lookup running in the background while a command runs.
pub struct UpdateCheck {
    task: tokio::task::JoinHandle<Option<Version>>,
    stamp: PathBuf,
    installation: Installation,
}

/// Starts a lookup at most once a day, only for interactive commands that call the API.
/// It never delays the command: `finish` drops a lookup that has not completed yet.
pub fn start(command: Option<&Commands>, cli: &Cli) -> Option<UpdateCheck> {
    if !command.is_some_and(calls_api)
        || cli.json
        || !std::io::stderr().is_terminal()
        || env_is_set(NO_UPDATE_CHECK_ENV)
        || env_is_set("CI")
    {
        return None;
    }
    let stamp = stamp_path()?;
    if !is_due(read_stamp(&stamp), now()) {
        return None;
    }
    let releases = Releases::configured().ok()?;
    let installation = Installation::detect().ok()?;
    let task = tokio::spawn(async move {
        let client = release_client(Duration::from_secs(5)).ok()?;
        latest_version(&client, &releases).await.ok()
    });
    Some(UpdateCheck {
        task,
        stamp,
        installation,
    })
}

/// Prints a one-line notice on stderr when the completed lookup found a newer release.
pub async fn finish(check: Option<UpdateCheck>) {
    let Some(check) = check else { return };
    if !check.task.is_finished() {
        // Retried on the next command because the stamp is left unchanged.
        check.task.abort();
        return;
    }
    let latest = check.task.await.ok().flatten();
    // A failed lookup also counts, so an unreachable GitHub is not asked on every command.
    write_stamp(&check.stamp, now());
    if let Some(notice) = latest.and_then(|latest| notice(&Version::current(), &latest, &check.installation))
    {
        eprintln!("{}", styled(notice.yellow()));
    }
}

pub fn notice(current: &Version, latest: &Version, installation: &Installation) -> Option<String> {
    (latest > current).then(|| {
        format!(
            "lingohub {latest} is available (installed: {current}). Upgrade with: {}",
            installation.upgrade_command()
        )
    })
}

/// Commands that contact the API anyway. Offline commands, the dashboard and the MCP
/// stdio server never look up releases.
fn calls_api(command: &Commands) -> bool {
    match command {
        Commands::Workspace
        | Commands::Stack { .. }
        | Commands::Project { .. }
        | Commands::StyleGuide { .. } => true,
        Commands::Api { action } => !matches!(
            action,
            ApiAction::Operations | ApiAction::McpTools { .. } | ApiAction::McpServe { .. }
        ),
        Commands::Login { .. }
        | Commands::Logout
        | Commands::Tui
        | Commands::Completion { .. }
        | Commands::Upgrade { .. } => false,
    }
}

fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Per-user cache location of the last lookup time.
fn stamp_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env_dir("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        env_dir("HOME").map(|home| home.join("Library/Caches"))
    } else {
        env_dir("XDG_CACHE_HOME").or_else(|| env_dir("HOME").map(|home| home.join(".cache")))
    }?;
    Some(base.join("lingohub").join("update-check"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn read_stamp(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Due after a day, without a stamp, or when the stamp lies in the future (clock changes).
pub fn is_due(last_check: Option<u64>, now: u64) -> bool {
    last_check.is_none_or(|last| last > now || now - last >= CHECK_INTERVAL_SECONDS)
}

/// Best effort: replace the stamp atomically so concurrent commands never read half a file.
fn write_stamp(path: &Path, seconds: u64) {
    let Some(directory) = path.parent() else { return };
    let temporary = directory.join(format!("update-check.{}.tmp", std::process::id()));
    let written = fs::create_dir_all(directory)
        .and_then(|()| fs::write(&temporary, seconds.to_string()))
        .and_then(|()| fs::rename(&temporary, path));
    if written.is_err() {
        let _ = fs::remove_file(temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_parse_tags_and_reject_everything_else() {
        assert_eq!(version("v1.2.3"), version("1.2.3"));
        assert_eq!(version("1.2.3+build.7"), version("1.2.3"));
        assert_eq!(version("0.2.0-rc.1").to_string(), "0.2.0-rc.1");
        for invalid in [
            "",
            "v",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1..3",
            "-1.2.3",
            "1.2.3-",
            "1.2.3-rc 1",
            "1.2.3-\u{1b}[31m",
            "1234567890.0.0",
            "latest",
        ] {
            assert_eq!(Version::parse(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn versions_order_numerically_with_prereleases_first() {
        assert!(version("0.10.0") > version("0.9.9"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert!(version("0.2.0") > version("0.2.0-rc.2"));
        assert!(version("0.2.0-rc.2") > version("0.2.0-rc.1"));
        assert!(version("0.2.0-rc.1") > version("0.1.9"));
        assert_eq!(version("0.1.0").cmp(&version("v0.1.0")), Ordering::Equal);
    }

    #[test]
    fn prereleases_follow_semver_precedence() {
        // The precedence example from the SemVer specification, plus multi-digit numbers.
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0-rc.2",
            "1.0.0-rc.10",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(version(pair[0]) < version(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(
            version("1.0.0-rc.12345678901234567890").cmp(&version("1.0.0-rc.9")),
            Ordering::Greater
        );
    }

    #[test]
    fn a_failed_upgrade_restores_or_reports_the_saved_binary() {
        let directory = std::env::temp_dir().join(format!("lingohub-restore-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("lingohub.exe");
        let backup = sibling(&executable, "previous");
        assert_eq!(backup, directory.join("lingohub.exe.previous"));
        assert_eq!(
            sibling(&directory.join("lingohub"), "old"),
            directory.join("lingohub.old")
        );

        // Nothing written by the installer: the backup moves back.
        fs::write(&backup, "previous").unwrap();
        restore(&executable, &backup).unwrap();
        assert_eq!(fs::read_to_string(&executable).unwrap(), "previous");
        assert!(!backup.exists());

        // A replacement written before a failure is removed before the backup moves back.
        fs::rename(&executable, &backup).unwrap();
        fs::write(&executable, "partial").unwrap();
        restore(&executable, &backup).unwrap();
        assert_eq!(fs::read_to_string(&executable).unwrap(), "previous");

        // A replacement that cannot be removed is reported; the backup stays in place.
        fs::rename(&executable, &backup).unwrap();
        fs::create_dir(&executable).unwrap();
        fs::write(executable.join("locked"), "").unwrap();
        let failure = restore(&executable, &backup).unwrap_err();
        assert_eq!(failure, format!("{} could not be removed", executable.display()));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "previous");

        // After a successful upgrade the backup disappears.
        fs::remove_dir_all(&executable).unwrap();
        discard(&executable, &backup);
        assert!(!backup.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn homebrew_installations_are_recognized_by_their_cellar() {
        for homebrew in [
            "/opt/homebrew/Cellar/lingohub/0.1.0/bin/lingohub",
            "/usr/local/Cellar/lingohub/0.1.0/bin/lingohub",
            "/home/linuxbrew/.linuxbrew/Cellar/lingohub/0.1.0/bin/lingohub",
        ] {
            assert_eq!(Installation::classify(homebrew.into()), Installation::Homebrew);
        }
        let standalone = PathBuf::from("/home/user/.local/bin/lingohub");
        assert_eq!(
            Installation::classify(standalone.clone()),
            Installation::Standalone(standalone)
        );
    }

    #[test]
    fn notice_names_the_upgrade_command_of_the_installation() {
        let standalone = Installation::Standalone("/home/user/.local/bin/lingohub".into());
        assert_eq!(
            notice(&version("0.1.0"), &version("0.2.0"), &Installation::Homebrew).unwrap(),
            "lingohub 0.2.0 is available (installed: 0.1.0). Upgrade with: brew upgrade lingohub"
        );
        assert_eq!(
            notice(&version("0.1.0"), &version("0.2.0"), &standalone).unwrap(),
            "lingohub 0.2.0 is available (installed: 0.1.0). Upgrade with: lingohub upgrade"
        );
        assert_eq!(notice(&version("0.2.0"), &version("0.2.0"), &standalone), None);
        assert_eq!(notice(&version("0.3.0"), &version("0.2.0"), &standalone), None);
    }

    #[test]
    fn lookups_are_due_once_a_day() {
        let now = 1_800_000_000;
        assert!(is_due(None, now));
        assert!(!is_due(Some(now), now));
        assert!(!is_due(Some(now - CHECK_INTERVAL_SECONDS + 1), now));
        assert!(is_due(Some(now - CHECK_INTERVAL_SECONDS), now));
        assert!(is_due(Some(now + 60), now), "a stamp from the future is ignored");
    }

    #[test]
    fn stamps_round_trip_and_replace_atomically() {
        let directory = std::env::temp_dir().join(format!("lingohub-stamp-{}", std::process::id()));
        let path = directory.join("lingohub").join("update-check");
        assert_eq!(read_stamp(&path), None);
        write_stamp(&path, 42);
        assert_eq!(read_stamp(&path), Some(42));
        write_stamp(&path, 43);
        assert_eq!(read_stamp(&path), Some(43));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
