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

/// Check outcomes independently of output so CLI and MCP report the same partial failures.
pub fn validate_expected_body(
    body: Option<&Value>,
    expected: Option<ExpectedResponse>,
    input: Option<&Value>,
) -> Result<(), Error> {
    let bulk = match expected {
        Some(ExpectedResponse::Bulk(kind)) => Some(kind),
        _ => None,
    };
    if let Some(body) = body {
        if bulk.is_some() && body.get("object_type").and_then(Value::as_str) != Some("BULK_RESULT") {
            return Err(Error::Protocol("Expected a BULK_RESULT response. Inspect the full response and reconcile affected resources before retrying.".into()));
        }
        if bulk.is_some() {
            require_bulk_correlation(body, input)?;
        }
        require_bulk_success(body, bulk)?;
        if matches!(expected, Some(ExpectedResponse::LinaTranslation)) {
            require_lina_translation_success(body, input)?;
        }
    } else if let Some(expected) = expected {
        let kind = match expected {
            ExpectedResponse::Bulk(_) => "BULK_RESULT",
            ExpectedResponse::LinaTranslation => "LINA_TRANSLATE_RESULT",
        };
        return Err(Error::Protocol(format!(
            "Expected a {kind} response, but the server returned no body. Reconcile uncertain outcomes before retrying."
        )));
    }
    Ok(())
}

fn require_lina_translation_success(body: &Value, input: Option<&Value>) -> Result<(), Error> {
    let malformed = || {
        Error::Protocol("Malformed LINA_TRANSLATE_RESULT: expected one result per input key and a consistent overall status. Inspect the full response; inspect it before retrying.".into())
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
    for (result, source) in results.iter().zip(expected) {
        if source.get("key").and_then(Value::as_str).is_none() || result.get("key") != source.get("key") {
            return Err(malformed());
        }
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

fn require_bulk_correlation(body: &Value, input: Option<&Value>) -> Result<(), Error> {
    let malformed = || {
        Error::Protocol("Malformed BULK_RESULT: results must match every submitted item's index and reference exactly. Inspect the full response; reconcile affected resources before retrying.".into())
    };
    let expected = input
        .and_then(|value| value.get("items"))
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    let results = body.get("data").and_then(Value::as_array).ok_or_else(malformed)?;
    if results.len() != expected.len() {
        return Err(malformed());
    }
    for (index, (result, item)) in results.iter().zip(expected).enumerate() {
        let reference = item
            .get("reference")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        if result.get("index").and_then(Value::as_u64) != Some(index as u64)
            || result.get("reference").and_then(Value::as_str) != Some(reference)
        {
            return Err(malformed());
        }
    }
    Ok(())
}

fn require_bulk_success(body: &Value, bulk: Option<BulkResultKind>) -> Result<(), Error> {
    if body.get("object_type").and_then(Value::as_str) != Some("BULK_RESULT") {
        return Ok(());
    }
    let malformed = || {
        Error::Protocol(
        "Malformed BULK_RESULT: expected a nonempty data array with an outcome string per item. Inspect the full response; reconcile affected resources before retrying.".into()
    )
    };
    let items = body
        .get("data")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(malformed)?;
    let (mut succeeded, mut failed, mut unknown, mut not_processed, mut unrecognized) = (0, 0, 0, 0, 0);
    for item in items {
        require_bulk_item_shape(item, bulk)?;
        match item
            .get("outcome")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?
        {
            "SUCCEEDED" => succeeded += 1,
            "FAILED" => failed += 1,
            "UNKNOWN" => unknown += 1,
            "NOT_PROCESSED" => not_processed += 1,
            _ => unrecognized += 1,
        }
    }
    if succeeded == items.len() {
        Ok(())
    } else {
        Err(Error::BulkIncomplete {
            succeeded,
            failed,
            unknown,
            not_processed,
            unrecognized,
        })
    }
}

fn require_bulk_item_shape(item: &Value, bulk: Option<BulkResultKind>) -> Result<(), Error> {
    let malformed = || {
        Error::Protocol("Malformed BULK_RESULT item: required fields or outcome-specific result types are invalid. Inspect the full response; reconcile affected resources before retrying.".into())
    };
    if [
        "index",
        "reference",
        "outcome",
        "status",
        "data",
        "error",
        "location",
        "etag",
    ]
    .iter()
    .any(|field| item.get(field).is_none())
        || item["index"].as_u64().is_none()
        || item["reference"].as_str().is_none()
    {
        return Err(malformed());
    }
    let nonempty = |value: &Value| value.as_str().is_some_and(|text| !text.is_empty());
    let empty_result = || item["data"].is_null() && item["location"].is_null() && item["etag"].is_null();
    let valid = match item["outcome"].as_str() {
        Some("SUCCEEDED") => {
            bulk.is_some_and(|kind| {
                item["status"]
                    .as_u64()
                    .zip(item["data"]["object_type"].as_str())
                    .is_some_and(|(status, object_type)| kind.accepts_success(status, object_type))
            }) && item["data"].is_object()
                && nonempty(&item["data"]["id"])
                && nonempty(&item["data"]["object_type"])
                && item["error"].is_null()
                && nonempty(&item["location"])
                && nonempty(&item["etag"])
        }
        Some("FAILED" | "UNKNOWN") => {
            let range = if item["outcome"] == "FAILED" {
                400..500
            } else {
                500..600
            };
            item["status"]
                .as_u64()
                .is_some_and(|status| range.contains(&status))
                && empty_result()
                && item["error"].is_object()
                && item["error"]["status"] == item["status"]
                && nonempty(&item["error"]["code"])
        }
        Some("NOT_PROCESSED") => item["status"].is_null() && item["error"].is_null() && empty_result(),
        Some(_) => true, // Preserve unknown future results, but they still produce a nonzero exit.
        None => false,
    };
    if valid { Ok(()) } else { Err(malformed()) }
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
