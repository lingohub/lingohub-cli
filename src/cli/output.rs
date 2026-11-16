// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    colors::{C_BLUE_DARK, C_BLUE_LIGHT, C_GREY, styled},
    contract::{BulkResultKind, ExpectedResponse},
    error::Error,
    transport::ApiResponse,
};
use crossterm::style::Stylize;
use serde_json::Value;

pub fn print_value(body: &Value, json: bool) -> Result<(), Error> {
    if json {
        println!("{}", serde_json::to_string_pretty(body)?);
    } else if let Some(data) = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())
    {
        if let Some(title) = data.first().and_then(resource_title) {
            println!(
                "{} ({} shown)",
                styled(title.with(C_BLUE_DARK).bold()),
                data.len()
            );
            println!();
        }
        for item in data {
            print_item(item);
            println!();
        }
        if body.get("has_more").and_then(Value::as_bool) == Some(true) {
            eprintln!(
                "More results; next_cursor: {}",
                body["next_cursor"].as_str().unwrap_or("")
            );
        }
    } else {
        print_item(body);
    }
    Ok(())
}
fn print_item(body: &Value) {
    if let Some(fields) = body.as_object() {
        if resource_title(body).is_some() {
            let mut rows: Vec<_> = fields.iter().filter(|(key, _)| *key != "object_type").collect();
            rows.sort_by_key(|(key, _)| match key.as_str() {
                "id" => 0,
                "name" => 1,
                _ => 2,
            });
            let rows: Vec<_> = rows
                .into_iter()
                .map(|(key, value)| (field_label(key), value))
                .collect();
            let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
            for (label, value) in rows {
                let label = styled(format!("{label:<width$}").with(C_BLUE_LIGHT));
                if value.is_null() {
                    println!("{label}  {}", styled("—".with(C_GREY)));
                } else {
                    println!(
                        "{label}  {}",
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    );
                }
            }
            return;
        }
        for (key, value) in fields {
            println!(
                "{key}: {}",
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
            );
        }
    } else {
        println!("{body}");
    }
}

fn resource_title(body: &Value) -> Option<&'static str> {
    match body.get("object_type").and_then(Value::as_str) {
        Some("WORKSPACE") => Some("Workspaces"),
        Some("STACK") => Some("Stacks"),
        Some("PROJECT") => Some("Projects"),
        Some("STYLE_GUIDE") => Some("Style Guides"),
        _ => None,
    }
}

fn field_label(key: &str) -> String {
    key.split('_')
        .map(|word| match word {
            "id" | "ai" | "url" => word.to_ascii_uppercase(),
            _ => {
                let mut letters = word.chars();
                letters
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + letters.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn print_error(error: &Error) {
    eprintln!("{} {error}", styled("Error:".red().bold()));
}
pub fn print_response(response: ApiResponse, json: bool, headers: bool) -> Result<(), Error> {
    print_expected_response(response, json, headers, None, None)
}

pub fn print_expected_response(
    response: ApiResponse,
    json: bool,
    headers: bool,
    expected: Option<ExpectedResponse>,
    input: Option<&Value>,
) -> Result<(), Error> {
    if headers {
        print_headers(&response);
    }
    if let Some(body) = &response.body {
        print_value(body, json)?;
    }
    validate_expected_body(response.body.as_ref(), expected, input)
}

/// Check outcomes independently of output so CLI and MCP report the same result problems.
pub fn validate_expected_body(
    body: Option<&Value>,
    expected: Option<ExpectedResponse>,
    input: Option<&Value>,
) -> Result<(), Error> {
    match (expected, body) {
        (None, _) => Ok(()),
        (Some(ExpectedResponse::Bulk(kind)), Some(body)) => require_bulk_list(body, kind, input),
        (Some(ExpectedResponse::LinaTranslation), Some(body)) => {
            require_lina_translation_success(body, input)
        }
        (Some(expected), None) => {
            let kind = match expected {
                ExpectedResponse::Bulk(_) => "bulk LIST",
                ExpectedResponse::LinaTranslation => "LINA_TRANSLATE_RESULT",
            };
            Err(Error::Protocol(format!(
                "Expected a {kind} response, but the server returned no body. Reconcile uncertain outcomes before retrying."
            )))
        }
    }
}

fn require_lina_translation_success(body: &Value, input: Option<&Value>) -> Result<(), Error> {
    let malformed = || {
        Error::Protocol("Malformed LINA_TRANSLATE_RESULT: expected one result per input text, in request order, and a consistent overall status. Inspect the full response; inspect it before retrying.".into())
    };
    let expected = input
        .and_then(|x| x.get("texts"))
        .and_then(Value::as_array)
        .filter(|x| !x.is_empty())
        .ok_or_else(malformed)?;
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    if body["object_type"] != "LINA_TRANSLATE_RESULT" || results.len() != expected.len() {
        return Err(malformed());
    }
    let mut succeeded = 0;
    // Results carry no identity of their own: `results[i]` belongs to `texts[i]`.
    for result in results {
        match result.get("is_translated").and_then(Value::as_bool) {
            Some(true) if result.get("text").and_then(Value::as_str).is_some() => succeeded += 1,
            Some(false) if result.get("text").is_some_and(Value::is_null) => (),
            _ => return Err(malformed()),
        }
    }
    let status = if succeeded == results.len() {
        "SUCCEEDED"
    } else if succeeded == 0 {
        "FAILED"
    } else {
        "PARTIALLY_SUCCEEDED"
    };
    if body.get("status").and_then(Value::as_str) != Some(status)
        || (status == "SUCCEEDED" && !body.get("failure").is_some_and(Value::is_null))
        || (status != "SUCCEEDED" && body.get("failure").and_then(Value::as_str).is_none())
    {
        return Err(malformed());
    }
    if succeeded == results.len() {
        Ok(())
    } else {
        Err(Error::TranslationIncomplete {
            succeeded,
            failed: results.len() - succeeded,
        })
    }
}

/// A batch commits all items or none, so success must list one resource per submitted item.
fn require_bulk_list(body: &Value, kind: BulkResultKind, input: Option<&Value>) -> Result<(), Error> {
    let malformed = || {
        Error::Protocol("Malformed bulk result: expected a LIST with one resource of the operation's type per submitted item, in request order. The batch may be committed. Inspect the full response; reconcile affected resources before retrying.".into())
    };
    let object_type = kind.object_type().ok_or_else(malformed)?;
    let submitted = input
        .and_then(|value| value.get("items"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(malformed)?;
    let results = body.get("data").and_then(Value::as_array).ok_or_else(malformed)?;
    if body.get("object_type").and_then(Value::as_str) != Some("LIST") || results.len() != submitted.len() {
        return Err(malformed());
    }
    for (result, item) in results.iter().zip(submitted) {
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if result.get("object_type").and_then(Value::as_str) != Some(object_type) || id.is_none() {
            return Err(malformed());
        }
        // Segment batches name their targets; created keys receive new identities.
        if kind == BulkResultKind::SegmentMutation && item.get("segment_id").and_then(Value::as_str) != id {
            return Err(malformed());
        }
    }
    Ok(())
}

pub fn print_headers(response: &ApiResponse) {
    eprintln!("HTTP {}", response.status);
    for key in [
        "cache-control",
        "etag",
        "last-modified",
        "location",
        "retry-after",
        "x-request-id",
        "idempotent-replayed",
    ] {
        if let Some(value) = response.headers.get(key).and_then(|x| x.to_str().ok()) {
            eprintln!("{key}: {value}");
        }
    }
}
