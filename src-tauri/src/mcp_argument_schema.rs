//! Argument VALUE checks from the tool catalog: required, type, enum, and one
//! level of shape inside arrays/objects that the catalog describes.
//!
//! `mcp_argument_validation` closes the set of top-level argument NAMES. This
//! module answers the next question for the names that survived: is the value
//! the kind of thing the handler is written to read? It exists because a
//! handler reached with the wrong kind of value does one of two things: errors
//! late, or reads a default and reports success. The second is the Song P0
//! class (`verse` missing → `unwrap_or_default` → a blank verse rotated the
//! Song), and it recurs one level down in nested bags (a valuation entry whose
//! `block_id` is misspelled becomes an entry with an empty block id).
//!
//! What this is NOT: a JSON Schema engine. It reads exactly the keywords the
//! catalog is written with — `required`, `type`, `enum`, `properties`, `items`,
//! `minimum`, `maximum` — and ignores everything else. Free-form bags (a
//! property with `type: object` and no `properties`, or `type: array` and no
//! `items` shape) stay free. Explicit `null` is accepted for any property that
//! is not required, because the organism sends null placeholders on purpose.
//! A catalog `required` list is satisfied by any spelling the catalog also
//! lists as a property (camel or snake twin), so the check never demands a
//! spelling the handler would not itself accept.
//!
//! Wiring is the dispatcher's: names are checked before home-graph injection;
//! THIS check runs after it (a connection with a home graph legitimately omits
//! graphId, and the catalog marks graphId required on many tools) and before
//! any handler: `validate_argument_values(name, &arguments_with_home)?`. Errors name the tool, the
//! path, what was expected, and state that no operation was performed.
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::OnceLock};

fn schemas() -> &'static BTreeMap<String, Value> {
    static SCHEMAS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        let catalog: Value = serde_json::from_str(include_str!("mcp_tool_catalog.json"))
            .expect("valid embedded catalog");
        catalog["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| {
                (
                    tool["name"].as_str().expect("tool name").to_owned(),
                    tool["inputSchema"].clone(),
                )
            })
            .collect()
    })
}

pub(super) fn validate_argument_values(name: &str, arguments: &Value) -> Result<(), String> {
    let schema = schemas()
        .get(name)
        .ok_or_else(|| format!("unknown tool {name}"))?;
    validate_against(name, schema, arguments, "arguments")
        .map_err(|problem| format!("{name}: {problem}. No operation was performed."))
}

pub(super) fn home_graph_key(name: &str) -> Option<&'static str> {
    // Creation and changing the default are explicit acts, not uses of it.
    if matches!(name, "create_graph" | "set_home_graph") {
        return None;
    }
    let properties = schemas().get(name)?.get("properties")?;
    if properties.get("graphId").is_some() {
        Some("graphId")
    } else if properties.get("graph_id").is_some() {
        Some("graph_id")
    } else {
        None
    }
}

/// camelCase ⇄ snake_case, so a `required` entry is satisfied by either twin
/// when the catalog lists both.
fn spelling_twins(key: &str) -> Vec<String> {
    let mut twins = vec![key.to_string()];
    if key.contains('_') {
        let mut camel = String::new();
        let mut upper = false;
        for ch in key.chars() {
            if ch == '_' {
                upper = true;
            } else if upper {
                camel.extend(ch.to_uppercase());
                upper = false;
            } else {
                camel.push(ch);
            }
        }
        twins.push(camel);
    } else if key.chars().any(|ch| ch.is_ascii_uppercase()) {
        let mut snake = String::new();
        for ch in key.chars() {
            if ch.is_ascii_uppercase() {
                snake.push('_');
                snake.extend(ch.to_lowercase());
            } else {
                snake.push(ch);
            }
        }
        twins.push(snake);
    }
    twins
}

fn validate_against(tool: &str, schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(object) = value.as_object() else {
        return Err(format!("{path} must be an object"));
    };
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            let mut alternatives = spelling_twins(key);
            alternatives.extend(
                properties
                    .iter()
                    .filter(|(_, prop)| {
                        prop.get("x-garden-alias-for").and_then(Value::as_str) == Some(key)
                    })
                    .map(|(alias, _)| alias.clone()),
            );
            let present = alternatives
                .iter()
                .filter(|twin| properties.contains_key(*twin) || *twin == key)
                .any(|twin| object.get(twin).is_some_and(|v| !v.is_null()));
            if !present {
                return Err(format!("{path}.{key} is required"));
            }
        }
    }

    for (key, value) in object {
        let Some(property) = properties.get(key) else {
            // Unknown names are the name validator's business; a free-form
            // bag reaching here (no `properties`) is left alone.
            continue;
        };
        let canonical = property.get("x-garden-alias-for").and_then(Value::as_str);
        if canonical
            .and_then(|key| object.get(key))
            .is_some_and(|other| other != value)
        {
            return Err(format!(
                "{path}.{key} conflicts with {}",
                canonical.unwrap()
            ));
        }
        if value.is_null() {
            continue; // explicit null placeholder: accepted for optional properties
        }
        validate_value(tool, property, value, &format!("{path}.{key}"))?;
    }
    Ok(())
}

fn validate_value(tool: &str, property: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Some(allowed) = property.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let list = allowed
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("{path} must be one of: {list}"));
        }
    }
    let Some(kind) = property.get("type") else {
        return Ok(());
    };
    // `type` may be a string or a list of strings.
    let kinds: Vec<&str> = match kind {
        Value::String(single) => vec![single.as_str()],
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if kinds.is_empty() {
        return Ok(());
    }
    let matched = kinds.iter().any(|kind| matches_type(kind, value));
    if !matched {
        return Err(format!(
            "{path} must be {} (got {})",
            kinds.join(" or "),
            json_kind(value)
        ));
    }
    if let Some(min) = property.get("minimum").and_then(Value::as_f64) {
        if let Some(number) = value.as_f64() {
            if number < min {
                return Err(format!("{path} must be >= {min}"));
            }
        }
    }
    if let Some(max) = property.get("maximum").and_then(Value::as_f64) {
        if let Some(number) = value.as_f64() {
            if number > max {
                return Err(format!("{path} must be <= {max}"));
            }
        }
    }
    // One level of described shape: a record is described when the catalog
    // gives it `properties` or `required`. Bags with neither stay free. A
    // record that says `additionalProperties: false` is closed: unknown keys
    // inside it are rejected, which is where a misspelled `attrs` is caught.
    let described = property.get("properties").is_some() || property.get("required").is_some();
    if value.is_object() && described {
        validate_against(tool, property, value, path)?;
        if property.get("additionalProperties") == Some(&Value::Bool(false)) {
            let allowed = property
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let object = value.as_object().expect("checked is_object");
            let unknown = object
                .keys()
                .filter(|key| !allowed.contains(key))
                .cloned()
                .collect::<Vec<_>>();
            if !unknown.is_empty() {
                return Err(format!(
                    "{path} has unknown key(s): {}. Allowed: {}",
                    unknown.join(", "),
                    allowed.join(", ")
                ));
            }
        }
    }
    if let (Some(items), Some(entries)) = (property.get("items"), value.as_array()) {
        for (index, entry) in entries.iter().enumerate() {
            let entry_path = format!("{path}[{index}]");
            if entry.is_null() {
                return Err(format!("{entry_path} must not be null"));
            }
            validate_value(tool, items, entry, &entry_path)?;
        }
    }
    Ok(())
}

fn matches_type(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        // Handlers read integers with as_u64/as_i64, which is None for 8.0,
        // so a whole float would pass here and silently default there. Only a
        // JSON integer representation counts.
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => true, // an unknown keyword in the catalog never blocks a call
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[allow(dead_code)]
fn describe(map: &Map<String, Value>) -> Vec<&String> {
    map.keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sing_requires_a_string_verse_and_a_known_mode() {
        // The Song P0 shape: `content` instead of `verse`. The name check
        // rejects the unknown key; this check rejects the missing required one
        // even when no unknown key is present.
        let err = validate_argument_values("sing", &json!({"graph_id": "g"})).unwrap_err();
        assert!(err.contains("verse is required"), "{err}");
        assert!(err.ends_with("No operation was performed."));
        let err = validate_argument_values("sing", &json!({"verse": 7})).unwrap_err();
        assert!(err.contains("verse must be string"), "{err}");
        let err =
            validate_argument_values("sing", &json!({"verse": "x", "mode": "loud"})).unwrap_err();
        assert!(
            err.contains("mode must be one of: verse, counterpoint, coda"),
            "{err}"
        );
        validate_argument_values(
            "sing",
            &json!({"verse": "x", "mode": "counterpoint", "verse_index": -1}),
        )
        .unwrap();
    }

    #[test]
    fn required_is_satisfied_by_either_spelling_twin() {
        // insert_blocks requires graphId/documentId; the snake twins are also
        // catalog properties, and the handler accepts them.
        validate_argument_values(
            "insert_blocks",
            &json!({"graph_id": "g", "document_id": "d", "content": "hi"}),
        )
        .unwrap();
        let err =
            validate_argument_values("insert_blocks", &json!({"graphId": "g", "content": "hi"}))
                .unwrap_err();
        assert!(err.contains("documentId is required"), "{err}");
    }

    #[test]
    fn integers_must_be_json_integers() {
        validate_argument_values("recall", &json!({"limit": 8})).unwrap();
        // 8.0 is a float on the wire; mcp_arg_u64 would read it as absent.
        let err = validate_argument_values("recall", &json!({"limit": 8.0})).unwrap_err();
        assert!(err.contains("limit must be integer (got number)"), "{err}");
        let err = validate_argument_values("recall", &json!({"limit": "eight"})).unwrap_err();
        assert!(err.contains("limit must be integer (got string)"), "{err}");
        let err = validate_argument_values("recall", &json!({"limit": 8.5})).unwrap_err();
        assert!(err.contains("limit must be integer (got number)"), "{err}");
    }

    #[test]
    fn explicit_null_is_accepted_for_optional_properties_only() {
        // The organism sends null placeholders on create_document.
        validate_argument_values(
            "create_document",
            &json!({"graphId": "g", "title": "T", "documentKind": null}),
        )
        .unwrap();
        let err = validate_argument_values("sing", &json!({"verse": null})).unwrap_err();
        assert!(err.contains("verse is required"), "{err}");
    }

    #[test]
    fn enum_and_bounds_are_read_from_the_catalog() {
        let err = validate_argument_values(
            "insert_blocks",
            &json!({"graphId": "g", "documentId": "d", "format": "rtf"}),
        )
        .unwrap_err();
        assert!(
            err.contains("format must be one of: auto, plain, markdown, html, xml"),
            "{err}"
        );
    }

    #[test]
    fn free_form_bags_stay_free_but_described_items_are_checked() {
        // `attrs` on update_blocks is a bag with no described properties: any
        // key inside is the user's business.
        validate_argument_values(
            "update_blocks",
            &json!({"graphId": "g", "documentId": "d", "blockId": "b", "attrs": {"anything": {"nested": true}}}),
        )
        .unwrap();
        // A described array item shape is checked one level down: the
        // update_blocks edit record requires blockId and attrs (the catalog
        // says so even without `properties`), so a misspelled `attrs` is an
        // error, not an update that "succeeds" with no attributes.
        let err = validate_argument_values(
            "update_blocks",
            &json!({"graphId": "g", "documentId": "d", "edits": [{"blockId": "b", "atrrs": {"checked": true}}]}),
        )
        .unwrap_err();
        assert!(err.contains("edits[0].attrs is required"), "{err}");
        validate_argument_values(
            "update_blocks",
            &json!({"graphId": "g", "documentId": "d", "edits": [{"blockId": "b", "attrs": {"checked": true}}]}),
        )
        .unwrap();
        let err = validate_argument_values("value", &json!({"graphId": "g", "valuations": [null]}))
            .unwrap_err();
        assert!(err.contains("valuations[0] must not be null"), "{err}");
    }

    #[test]
    fn non_object_arguments_and_unknown_tools_are_named() {
        let err = validate_argument_values("recall", &json!("nope")).unwrap_err();
        assert!(err.contains("arguments must be an object"), "{err}");
        assert!(validate_argument_values("no_such_tool", &json!({})).is_err());
    }

    #[test]
    fn create_wires_schema_admits_artifact_targets_at_top_level_and_in_batch() {
        validate_argument_values(
            "create_wires",
            &json!({ "sourceDocumentId": "doc", "targetArtifactId": "artifact-pdf" }),
        )
        .expect("camelCase artifact target");
        validate_argument_values(
            "create_wires",
            &json!({
                "wires": [{ "source_document_id": "doc", "target_artifact_id": "artifact-pdf" }]
            }),
        )
        .expect("snake_case batch artifact target");

        let schema = schemas()["create_wires"].clone();
        for key in [
            "target_document_id",
            "targetDocumentId",
            "target_artifact_id",
            "targetArtifactId",
        ] {
            assert!(
                schema.pointer(&format!("/properties/{key}")).is_some(),
                "top-level {key}"
            );
            assert!(
                schema
                    .pointer(&format!("/properties/wires/items/properties/{key}"))
                    .is_some(),
                "batch {key}"
            );
        }
    }

    #[test]
    fn every_catalog_tool_accepts_its_own_minimal_required_example() {
        // Smoke: the catalog's own required lists are satisfiable with typed
        // placeholder values, so the check can never reject every call to a
        // tool. Catches a catalog `required` entry with no matching property.
        for (name, schema) in schemas() {
            let properties = schema["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let mut args = Map::new();
            for key in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let property = properties.get(key).cloned().unwrap_or(json!({}));
                let placeholder = match property.get("type").and_then(Value::as_str) {
                    Some("integer") | Some("number") => json!(1),
                    Some("boolean") => json!(true),
                    Some("array") => json!([]),
                    Some("object") => json!({}),
                    _ => property
                        .get("enum")
                        .and_then(Value::as_array)
                        .and_then(|e| e.first().cloned())
                        .unwrap_or(json!("x")),
                };
                args.insert(key.to_string(), placeholder);
            }
            let result = validate_argument_values(name, &Value::Object(args.clone()));
            assert!(
                result.is_ok(),
                "{name} rejects its own required example {args:?}: {result:?}"
            );
        }
    }
}
