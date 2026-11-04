// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    cli::{output, parse::Cli},
    contract,
    error::Error,
    transport::{ApiClient, RequestOptions, collection_page, is_loopback},
};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// Refresh export metadata, then retrieve its artifact without forwarding API credentials.
pub async fn run(
    client: &ApiClient,
    export_id: &str,
    destination: &str,
    max_bytes: u64,
    cli: &Cli,
) -> Result<(), Error> {
    let (_, path) = contract::resolve("getExport", &[("exportId".into(), export_id.into())])?;
    let mut pending = PendingFile::create(Path::new(destination))?;
    let metadata = client
        .request(Method::GET, &path, &[], None, &RequestOptions::default())
        .await?;
    if cli.include_headers {
        output::print_headers(&metadata);
    }
    if metadata.status != 200 {
        return Err(Error::Protocol("Expected HTTP 200 export metadata".into()));
    }
    let body = metadata
        .body
        .ok_or_else(|| Error::Protocol("Expected export metadata".into()))?;
    if body["object_type"].as_str() != Some("EXPORT") || body["id"].as_str() != Some(export_id) {
        return Err(Error::Protocol(
            "Export metadata does not match the requested export".into(),
        ));
    }
    match body["status"].as_str() {
        Some("SUCCEEDED") => (),
        Some("QUEUED" | "PROCESSING") => {
            return Err(Error::Protocol(
                "Export is not finished; inspect its status before downloading".into(),
            ));
        }
        Some("FAILED" | "PARTIALLY_SUCCEEDED" | "CANCELLED" | "SKIPPED") => {
            return Err(Error::Protocol(
                "Export did not succeed; inspect its status and entries before downloading".into(),
            ));
        }
        _ => return Err(Error::Protocol("Export has an unrecognized status".into())),
    }
    let url = artifact_url(body.get("download_url").and_then(Value::as_str))?;
    let written = save_artifact(&mut pending, url, destination, max_bytes).await?;
    output::print_value(
        &json!({"export_id": export_id, "output": destination, "bytes_written": written}),
        cli.json,
    )
}

/// Find an exact entry within its export and download only that successful file.
pub async fn run_entry(
    client: &ApiClient,
    export_id: &str,
    entry_id: &str,
    destination: &str,
    max_bytes: u64,
    max_pages: u64,
    cli: &Cli,
) -> Result<(), Error> {
    let (_, path) = contract::resolve("listExportEntries", &[("exportId".into(), export_id.into())])?;
    if entry_id.is_empty() {
        return Err(Error::Input("Export entry ID must not be empty".into()));
    }
    let mut pending = PendingFile::create(Path::new(destination))?;
    let mut query = vec![("limit".into(), "100".into())];
    let mut seen = HashSet::new();
    for _ in 0..max_pages {
        let response = client
            .request(Method::GET, &path, &query, None, &RequestOptions::default())
            .await?;
        if cli.include_headers {
            output::print_headers(&response);
        }
        if response.status != 200 {
            return Err(Error::Protocol("Expected HTTP 200 export entries".into()));
        }
        let body = response
            .body
            .ok_or_else(|| Error::Protocol("Expected export entries".into()))?;
        let (entries, cursor) = collection_page(&body)?;
        if entries.iter().any(|entry| {
            entry["object_type"].as_str() != Some("EXPORT_ENTRY")
                || entry["id"].as_str().is_none_or(str::is_empty)
        }) {
            return Err(Error::Protocol("Expected export entry identities".into()));
        }
        let mut matches = entries
            .iter()
            .filter(|entry| entry["id"].as_str() == Some(entry_id));
        if let Some(entry) = matches.next() {
            if matches.next().is_some() {
                return Err(Error::Protocol(
                    "Server repeated the requested export entry".into(),
                ));
            }
            if entry["status"].as_str() != Some("SUCCEEDED") {
                return Err(Error::Protocol(
                    "Export entry did not succeed; inspect its status before downloading".into(),
                ));
            }
            let url = artifact_url(entry.get("download_url").and_then(Value::as_str))?;
            let written = save_artifact(&mut pending, url, destination, max_bytes).await?;
            return output::print_value(
                &json!({"export_id": export_id, "export_entry_id": entry_id, "output": destination, "bytes_written": written}),
                cli.json,
            );
        }
        let Some(cursor) = cursor else {
            return Err(Error::Protocol(
                "Export entry was not found in the requested export".into(),
            ));
        };
        if !seen.insert(cursor.to_owned()) {
            return Err(Error::Protocol("Server repeated a pagination cursor".into()));
        }
        query.retain(|(key, _)| key != "cursor");
        query.push(("cursor".into(), cursor.into()));
    }
    Err(Error::Protocol(
        "Export entry was not found within --max-pages; increase the bound or inspect the export entries"
            .into(),
    ))
}

/// Refresh a release's computed download subresource without changing its metadata validator.
pub async fn run_release(
    client: &ApiClient,
    release_id: &str,
    destination: &str,
    max_bytes: u64,
    cli: &Cli,
) -> Result<(), Error> {
    let (_, path) = contract::resolve(
        "getDistributionReleaseDownload",
        &[("distributionReleaseId".into(), release_id.into())],
    )?;
    let mut pending = PendingFile::create(Path::new(destination))?;
    let metadata = client
        .request(Method::GET, &path, &[], None, &RequestOptions::default())
        .await?;
    if cli.include_headers {
        output::print_headers(&metadata);
    }
    if metadata.status != 200 {
        return Err(Error::Protocol("Expected HTTP 200 download metadata".into()));
    }
    let body = metadata
        .body
        .ok_or_else(|| Error::Protocol("Expected distribution release download metadata".into()))?;
    if body["object_type"].as_str() != Some("DISTRIBUTION_RELEASE_DOWNLOAD")
        || body["id"].as_str() != Some(release_id)
    {
        return Err(Error::Protocol(
            "Download metadata does not match the requested distribution release".into(),
        ));
    }
    let link = body.get("download_url").and_then(Value::as_str).ok_or_else(|| {
        Error::Protocol(
            "No completed archive is available; inspect the distribution release status before downloading"
                .into(),
        )
    })?;
    let url = artifact_url(Some(link))?;
    let written = save_artifact(&mut pending, url, destination, max_bytes).await?;
    output::print_value(
        &json!({"distribution_release_id": release_id, "output": destination, "bytes_written": written}),
        cli.json,
    )
}

async fn save_artifact(
    pending: &mut PendingFile,
    url: Url,
    destination: &str,
    max_bytes: u64,
) -> Result<u64, Error> {
    let artifact_client = Client::builder()
        .user_agent(concat!("lingohub-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(artifact_network_error)?;
    let mut response = artifact_client
        .get(url)
        .send()
        .await
        .map_err(artifact_network_error)?;
    if response.status().as_u16() != 200 {
        let status = response.status().as_u16();
        return Err(Error::Protocol(if matches!(status, 401 | 403) {
            format!(
                "Artifact download returned HTTP {status}; the link may have expired or access may be denied. Rerun this command to obtain a fresh link"
            )
        } else {
            format!("Artifact download returned HTTP {status}; no file was saved")
        }));
    }
    if response.content_length().is_some_and(|size| size > max_bytes) {
        return Err(Error::Protocol(
            "Artifact exceeds --max-bytes; no file was saved".into(),
        ));
    }
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(artifact_network_error)? {
        if chunk.len() as u64 > max_bytes - written {
            return Err(Error::Protocol(
                "Artifact exceeds --max-bytes; no file was saved".into(),
            ));
        }
        pending
            .file
            .as_mut()
            .expect("pending file is open")
            .write_all(&chunk)
            .map_err(|_| {
                Error::Protocol("Could not write the downloaded artifact; no file was saved".into())
            })?;
        written += chunk.len() as u64;
    }
    pending.publish(Path::new(destination))?;
    Ok(written)
}

fn artifact_url(value: Option<&str>) -> Result<Url, Error> {
    let invalid = || {
        Error::Protocol("Artifact download URL must use HTTPS (or HTTP loopback for local testing), without embedded credentials or a fragment".into())
    };
    let url = Url::parse(
        value.ok_or_else(|| Error::Protocol("Completed export or entry has no download URL".into()))?,
    )
    .map_err(|_| invalid())?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || url.scheme() == "http" && is_loopback(&url))
    {
        return Err(invalid());
    }
    Ok(url)
}

fn artifact_network_error(error: reqwest::Error) -> Error {
    Error::Network(
        if error.is_timeout() {
            "Artifact download timed out; no file was saved"
        } else {
            "Artifact download failed; no file was saved"
        }
        .into(),
    )
}

/// Stage beside the destination and publish with a non-overwriting hard link only after success.
struct PendingFile {
    path: PathBuf,
    file: Option<File>,
}
impl PendingFile {
    fn create(destination: &Path) -> Result<Self, Error> {
        if destination.file_name().is_none() {
            return Err(Error::Input("--output must name a new file".into()));
        }
        match fs::symlink_metadata(destination) {
            Ok(_) => return Err(Error::Input("Output already exists; select a new file".into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return Err(Error::Input("Cannot inspect the output destination".into())),
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..32 {
            let path = parent.join(format!(
                ".lingohub-download-{}-{}.part",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => {
                    return Err(Error::Input(
                        "Cannot create a staging file beside --output".into(),
                    ));
                }
            }
        }
        Err(Error::Input(
            "Cannot allocate a unique staging file beside --output".into(),
        ))
    }
    fn publish(&mut self, destination: &Path) -> Result<(), Error> {
        self.file
            .as_ref()
            .expect("pending file is open")
            .sync_all()
            .map_err(|_| Error::Protocol("Cannot flush the downloaded artifact; no file was saved".into()))?;
        self.file.take();
        fs::hard_link(&self.path, destination).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                Error::Input("Output appeared during download; existing file was preserved".into())
            } else {
                Error::Protocol("Cannot publish the downloaded artifact; the destination filesystem must support hard links".into())
            }
        })?;
        Ok(())
    }
}
impl Drop for PendingFile {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}
