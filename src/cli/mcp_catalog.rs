// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{contract, error::Error};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

const DISCOVERY: &[&str] = &[
    "getApiKey",
    "getWorkspace",
    "listStacks",
    "listProjects",
    "listBranches",
    "listResources",
    "listLocales",
    "getProjectCapabilities",
];

/// Generate SDK-independent MCP tool descriptors without credentials or network access.
pub fn catalog(all: bool, operations: &[String], tags: &[String]) -> Result<Value, Error> {
    build(contract::specification(), all, operations, tags)
}

fn build(spec: &Value, all: bool, operations: &[String], tags: &[String]) -> Result<Value, Error> {
    let paths = spec["paths"]
        .as_object()
        .ok_or_else(|| invalid("Missing OpenAPI paths"))?;
    let mut inventory = BTreeMap::new();
    let mut known_tags = BTreeSet::new();
    for (path, item) in paths {
        if !path.starts_with("/v2/") {
            return Err(invalid("Non-V2 path in contract"));
        }
        for method in ["get", "post", "put", "patch", "delete", "head", "options"] {
            let Some(op) = item.get(method) else { continue };
            let id = op["operationId"]
                .as_str()
                .ok_or_else(|| invalid("Missing operationId"))?;
            let tag = op["tags"]
                .as_array()
                .filter(|tags| tags.len() == 1)
                .and_then(|tags| tags[0].as_str())
                .ok_or_else(|| invalid("Expected one operation tag"))?;
            if inventory
                .insert(id, (path.as_str(), method, item, op, tag))
                .is_some()
            {
                return Err(invalid("Duplicate operationId"));
            }
            known_tags.insert(tag);
        }
    }
    for id in operations {
        if !inventory.contains_key(id.as_str()) {
            return Err(invalid(&format!("Unknown operation: {id}")));
        }
    }
    for tag in tags {
        if !known_tags.contains(tag.as_str()) {
            return Err(invalid(&format!("Unknown tag: {tag}")));
        }
    }
    let default = !all && operations.is_empty() && tags.is_empty();
    if default && DISCOVERY.iter().any(|id| !inventory.contains_key(id)) {
        return Err(invalid("Discovery preset no longer matches the bundled contract"));
    }
    let mut tools = Vec::new();
    let mut unsupported = Vec::new();
    let mut names = BTreeSet::new();
    for (id, (path, method, item, op, tag)) in &inventory {
        if !(all
            || operations.iter().any(|v| v == id)
            || tags.iter().any(|v| v == tag)
            || (default && DISCOVERY.contains(id)))
        {
            continue;
        }
        let namespace = tag
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        let name = format!("{namespace}.{id}");
        if namespace.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
            || !names.insert(name.clone())
        {
            return Err(invalid("Ambiguous or invalid MCP tool name"));
        }
        let body = op
            .get("requestBody")
            .map(|value| object(spec, value))
            .transpose()?;
        let content = body
            .as_ref()
            .and_then(|b| b.get("content"))
            .and_then(Value::as_object);
        let json_body = content.and_then(|c| {
            c.get("application/json")
                .or_else(|| c.get("application/merge-patch+json"))
        });
        if body.is_some() && json_body.is_none() {
            unsupported.push(json!({"operation_id":id,"reason":"Requires a multipart or non-JSON adapter; use the existing CLI upload/download workflow."}));
            continue;
        }
        let mut definitions = Map::new();
        let mut properties = Map::new();
        let mut required = Vec::new();
        let mut parameters = BTreeMap::new();
        for source in [item.get("parameters"), op.get("parameters")] {
            for parameter in source.and_then(Value::as_array).into_iter().flatten() {
                let parameter = object(spec, parameter)?;
                let location = parameter["in"]
                    .as_str()
                    .ok_or_else(|| invalid("Missing parameter location"))?;
                if !matches!(location, "path" | "query" | "header") {
                    return Err(invalid("Unsupported MCP parameter location"));
                }
                let name = parameter["name"]
                    .as_str()
                    .ok_or_else(|| invalid("Missing parameter name"))?;
                parameters.insert((location.to_owned(), name.to_owned()), parameter);
            }
        }
        for (location, group) in [("path", "path"), ("query", "query"), ("header", "headers")] {
            let mut fields = Map::new();
            let mut field_required = Vec::new();
            for ((parameter_location, parameter_name), parameter) in &parameters {
                if parameter_location != location {
                    continue;
                }
                if location == "header" && !matches!(parameter_name.as_str(), "If-Match" | "Idempotency-Key")
                {
                    return Err(invalid(&format!(
                        "Unsupported MCP header parameter: {parameter_name}"
                    )));
                }
                let schema = parameter
                    .get("schema")
                    .or_else(|| parameter.pointer("/content/application~1json/schema"))
                    .ok_or_else(|| invalid("Parameter has no supported input schema"))?;
                let mut field = schema_refs(spec, schema, &mut definitions)?;
                if let Some(fields) = field.as_object_mut()
                    && let Some(description) = parameter.get("description")
                {
                    fields.entry("description").or_insert_with(|| description.clone());
                }
                fields.insert(parameter_name.clone(), field);
                if parameter["required"].as_bool().unwrap_or(false) || location == "path" {
                    field_required.push(Value::String(parameter_name.clone()));
                }
            }
            if !fields.is_empty() {
                if !field_required.is_empty() {
                    required.push(Value::String(group.into()));
                }
                properties.insert(group.into(), json!({"type":"object","properties":fields,"required":field_required,"additionalProperties":false}));
            }
        }
        if let Some(body_content) = json_body {
            let schema = body_content
                .get("schema")
                .ok_or_else(|| invalid("JSON body has no schema"))?;
            properties.insert("body".into(), schema_refs(spec, schema, &mut definitions)?);
            if body
                .as_ref()
                .is_some_and(|b| b["required"].as_bool().unwrap_or(false))
            {
                required.push(Value::String("body".into()));
            }
        }
        let mut schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":properties, "required":required,"additionalProperties":false});
        if !definitions.is_empty() {
            schema["$defs"] = Value::Object(definitions);
        }
        let read_only = matches!(*method, "get" | "head" | "options");
        let mut tool = json!({"name":name,"inputSchema":schema,
            "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"idempotentHint":read_only,"openWorldHint":true},
            "_meta":{"lingohub/operationId":id,"lingohub/tag":tag,"lingohub/method":method.to_uppercase(),"lingohub/path":path}});
        for (key, source) in [("title", "summary"), ("description", "description")] {
            if let Some(value) = op.get(source).and_then(Value::as_str) {
                tool[key] = Value::String(value.into());
            }
        }
        tools.push(tool);
    }
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(
        json!({"tools":tools,"unsupported":unsupported,"available_operation_count":inventory.len(),"selection":if default {"discovery"} else if all {"all"} else {"explicit"}}),
    )
}

fn object<'a>(spec: &'a Value, mut value: &'a Value) -> Result<&'a Value, Error> {
    let mut seen = BTreeSet::new();
    while let Some(reference) = value.get("$ref").and_then(Value::as_str) {
        if !seen.insert(reference) {
            return Err(invalid("Cyclic operation reference"));
        }
        value = reference
            .strip_prefix('#')
            .filter(|p| p.starts_with('/'))
            .and_then(|p| spec.pointer(p))
            .ok_or_else(|| invalid("Unresolved or external operation reference"))?;
    }
    value
        .as_object()
        .ok_or_else(|| invalid("Expected an OpenAPI object"))?;
    Ok(value)
}

/// Keep recursive schemas finite with tool-local definitions, without dropping nullability/facets.
fn schema_refs(spec: &Value, value: &Value, definitions: &mut Map<String, Value>) -> Result<Value, Error> {
    match value {
        Value::Object(properties) => {
            let mut result = Map::new();
            for (key, child) in properties {
                if key == "$ref" {
                    let reference = child
                        .as_str()
                        .ok_or_else(|| invalid("Invalid schema reference"))?;
                    let name = reference
                        .strip_prefix("#/components/schemas/")
                        .filter(|name| !name.is_empty() && !name.contains('/'))
                        .ok_or_else(|| invalid("Unsupported or external schema reference"))?;
                    let definition_key = name.replace("~1", "/").replace("~0", "~");
                    if !definitions.contains_key(&definition_key) {
                        let schema = spec
                            .pointer(&reference[1..])
                            .ok_or_else(|| invalid("Unresolved schema reference"))?;
                        definitions.insert(definition_key.clone(), Value::Null);
                        let rewritten = schema_refs(spec, schema, definitions)?;
                        definitions.insert(definition_key, rewritten);
                    }
                    result.insert(key.clone(), Value::String(format!("#/$defs/{name}")));
                } else if key == "example" {
                    // Example contents are instance data; do not rewrite their keys.
                    if !properties.contains_key("examples") {
                        result.insert("examples".into(), Value::Array(vec![child.clone()]));
                    }
                } else if key == "nullable" {
                    // OpenAPI 3.0's keyword has no meaning in JSON Schema 2020-12.
                } else {
                    let converted = match key.as_str() {
                        "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                            let schemas = child.as_object().ok_or_else(|| invalid("Expected schema map"))?;
                            let mut converted = Map::new();
                            for (name, schema) in schemas {
                                converted.insert(name.clone(), schema_refs(spec, schema, definitions)?);
                            }
                            Value::Object(converted)
                        }
                        "items"
                        | "additionalProperties"
                        | "unevaluatedProperties"
                        | "unevaluatedItems"
                        | "propertyNames"
                        | "contains"
                        | "if"
                        | "then"
                        | "else"
                        | "not"
                        | "allOf"
                        | "anyOf"
                        | "oneOf"
                        | "prefixItems" => schema_refs(spec, child, definitions)?,
                        // Examples, defaults and enum values are instance data, not schemas.
                        _ => child.clone(),
                    };
                    result.insert(key.clone(), converted);
                }
            }
            for (bound, exclusive) in [("minimum", "exclusiveMinimum"), ("maximum", "exclusiveMaximum")] {
                match properties.get(exclusive).and_then(Value::as_bool) {
                    Some(true) => {
                        let value = result
                            .remove(bound)
                            .filter(Value::is_number)
                            .ok_or_else(|| invalid("Exclusive OpenAPI bound requires a numeric limit"))?;
                        result.insert(exclusive.into(), value);
                    }
                    Some(false) => {
                        result.remove(exclusive);
                    }
                    None => {}
                }
            }
            if properties.get("nullable").and_then(Value::as_bool) == Some(true) {
                Ok(json!({"anyOf":[Value::Object(result), {"type":"null"}]}))
            } else {
                Ok(Value::Object(result))
            }
        }
        Value::Array(items) => items
            .iter()
            .map(|v| schema_refs(spec, v, definitions))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        value => Ok(value.clone()),
    }
}

fn invalid(message: &str) -> Error {
    Error::Input(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_is_bounded_read_only_and_deterministic() {
        let first = catalog(false, &[], &[]).unwrap();
        assert_eq!(first, catalog(false, &[], &[]).unwrap());
        let tools = first["tools"].as_array().unwrap();
        assert_eq!(DISCOVERY.len(), tools.len());
        assert!(
            tools
                .iter()
                .all(|tool| tool["annotations"]["readOnlyHint"] == true)
        );
        assert_eq!(315, first["available_operation_count"]);
    }

    #[test]
    fn complete_inventory_accounts_for_every_operation_and_resolves_input_references() {
        let result = catalog(true, &[], &[]).unwrap();
        assert_eq!(
            315,
            result["tools"].as_array().unwrap().len() + result["unsupported"].as_array().unwrap().len()
        );
        for tool in result["tools"].as_array().unwrap() {
            assert_resolved(&tool["inputSchema"], &tool["inputSchema"]);
            // Descriptions may explain authorization; credentials must never be tool inputs.
            if let Some(headers) = tool["inputSchema"]["properties"]["headers"]["properties"].as_object() {
                assert!(
                    headers
                        .keys()
                        .all(|name| !name.eq_ignore_ascii_case("authorization"))
                );
            }
        }
    }

    fn assert_resolved(root: &Value, value: &Value) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "$ref" {
                        assert!(
                            root.pointer(child.as_str().unwrap().strip_prefix('#').unwrap())
                                .is_some()
                        );
                    } else {
                        assert_resolved(root, child);
                    }
                }
            }
            Value::Array(values) => {
                for child in values {
                    assert_resolved(root, child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn conditional_replacement_preserves_required_body_public_enum_and_header() {
        let result = catalog(false, &["setProjectKeySettings".into()], &[]).unwrap();
        let tool = &result["tools"][0];
        assert_eq!("project_settings.setProjectKeySettings", tool["name"]);
        assert_eq!(false, tool["annotations"]["readOnlyHint"]);
        assert_eq!(json!(["path", "body"]), tool["inputSchema"]["required"]);
        assert_eq!(
            json!(["projectId"]),
            tool["inputSchema"]["properties"]["path"]["required"]
        );
        assert!(
            tool["inputSchema"]["properties"]["headers"]["properties"]
                .get("If-Match")
                .is_some()
        );
        assert_eq!(
            json!(["normalization_strategy"]),
            tool["inputSchema"]["$defs"]["SetProjectKeySettings"]["required"]
        );
    }

    #[test]
    fn unknown_selections_fail_and_multipart_remains_explicit() {
        assert!(catalog(false, &["missing".into()], &[]).is_err());
        assert!(catalog(false, &[], &["missing".into()]).is_err());
        let result = catalog(true, &[], &[]).unwrap();
        assert!(!result["unsupported"].as_array().unwrap().is_empty());
        assert!(
            result["unsupported"]
                .as_array()
                .unwrap()
                .iter()
                .all(|x| x["reason"].as_str().unwrap().contains("adapter"))
        );
    }

    #[test]
    fn recursive_schema_references_keep_facets_and_reject_external_or_missing_targets() {
        let spec = json!({"components":{"schemas":{"Node":{"type":["object","null"],"properties":{"next":{"$ref":"#/components/schemas/Node"}}}}}});
        let mut defs = Map::new();
        let schema = schema_refs(&spec, &json!({"$ref":"#/components/schemas/Node"}), &mut defs).unwrap();
        assert_eq!(json!({"$ref":"#/$defs/Node"}), schema);
        assert_eq!(json!(["object", "null"]), defs["Node"]["type"]);
        assert_eq!("#/$defs/Node", defs["Node"]["properties"]["next"]["$ref"]);
        for reference in ["https://example.test/schema", "#/components/schemas/Missing"] {
            assert!(schema_refs(&spec, &json!({"$ref":reference}), &mut Map::new()).is_err());
        }
    }

    #[test]
    fn nullable_inputs_allow_null_without_weakening_non_null_constraints() {
        let spec = json!({"components":{"schemas":{"Choice":{"type":"string","enum":["A","B"]}}}});
        for original in [
            json!({"nullable":true,"type":"string","minLength":1}),
            json!({"nullable":true,"oneOf":[{"$ref":"#/components/schemas/Choice"}]}),
            json!({"nullable":true,"$ref":"#/components/schemas/Choice"}),
        ] {
            let converted = schema_refs(&spec, &original, &mut Map::new()).unwrap();
            assert_eq!(json!({"type":"null"}), converted["anyOf"][1]);
            assert!(converted["anyOf"][0].get("nullable").is_none());
            assert!(converted.get("nullable").is_none());
        }
        let original = json!({"type":"object","properties":{"nullable":{"type":"boolean"}},
            "example":{"nullable":true,"$ref":"literal"},"default":{"nullable":true}});
        let converted = schema_refs(&spec, &original, &mut Map::new()).unwrap();
        assert_eq!(converted["examples"], json!([original["example"].clone()]));
        assert!(converted.get("example").is_none());
        assert_eq!(converted["properties"], original["properties"]);
        assert_eq!(converted["default"], original["default"]);
        let result = catalog(false, &["setProjectFormatSettings".into()], &[]).unwrap();
        let configuration = &result["tools"][0]["inputSchema"]["$defs"]["SetProjectFormatSettings"]["properties"]
            ["configuration"];
        assert_eq!(json!({"type":"null"}), configuration["anyOf"][1]);
    }

    #[test]
    fn converts_openapi_exclusive_bounds_and_preserves_numeric_bounds_and_instance_data() {
        for (source, expected) in [
            (
                json!({"minimum":0,"exclusiveMinimum":true,"maximum":1,"exclusiveMaximum":false}),
                json!({"exclusiveMinimum":0,"maximum":1}),
            ),
            (
                json!({"minimum":0,"exclusiveMinimum":false,"maximum":1,"exclusiveMaximum":true}),
                json!({"minimum":0,"exclusiveMaximum":1}),
            ),
            (
                json!({"exclusiveMinimum":0.25,"exclusiveMaximum":0.75}),
                json!({"exclusiveMinimum":0.25,"exclusiveMaximum":0.75}),
            ),
            (
                json!({"example":{"exclusiveMinimum":true},"default":{"exclusiveMaximum":false}}),
                json!({"examples":[{"exclusiveMinimum":true}],"default":{"exclusiveMaximum":false}}),
            ),
        ] {
            assert_eq!(
                schema_refs(&json!({}), &source, &mut Map::new()).unwrap(),
                expected
            );
        }
        assert!(schema_refs(&json!({}), &json!({"exclusiveMinimum":true}), &mut Map::new()).is_err());
    }

    #[test]
    fn unsupported_contract_shapes_fail_instead_of_silently_losing_parameters() {
        let original = json!({"paths":{"/v2/example":{"get":{"operationId":"getExample","tags":["Examples"],"parameters":[{"in":"cookie","name":"secret","schema":{"type":"string"}}]}}}});
        assert!(build(&original, true, &[], &[]).is_err());
        let mut bad_header = original.clone();
        bad_header["paths"]["/v2/example"]["get"]["parameters"][0]["in"] = json!("header");
        assert!(build(&bad_header, true, &[], &[]).is_err());
        let mut bad_name = original;
        bad_name["paths"]["/v2/example"]["get"]["parameters"] = json!([]);
        bad_name["paths"]["/v2/example"]["get"]["operationId"] = json!("get/Example");
        assert!(build(&bad_name, true, &[], &[]).is_err());
    }
}
