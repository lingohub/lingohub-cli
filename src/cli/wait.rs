// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    cli::{output, parse::Cli},
    contract,
    error::Error,
    transport::{ApiClient, RequestOptions},
};
use reqwest::Method;
use std::time::Duration;

/// Poll only known resource reads, retaining the last representation on failure or timeout.
pub async fn run(
    client: &ApiClient,
    operation: &str,
    parameters: &[(String, String)],
    interval: u64,
    timeout: u64,
    cli: &Cli,
) -> Result<(), Error> {
    if cli.file.is_some() || !cli.fields.is_empty() || cli.if_match.is_some() || cli.idempotency_key.is_some()
    {
        return Err(Error::Input(
            "Waiting does not accept request bodies or mutation headers".into(),
        ));
    }
    let (parameter, object_type, status_field, status_schema) = match operation {
        "getLinaExecution" => ("executionId", "LINA_EXECUTION", "status", "LinaExecutionStatus"),
        "getImport" => ("importId", "IMPORT", "status", "ImportStatus"),
        "getBackupRestore" => (
            "backupRestoreId",
            "BACKUP_RESTORE",
            "status",
            "BackupRestoreStatus",
        ),
        "getProjectMove" => ("projectMoveId", "PROJECT_MOVE", "status", "ProjectMoveStatus"),
        "getBackup" => ("backupId", "BACKUP", "status", "BackupStatus"),
        "getExport" => ("exportId", "EXPORT", "status", "ExportStatus"),
        "getWebhookDelivery" => (
            "webhookDeliveryId",
            "WEBHOOK_DELIVERY",
            "status",
            "WebhookDeliveryStatus",
        ),
        "getBranch" => (
            "branchId",
            "BRANCH",
            "initialization_status",
            "BranchInitializationStatus",
        ),
        _ => return Err(Error::Input("Unsupported polling operation".into())),
    };
    let (method, path) = contract::resolve(operation, parameters)?;
    if method != "GET" {
        return Err(Error::Protocol("Polling operation must remain a GET".into()));
    }
    let id = parameters
        .iter()
        .find(|(name, _)| name == parameter)
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| Error::Input("Missing polling resource identity".into()))?;
    let specification = contract::specification();
    let allowed = specification["components"]["schemas"][status_schema]["enum"]
        .as_array()
        .ok_or_else(|| Error::Protocol("Bundled polling status contract is missing".into()))?;
    let mut last = None;
    let outcome = tokio::time::timeout(Duration::from_secs(timeout), async {
        loop {
            last = Some(client.request(Method::GET, &path, &[], None, &RequestOptions::default()).await?);
            let body = last.as_ref().and_then(|response| response.body.as_ref())
                .ok_or_else(|| Error::Protocol("Expected a polling resource response body".into()))?;
            if body["object_type"].as_str() != Some(object_type) || body["id"].as_str() != Some(id) {
                return Err(Error::Protocol("Polling response does not match the requested resource".into()));
            }
            let status = body[status_field].as_str()
                .filter(|status| allowed.iter().any(|value| value.as_str() == Some(status)))
                .ok_or_else(|| Error::Protocol("Polling response has a missing or unrecognized status".into()))?;
            match status {
                "SUCCEEDED" => return Ok(()),
                "QUEUED" | "PROCESSING" => tokio::time::sleep(Duration::from_secs(interval)).await,
                "PARTIALLY_SUCCEEDED" | "FAILED" | "CANCELLED" | "SKIPPED" => return Err(Error::Protocol(format!("Operation finished with {status}; inspect the resource and its entries before retrying"))),
                _ => return Err(Error::Protocol("Unsupported polling status; inspect the resource before retrying".into())),
            }
        }
    }).await;
    if let Some(response) = last {
        output::print_response(response, cli.json, cli.include_headers)?;
    }
    outcome.unwrap_or_else(|_| {
        Err(Error::Protocol(
            "Waiting timed out; the operation may still be running. No mutation was retried".into(),
        ))
    })
}
