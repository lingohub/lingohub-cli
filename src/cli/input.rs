// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{error::Error, prompt};
use serde_json::{Map, Value};
use std::io::Read;

const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

pub fn read_body_source(path: &str) -> Result<String, Error> {
    let bytes = if path == "-" {
        read_limited(std::io::stdin().lock(), MAX_BODY_BYTES)?
    } else {
        read_regular_file(std::path::Path::new(path), MAX_BODY_BYTES)?
    };
    String::from_utf8(bytes).map_err(|_| Error::Input("Request body must be UTF-8".into()))
}

fn read_limited(reader: impl Read, limit: u64) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Input(format!("Input exceeds the {limit}-byte limit")));
    }
    Ok(bytes)
}

/// Bound opening and reading too: a path can change into a FIFO after a metadata check.
pub fn read_regular_file(path: &std::path::Path, limit: u64) -> Result<Vec<u8>, Error> {
    read_regular_file_with_timeout(path, limit, std::time::Duration::from_secs(10))
}

fn read_regular_file_with_timeout(
    path: &std::path::Path,
    limit: u64,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, Error> {
    let path = path.to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    // A detached OS thread cannot hold the async runtime open after a timed-out CLI command exits.
    std::thread::Builder::new()
        .name("input-file".into())
        .spawn(move || {
            let result = (|| {
                let file =
                    std::fs::File::open(path).map_err(|_| Error::Input("Cannot open input file".into()))?;
                let metadata = file
                    .metadata()
                    .map_err(|_| Error::Input("Cannot inspect input file".into()))?;
                if !metadata.is_file() || metadata.len() > limit {
                    return Err(Error::Input(format!(
                        "Input must be a regular file of at most {limit} bytes"
                    )));
                }
                read_limited(file, limit)
                    .map_err(|_| Error::Input(format!("Cannot read input within the {limit}-byte limit")))
            })();
            let _ = sender.send(result);
        })?;
    receiver
        .recv_timeout(timeout)
        .map_err(|_| Error::Input("Opening or reading the input file timed out".into()))?
}

pub fn request_body(file: Option<&str>, fields: &[(String, String)]) -> Result<Option<Value>, Error> {
    if let Some(path) = file {
        return Ok(Some(serde_json::from_str(&read_body_source(path)?)?));
    }
    if fields.is_empty() {
        return Ok(None);
    }
    let mut body = Map::new();
    for (key, value) in fields {
        if body.insert(key.clone(), serde_json::from_str(value)?).is_some() {
            return Err(Error::Input(format!("Duplicate request field: {key}")));
        }
    }
    Ok(Some(Value::Object(body)))
}

/// Prompt only for current top-level fields. Files preserve nested values and explicit nulls.
pub fn interactive_body(resource: &str, create: bool) -> Result<Value, Error> {
    let fields: &[&str] = match (resource, create) {
        ("stacks", true) => &["name", "color", "type"],
        ("stacks", false) => &["name", "color"],
        ("projects", true) => &["name", "type", "source_language_code", "target_language_codes"],
        ("projects", false) => &["name", "color", "website", "ai_context"],
        ("style-guides", true) => &["name"],
        ("style-guides", false) => &["name", "ai_context", "audience_description"],
        _ => return Err(Error::Input("Request body required via --file".into())),
    };
    let mut values = Map::new();
    if !create {
        eprintln!("Leave blank to retain a field; --file supports explicit null and nested rules.");
    }
    for field in fields {
        let text = loop {
            let text = prompt::prompt(if *field == "target_language_codes" {
                "target_language_codes (comma-separated)"
            } else {
                field
            })?;
            if !create || !text.is_empty() {
                break text;
            }
            eprintln!("{field} is required");
        };
        if !create && text.is_empty() {
            continue;
        }
        values.insert(
            (*field).into(),
            if *field == "target_language_codes" {
                Value::Array(
                    text.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| Value::String(s.into()))
                        .collect(),
                )
            } else {
                Value::String(text)
            },
        );
    }
    Ok(Value::Object(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_limit_accepts_exact_input_and_stops_infinite_reader() {
        assert_eq!(read_limited(&b"1234"[..], 4).unwrap(), b"1234");
        assert!(matches!(
            read_limited(std::io::repeat(b'x'), 4),
            Err(Error::Input(_))
        ));
    }
    #[cfg(unix)]
    #[test]
    fn opening_a_fifo_cannot_stall_the_caller() {
        let path = std::env::temp_dir().join(format!("lingohub-fifo-{}", std::process::id()));
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let result = read_regular_file_with_timeout(&path, 4, std::time::Duration::from_millis(100));
        // Release the detached reader so the test leaves no blocked worker behind.
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        drop(writer);
        std::fs::remove_file(path).unwrap();
        assert!(matches!(result, Err(Error::Input(message)) if message.contains("timed out")));
    }
}
