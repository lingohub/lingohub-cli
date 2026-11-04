// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use std::{fmt, io};

#[derive(Debug)]
pub enum Error {
    Api {
        status: u16,
        problem: Value,
        request_id: Option<String>,
        retry_after: Option<String>,
    },
    Network(String),
    Input(String),
    Protocol(String),
    BulkIncomplete {
        succeeded: usize,
        failed: usize,
        unknown: usize,
        not_processed: usize,
        unrecognized: usize,
    },
    TranslationIncomplete {
        succeeded: usize,
        failed: usize,
    },
    Io(io::Error),
    Json(serde_json::Error),
}
impl Error {
    pub fn from_response(status: u16, body: &[u8], headers: &HeaderMap) -> Self {
        let problem = serde_json::from_slice::<Value>(body)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"title":"HTTP request failed", "status":status}));
        Self::Api {
            status,
            problem,
            request_id: headers
                .get("X-Request-Id")
                .and_then(|s| s.to_str().ok())
                .map(str::to_owned),
            retry_after: headers
                .get("Retry-After")
                .and_then(|s| s.to_str().ok())
                .map(str::to_owned),
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Input(_) | Self::Json(_) => 2,
            _ => 1,
        }
    }
    pub fn as_json(&self) -> Value {
        match self {
            Self::Api {
                status,
                problem,
                request_id,
                retry_after,
            } => json!({"status":status,"problem":problem,"request_id":request_id,"retry_after":retry_after}),
            Self::BulkIncomplete {
                succeeded,
                failed,
                unknown,
                not_processed,
                unrecognized,
            } => json!({
                "error": "bulk_incomplete", "succeeded": succeeded, "failed": failed,
                "unknown": unknown, "not_processed": not_processed, "unrecognized": unrecognized,
                "detail": self.to_string()
            }),
            Self::TranslationIncomplete { succeeded, failed } => {
                json!({"error":"translation_incomplete", "succeeded":succeeded, "failed":failed, "detail":self.to_string()})
            }
            _ => json!({"error":self.to_string()}),
        }
    }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api {
                status,
                problem,
                request_id,
                retry_after,
            } => {
                write!(
                    f,
                    "HTTP {status}: {}",
                    problem
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("API error")
                )?;
                for key in ["detail", "code"] {
                    if let Some(value) = problem.get(key).and_then(Value::as_str) {
                        write!(f, "\n{key}: {value}")?;
                    }
                }
                if let Some(errors) = problem.get("errors").and_then(Value::as_array) {
                    for error in errors {
                        write!(
                            f,
                            "\n{}: {}",
                            error.get("pointer").and_then(Value::as_str).unwrap_or(""),
                            error.get("detail").and_then(Value::as_str).unwrap_or("")
                        )?;
                    }
                }
                if let Some(id) = problem
                    .get("request_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .or(request_id.as_deref())
                {
                    write!(f, "\nrequest_id: {id}")?;
                }
                if let Some(delay) = retry_after {
                    write!(f, "\nretry_after: {delay}")?;
                }
                Ok(())
            }
            Self::Network(s) | Self::Input(s) | Self::Protocol(s) => f.write_str(s),
            Self::BulkIncomplete {
                succeeded,
                failed,
                unknown,
                not_processed,
                unrecognized,
            } => write!(
                f,
                "Bulk result: {succeeded} succeeded, {failed} failed, {unknown} unknown, {not_processed} not processed, {unrecognized} unrecognized. Successful items are already committed. Full results are on stdout. Reconcile uncertain outcomes before starting new work; no items were automatically retried."
            ),
            Self::TranslationIncomplete { succeeded, failed } => write!(
                f,
                "Translation result: {succeeded} succeeded, {failed} failed. Full keyed results are on stdout. Completed work may have consumed credits; retry only deliberately selected unfinished inputs. No inputs were automatically retried."
            ),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}
impl std::error::Error for Error {}
