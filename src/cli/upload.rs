// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use super::input::read_regular_file;
use crate::{error::Error, transport::MultipartUpload};
use serde_json::Value;
use std::{collections::HashSet, path::Path};

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FORM_BYTES: usize = 64 * 1024;

/// Read explicitly named JSON files; ordinary --form values remain literal, including @ prefixes.
pub fn prepare_with_files(
    path: &str,
    fields: &[(String, String)],
    files: &[(String, String)],
    schema: &Value,
) -> Result<MultipartUpload, Error> {
    let mut combined = fields.to_vec();
    let mut total = fields
        .iter()
        .map(|(name, value)| name.len() + value.len())
        .sum::<usize>();
    for (name, file_path) in files {
        if !matches!(
            schema["properties"][name]["type"].as_str(),
            Some("array" | "object")
        ) {
            return Err(Error::Input(format!(
                "Multipart field {name} does not accept a JSON file"
            )));
        }
        let value = String::from_utf8(read_regular_file(Path::new(file_path), MAX_FORM_BYTES as u64)?)
            .map_err(|_| Error::Input("Multipart JSON file must be readable UTF-8".into()))?;
        total = total.saturating_add(name.len()).saturating_add(value.len());
        if total > MAX_FORM_BYTES {
            return Err(Error::Input("Multipart text fields exceed 64 KiB".into()));
        }
        combined.push((name.clone(), value));
    }
    prepare(path, &combined, schema)
}

/// Validate against the bundled form schema before opening the selected regular file.
pub fn prepare(path: &str, fields: &[(String, String)], schema: &Value) -> Result<MultipartUpload, Error> {
    let properties = schema["properties"]
        .as_object()
        .ok_or_else(|| Error::Protocol("Missing multipart form schema".into()))?;
    if properties.get("file").and_then(|s| s["format"].as_str()) != Some("binary") {
        return Err(Error::Input(
            "This upload command requires a single binary file part".into(),
        ));
    }
    let mut json_fields = HashSet::new();
    let mut names = HashSet::from(["file"]);
    let mut total = 0usize;
    for (name, value) in fields {
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') || !names.insert(name.as_str()) {
            return Err(Error::Input("Invalid or duplicate multipart field name".into()));
        }
        let field = properties
            .get(name)
            .ok_or_else(|| Error::Input(format!("Unknown multipart field: {name}")))?;
        match field["type"].as_str() {
            Some("string") if field["format"] != "binary" => (),
            Some("boolean") if matches!(value.as_str(), "true" | "false") => (),
            Some(kind @ ("array" | "object")) => {
                let parsed: Value = serde_json::from_str(value)
                    .map_err(|_| Error::Input(format!("Invalid JSON multipart field: {name}")))?;
                if (kind == "array" && !parsed.is_array()) || (kind == "object" && !parsed.is_object()) {
                    return Err(Error::Input(format!("Expected JSON {kind} for {name}")));
                }
                json_fields.insert(name.as_str());
            }
            _ => return Err(Error::Input(format!("Invalid multipart value type for {name}"))),
        }
        total = total.saturating_add(name.len()).saturating_add(value.len());
        if total > MAX_FORM_BYTES {
            return Err(Error::Input("Multipart text fields exceed 64 KiB".into()));
        }
    }
    if let Some(required) = schema["required"].as_array() {
        for name in required.iter().filter_map(Value::as_str) {
            if !names.contains(name) {
                return Err(Error::Input(format!("Missing multipart field: {name}")));
            }
        }
    }
    let path = Path::new(path);
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && !n.chars().any(|c| c.is_control() || matches!(c, '"' | '\\')))
        .ok_or_else(|| {
            Error::Input(
                "Upload filename must be UTF-8 without quotes, backslashes or control characters".into(),
            )
        })?;
    let bytes = read_regular_file(path, MAX_FILE_BYTES)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(Error::Input(
            "Upload must contain between 1 byte and 64 MiB".into(),
        ));
    }
    // Scan each part once so many occupied candidates cannot cause quadratic rescanning.
    let boundary = unused_boundary(
        std::iter::once(bytes.as_slice())
            .chain(std::iter::once(filename.as_bytes()))
            .chain(
                fields
                    .iter()
                    .flat_map(|(name, value)| [name.as_bytes(), value.as_bytes()]),
            ),
    );
    let mut body = Vec::with_capacity(bytes.len() + total + 1024);
    for (name, value) in fields {
        let content_type = if json_fields.contains(name.as_str()) {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        };
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok(MultipartUpload {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        body,
    })
}

fn unused_boundary<'a>(parts: impl Iterator<Item = &'a [u8]>) -> String {
    const PREFIX: &[u8] = b"lingohub-cli-multipart-";
    let mut occupied = HashSet::new();
    for part in parts {
        for candidate in part.windows(PREFIX.len() + 16) {
            if candidate.starts_with(PREFIX) {
                let suffix = &candidate[PREFIX.len()..];
                if suffix
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                {
                    let hex = std::str::from_utf8(suffix).expect("ASCII hex");
                    occupied.insert(u64::from_str_radix(hex, 16).expect("sixteen hex digits"));
                }
            }
        }
    }
    let candidate = (0u64..)
        .find(|i| !occupied.contains(i))
        .expect("bounded input has an unused boundary");
    format!("lingohub-cli-multipart-{candidate:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundary_skips_many_occupied_candidates_across_parts() {
        let first = (0..10_000)
            .map(|i| format!("lingohub-cli-multipart-{i:016x}"))
            .collect::<String>();
        let second = b"lingohub-cli-multipart-0000000000002710";
        assert_eq!(
            unused_boundary([first.as_bytes(), second].into_iter()),
            "lingohub-cli-multipart-0000000000002711"
        );
    }
}
