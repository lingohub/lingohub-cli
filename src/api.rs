// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{error::Error, transport::ApiClient};
use serde::Deserialize;

/// Wire models used only by the interactive dashboard. CLI JSON retains the full response.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceData {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct StackData {
    pub id: String,
    pub name: String,
    pub color: String,
    #[serde(rename = "type")]
    pub stack_type: String,
    pub character_count: Option<i64>,
    pub word_count: Option<i64>,
    pub counts_updated_at: String,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectData {
    pub id: String,
    pub stack_id: String,
    pub name: String,
    pub website: Option<String>,
    pub ai_context: Option<String>,
    #[serde(rename = "type")]
    pub project_type: String,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn fetch_workspace(base: &str, key: &str) -> Result<WorkspaceData, Error> {
    Ok(serde_json::from_value(
        ApiClient::new(base, key)?.get("/v2/workspace").await?,
    )?)
}
pub async fn fetch_stacks(base: &str, key: &str) -> Result<Vec<StackData>, Error> {
    Ok(serde_json::from_value(
        ApiClient::new(base, key)?
            .list("/v2/stacks", &[("limit".into(), "100".into())], true)
            .await?,
    )?)
}
pub async fn fetch_projects(base: &str, key: &str) -> Result<Vec<ProjectData>, Error> {
    Ok(serde_json::from_value(
        ApiClient::new(base, key)?
            .list("/v2/projects", &[("limit".into(), "100".into())], true)
            .await?,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// API V2 drops `last_active_at` and `is_open_source` (lingohub/organization#2755). This CLI release must read
    /// both the payloads of the backend it ships with and those of the backend after the change.
    #[test]
    fn dashboard_models_accept_payloads_with_and_without_retired_fields() {
        let workspace = json!({"id":"space_a","name":"Example","created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-29T00:00:00.123Z"});
        let mut legacy_workspace = workspace.clone();
        legacy_workspace["last_active_at"] = json!("2026-09-29T00:00:00.123Z");
        for payload in [workspace, legacy_workspace] {
            let parsed: WorkspaceData = serde_json::from_value(payload).unwrap();
            assert_eq!(parsed.name, "Example");
        }

        let project = json!({"id":"pr_a","stack_id":"stack_a","name":"App","type":"STANDARD_JSON","website":null,"ai_context":null,"created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-29T00:00:00.123Z"});
        let mut legacy_project = project.clone();
        legacy_project["is_open_source"] = json!(false);
        for payload in [project, legacy_project] {
            let parsed: ProjectData = serde_json::from_value(payload).unwrap();
            assert_eq!(parsed.name, "App");
        }
    }
}
