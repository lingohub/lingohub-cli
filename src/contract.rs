// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::error::Error;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::OnceLock};

pub fn specification() -> &'static Value {
    static SPECIFICATION: OnceLock<Value> = OnceLock::new();
    SPECIFICATION.get_or_init(|| {
        serde_json::from_str(include_str!("../contract/v2.json")).expect("bundled OpenAPI is valid")
    })
}

pub fn operations() -> Value {
    let spec = specification();
    let mut operations = Vec::new();
    for (path, item) in spec["paths"].as_object().expect("OpenAPI paths") {
        for (method, op) in item.as_object().expect("OpenAPI path item") {
            if let Some(id) = op["operationId"].as_str() {
                operations.push(json!({"operation_id":id,"method":method.to_uppercase(),"path":path,"summary":op["summary"],"request_content_types":op.pointer("/requestBody/content").and_then(Value::as_object).map(|x| x.keys().collect::<Vec<_>>()).unwrap_or_default()}));
            }
        }
    }
    Value::Array(operations)
}

/// Follow `$ref` chains to the shared header, response, parameter or schema they name.
/// Returns `None` for external, unresolved or cyclic references.
pub fn dereference<'a>(spec: &'a Value, mut value: &'a Value) -> Option<&'a Value> {
    let mut seen = HashSet::new();
    while let Some(reference) = value.get("$ref").and_then(Value::as_str) {
        if !seen.insert(reference) {
            return None;
        }
        value = reference
            .strip_prefix('#')
            .filter(|pointer| pointer.starts_with('/'))
            .and_then(|pointer| spec.pointer(pointer))?;
    }
    Some(value)
}

/// Batches are atomic: a success lists one changed resource per submitted item, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BulkResultKind {
    KeyCreation,
    SegmentMutation,
    Unknown,
}

impl BulkResultKind {
    /// The `object_type` every listed resource must carry; `None` rejects unadapted schemas.
    pub fn object_type(self) -> Option<&'static str> {
        match self {
            Self::KeyCreation => Some("KEY"),
            Self::SegmentMutation => Some("SEGMENT"),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExpectedResponse {
    Bulk(BulkResultKind),
    LinaTranslation,
}

/// The batch operations and the list schema each returns on success. Batch semantics belong to
/// these operations, not to the shape of a response: an unpaged list from any other operation is
/// an ordinary read.
const BULK_OPERATIONS: [(&str, &str, BulkResultKind); 3] = [
    (
        "createKeysBulk",
        "#/components/schemas/KeyList",
        BulkResultKind::KeyCreation,
    ),
    (
        "updateSegmentsBulk",
        "#/components/schemas/SegmentList",
        BulkResultKind::SegmentMutation,
    ),
    (
        "transitionSegmentsBulk",
        "#/components/schemas/SegmentList",
        BulkResultKind::SegmentMutation,
    ),
];

/// Classify an operation's success schema. A batch operation whose declared schema is not the
/// adapted one is `Unknown`, which rejects its results until an adapter exists.
fn expected_schema(spec: &Value, operation: &Value, schema: &Value) -> Option<ExpectedResponse> {
    let reference = schema.get("$ref").and_then(Value::as_str);
    if let Some((_, adapted, kind)) = BULK_OPERATIONS
        .iter()
        .find(|(id, _, _)| operation["operationId"].as_str() == Some(id))
    {
        return Some(ExpectedResponse::Bulk(if reference == Some(adapted) {
            *kind
        } else {
            BulkResultKind::Unknown
        }));
    }
    let discriminator = dereference(spec, schema)?
        .pointer("/properties/object_type/enum")
        .and_then(Value::as_array)
        .filter(|values| values.len() == 1)
        .and_then(|values| values[0].as_str());
    (discriminator == Some("LINA_TRANSLATE_RESULT")).then_some(ExpectedResponse::LinaTranslation)
}

/// Resolve operation-specific result validation for both named and raw requests.
pub fn expected_response(method: &str, path: &str) -> Option<ExpectedResponse> {
    let spec = specification();
    let segments: Vec<_> = path.split('/').collect();
    spec["paths"]
        .as_object()
        .expect("OpenAPI paths")
        .iter()
        .find_map(|(template, item)| {
            let parts: Vec<_> = template.split('/').collect();
            if parts.len() != segments.len()
                || !parts.iter().zip(&segments).all(|(part, value)| {
                    part == value || (part.starts_with('{') && part.ends_with('}') && !value.is_empty())
                })
            {
                return None;
            }
            let operation = item.get(method.to_ascii_lowercase())?;
            let responses = operation["responses"].as_object()?;
            responses.iter().find_map(|(status, response)| {
                if !status.starts_with('2') {
                    return None;
                }
                let response = dereference(spec, response)?;
                expected_schema(
                    spec,
                    operation,
                    &response["content"]["application/json"]["schema"],
                )
            })
        })
}

/// Resolve an operation without permitting path parameters to change its resource scope.
pub fn resolve(id: &str, params: &[(String, String)]) -> Result<(String, String), Error> {
    resolve_content(id, params, false, false)
}

/// Named calls must honor the operation's declared request-body support before reading user input.
pub fn resolve_call(
    id: &str,
    params: &[(String, String)],
    has_body_input: bool,
) -> Result<(String, String), Error> {
    resolve_content(id, params, false, has_body_input)
}

pub fn resolve_upload(id: &str, params: &[(String, String)]) -> Result<(String, String), Error> {
    resolve_content(id, params, true, false)
}

pub fn upload_schema(id: &str) -> Result<Value, Error> {
    specification()["paths"]
        .as_object()
        .expect("OpenAPI paths")
        .values()
        .flat_map(|item| item.as_object().expect("OpenAPI path item").values())
        .find(|op| op["operationId"].as_str() == Some(id))
        .and_then(|op| {
            op.pointer("/requestBody/content/multipart~1form-data/schema")
                .cloned()
        })
        .ok_or_else(|| Error::Input("This operation does not accept multipart uploads".into()))
}

fn resolve_content(
    id: &str,
    params: &[(String, String)],
    multipart: bool,
    has_body_input: bool,
) -> Result<(String, String), Error> {
    let spec = specification();
    for (path, item) in spec["paths"].as_object().expect("OpenAPI paths") {
        for (method, op) in item.as_object().expect("OpenAPI path item") {
            if op["operationId"].as_str() != Some(id) {
                continue;
            }
            let content = op.pointer("/requestBody/content").and_then(Value::as_object);
            if has_body_input && content.is_none() {
                return Err(Error::Input(format!(
                    "Operation {id} does not accept a request body; omit --file and --field"
                )));
            }
            let accepted = if multipart {
                content.is_some_and(|c| c.contains_key("multipart/form-data"))
            } else {
                content.is_none_or(|c| c.keys().any(|s| s == "application/json" || s.ends_with("+json")))
            };
            if !accepted {
                return Err(Error::Input(if multipart {
                    "This operation does not accept multipart uploads".into()
                } else {
                    "This operation requires multipart input; use api upload OPERATION --upload PATH".into()
                }));
            }
            let mut result = path.clone();
            let mut seen = HashSet::new();
            for (name, value) in params {
                let marker = format!("{{{name}}}");
                if !seen.insert(name) || !result.contains(&marker) {
                    return Err(Error::Input(format!(
                        "Unexpected or duplicate path parameter: {name}"
                    )));
                }
                if value.is_empty()
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                    || matches!(value.as_str(), "." | "..")
                {
                    return Err(Error::Input(format!("Invalid path parameter: {name}")));
                }
                result = result.replace(&marker, value);
            }
            if result.contains('{') {
                return Err(Error::Input(format!("Missing --path parameters for {path}")));
            }
            return Ok((method.to_uppercase(), result));
        }
    }
    Err(Error::Input(format!(
        "Unknown operation: {id}; use api operations"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_batch_operation_has_an_outcome_adapter() {
        let spec = specification();
        let mut batches = Vec::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let Some(id) = operation["operationId"].as_str() else {
                    continue;
                };
                if !id.ends_with("Bulk") && !path.contains("/bulk-") {
                    continue;
                }
                let concrete = path
                    .replace("{resourceId}", "rc_1")
                    .replace("{branchId}", "prb_1");
                match expected_response(&method.to_uppercase(), &concrete) {
                    Some(ExpectedResponse::Bulk(kind)) => assert!(
                        kind != BulkResultKind::Unknown,
                        "The success schema of {id} changed; adapt its outcome validation"
                    ),
                    other => panic!("Add {id} to BULK_OPERATIONS before exposing it: {other:?}"),
                }
                batches.push(id);
            }
        }
        batches.sort_unstable();
        let mut mapped: Vec<_> = BULK_OPERATIONS.iter().map(|(id, _, _)| *id).collect();
        mapped.sort_unstable();
        assert_eq!(mapped, batches);
        assert_eq!(
            Some(ExpectedResponse::Bulk(BulkResultKind::KeyCreation)),
            expected_response("POST", "/v2/resources/rc_1/keys/bulk-create")
        );
        assert_eq!(
            Some(ExpectedResponse::Bulk(BulkResultKind::SegmentMutation)),
            expected_response("POST", "/v2/branches/prb_1/segments/bulk-transition")
        );
    }

    #[test]
    fn list_shape_alone_does_not_make_an_operation_a_batch() {
        let spec = specification();
        let list = json!({"$ref":"#/components/schemas/SegmentList"});
        assert_eq!(
            None,
            expected_schema(spec, &json!({"operationId":"listSomethingUnpaged"}), &list)
        );
        assert_eq!(
            Some(ExpectedResponse::Bulk(BulkResultKind::Unknown)),
            expected_schema(
                spec,
                &json!({"operationId":"createKeysBulk"}),
                &json!({"$ref":"#/components/schemas/SegmentList"})
            )
        );
        assert_eq!(None, expected_response("GET", "/v2/branches/prb_1/segments"));
    }

    #[test]
    fn shared_response_and_header_references_resolve_and_cycles_do_not() {
        let spec = specification();
        let operation = &spec["paths"]["/v2/segments/{segmentId}"]["get"];
        let missing = dereference(spec, &operation["responses"]["404"]).unwrap();
        assert!(missing["content"]["application/problem+json"]["schema"].is_object());
        let etag = dereference(spec, &operation["responses"]["200"]["headers"]["ETag"]).unwrap();
        assert_eq!("string", etag["schema"]["type"]);
        let cyclic = json!({"a":{"$ref":"#/b"},"b":{"$ref":"#/a"}});
        assert!(dereference(&cyclic, &cyclic["a"]).is_none());
        assert!(dereference(spec, &json!({"$ref":"https://example.test/x"})).is_none());
        assert!(dereference(spec, &json!({"$ref":"#/components/responses/Missing"})).is_none());
    }
}
