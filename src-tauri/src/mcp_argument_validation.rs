//! Tool argument names are a closed contract; content/attribute bags are not.
//! This is deliberately not a second JSON Schema engine. Handlers retain value
//! and operation-specific validation. The advertised catalog owns allowed names.
use serde_json::Value;
use std::{collections::BTreeMap, sync::OnceLock};

pub(super) fn validate_argument_names(name: &str, arguments: &Value) -> Result<(), String> {
    static SCHEMAS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    let schemas = SCHEMAS.get_or_init(|| {
        let catalog: Value = serde_json::from_str(include_str!("mcp_tool_catalog.json"))
            .expect("valid embedded catalog");
        catalog["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| {
                (
                    tool["name"].as_str().unwrap().to_owned(),
                    tool["inputSchema"].clone(),
                )
            })
            .collect()
    });
    let schema = schemas
        .get(name)
        .ok_or_else(|| format!("unknown tool {name}"))?;
    let object = arguments
        .as_object()
        .ok_or_else(|| format!("{name}: arguments must be an object"))?;
    let properties = schema["properties"].as_object().expect("tool properties");
    let unknown: Vec<_> = object
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{name}: unknown parameter(s): {}. Allowed parameters: {}. No operation was performed.",
            unknown.join(", "),
            properties.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    // Shrubbery sends these null placeholders on ordinary document creation.
    // The handler has never implemented their non-null semantics; acknowledging
    // them as successful metadata writes would preserve a silent data-loss bug.
    if name == "create_document" {
        for key in ["documentKind", "dailyNoteDate", "dailyNoteTimeZone"] {
            if object.get(key).is_some_and(|value| !value.is_null()) {
                return Err(format!("create_document: {key} is not supported; omit it or send null. No operation was performed."));
            }
        }
    }
    // Explicit aliases must not supply contradictory identities or guards.
    for (key, value) in object {
        let mut snake = String::new();
        for c in key.chars() {
            if c.is_ascii_uppercase() {
                snake.push('_');
                snake.push(c.to_ascii_lowercase());
            } else {
                snake.push(c);
            }
        }
        if snake != *key
            && properties.contains_key(&snake)
            && object.get(&snake).is_some_and(|other| other != value)
        {
            return Err(format!("{name}: conflicting parameters {key} and {snake}. Supply only one spelling. No operation was performed."));
        }
    }
    let alias_groups: &[&[&str]] = match name {
        "update_blocks" => &[&["edits", "updates"], &["attrs", "attributes"]],
        "edit_block_text" => &[&["operations", "ops"]],
        "move_folder" => &[
            &["newParentId", "new_parent_id", "parentId", "parent_id"],
            &["newOrder", "new_order", "order"],
        ],
        _ => &[],
    };
    for group in alias_groups {
        let supplied: Vec<_> = group
            .iter()
            .filter_map(|key| object.get(*key).map(|value| (*key, value)))
            .collect();
        if supplied.windows(2).any(|pair| pair[0].1 != pair[1].1) {
            return Err(format!("{name}: conflicting aliases {}. Supply only one spelling. No operation was performed.", supplied.iter().map(|(key, _)| *key).collect::<Vec<_>>().join(", ")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn misspelled_anchor_and_song_body_are_rejected() {
        let error = validate_argument_names(
            "insert_blocks",
            &json!({"content":"hello", "after_block_id":"b"}),
        )
        .unwrap_err();
        assert!(
            error.contains("after_block_id")
                && error.contains("blockId")
                && error.contains("No operation")
        );
        assert!(validate_argument_names("sing", &json!({"content":"verse"})).is_err());
    }

    #[test]
    fn every_tool_has_a_closed_top_level_contract() {
        let catalog: Value = serde_json::from_str(include_str!("mcp_tool_catalog.json")).unwrap();
        for tool in catalog["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
            assert!(
                validate_argument_names(name, &json!({"not_a_garden_parameter":true})).is_err(),
                "{name}"
            );
            assert!(validate_argument_names(name, &json!([])).is_err(), "{name}");
        }
    }

    #[test]
    fn explicit_aliases_guards_and_free_form_attributes_survive() {
        assert!(validate_argument_names("update_blocks", &json!({"graph_id":"g", "document_id":"d", "block_id":"b", "attributes":{"arbitrary-key":true}, "expected_revision":3,"force":false})).is_ok());
        assert!(
            validate_argument_names("write_document", &json!({"graphId":"a", "graph_id":"b"}))
                .is_err()
        );
        assert!(
            validate_argument_names("write_document", &json!({"graphId":"a", "graph_id":"a"}))
                .is_ok()
        );
    }

    #[test]
    fn frontend_null_placeholders_and_existing_mutation_guards() {
        assert!(validate_argument_names("create_document", &json!({"graphId":"g","documentId":"d","title":"new","parentId":null,"order":null,"documentKind":null,"dailyNoteDate":null,"dailyNoteTimeZone":null})).is_ok());
        assert!(
            validate_argument_names("create_document", &json!({"documentKind":"daily-note"}))
                .unwrap_err()
                .contains("not supported")
        );
        assert!(validate_argument_names("reindex_graph", &json!({"graphId":"g"})).is_ok());
        assert!(validate_argument_names(
            "delete",
            &json!({"type":"blocks","expectedRevision":2,"force":false,"agent":"epsilon"})
        )
        .is_ok());
        assert!(
            validate_argument_names("move_folder", &json!({"newParentId":"a","parentId":"b"}))
                .is_err()
        );
        assert!(validate_argument_names(
            "update_blocks",
            &json!({"attrs":{"a":1},"attributes":{"a":2}})
        )
        .is_err());
    }
}
