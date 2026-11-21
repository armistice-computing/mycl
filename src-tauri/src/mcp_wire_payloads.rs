use crate::mcp_utils::{mcp_arg_bool_with_fallback, mcp_arg_string_with_fallback};

pub(crate) fn wire_inputs(arguments: &serde_json::Value) -> Vec<serde_json::Value> {
    arguments
        .get("wires")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter(|value| value.is_object())
                .cloned()
                .collect()
        })
        .unwrap_or_else(|| vec![arguments.clone()])
}

pub(super) fn wire_create_payload(
    wire_input: &serde_json::Value,
    arguments: &serde_json::Value,
    graph_id: &str,
) -> Result<(String, serde_json::Value), String> {
    let source_document_id = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &[
            "source_document_id",
            "sourceDocumentId",
            "document_id",
            "documentId",
        ],
    )
    .ok_or_else(|| "source_document_id is required".to_string())?;
    let target_document_id = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["target_document_id", "targetDocumentId"],
    );
    let target_artifact_id = mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["target_artifact_id", "targetArtifactId"],
    );
    if target_document_id.is_some() == target_artifact_id.is_some() {
        return Err(
            "exactly one of target_document_id/targetDocumentId or target_artifact_id/targetArtifactId is required"
                .to_string(),
        );
    }
    let target_block_id = mcp_arg_string_with_fallback(wire_input, arguments, &["target_block_id", "targetBlockId"]);
    if target_artifact_id.is_some() && target_block_id.is_some() {
        return Err("targetBlockId/target_block_id requires a document target, not an artifact target".to_string());
    }
    // A region on the artifact: the short form (`L10-L20`, `p12`,
    // `b100-200`, `m:path`, `j:/pointer`) or the object. The server reads
    // and hashes it in the createWire operation; here it is only parsed.
    let region_value = ["region", "targetRegion", "target_region"]
        .iter()
        .find_map(|key| wire_input.get(*key).filter(|value| !value.is_null()))
        .or_else(|| {
            ["region", "targetRegion", "target_region"]
                .iter()
                .find_map(|key| arguments.get(*key).filter(|value| !value.is_null()))
        });
    let target_region = match region_value {
        Some(value) => {
            if target_artifact_id.is_none() {
                return Err("region/targetRegion requires an artifact target".to_string());
            }
            Some(crate::artifact_regions::Region::parse(value).map_err(|error| format!("region: {error}"))?.to_json())
        }
        None => None,
    };
    let target_graph_id =
        mcp_arg_string_with_fallback(wire_input, arguments, &["target_graph_id", "targetGraphId"])
            .unwrap_or_else(|| graph_id.to_string());
    let predicate = crate::crdt_engine::workspace_ops::validated_wire_predicate(
        mcp_arg_string_with_fallback(wire_input, arguments, &["predicate"]),
    )?;
    let payload = serde_json::json!({
        "wireId": mcp_arg_string_with_fallback(wire_input, arguments, &["wire_id", "wireId", "id"]),
        "sourceDocumentId": source_document_id.clone(),
        "sourceBlockId": mcp_arg_string_with_fallback(wire_input, arguments, &["source_block_id", "sourceBlockId"]),
        "sourceMarkId": mcp_arg_string_with_fallback(wire_input, arguments, &["source_mark_id", "sourceMarkId"]),
        "targetGraphId": target_graph_id,
        "targetDocumentId": target_document_id,
        "targetArtifactId": target_artifact_id,
        "targetBlockId": target_block_id,
        "targetRegion": target_region,
        "targetMarkId": mcp_arg_string_with_fallback(wire_input, arguments, &["target_mark_id", "targetMarkId"]),
        "predicate": predicate,
        "bidirectional": mcp_arg_bool_with_fallback(wire_input, arguments, &["bidirectional"], false),
    });
    // Refuse what the projection would quietly leave out (a wire, document,
    // artifact, block or mark id that cannot become an IRI), so the agent hears
    // about it instead of seeing success for a wire SPARQL and traversal can't see.
    let mut as_stored = payload.clone();
    as_stored["id"] = payload["wireId"].clone();
    if let Some(triple) = crate::rdf_wire_materializer::wire_unwritable_triple(graph_id, &as_stored) {
        return Err(format!(
            "this wire can't be stored as written: one of its ids (wire_id, a document or artifact, \
             block or mark id) cannot become an IRI. Offending triple: {triple}"
        ));
    }
    Ok((source_document_id, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The MCP tool refuses a bad predicate while building the payload, before
    /// any wire in the batch is enqueued.
    #[test]
    fn a_bad_predicate_is_refused_before_anything_is_enqueued() {
        let arguments = serde_json::json!({ "source_document_id": "a", "target_document_id": "b" });
        let wire = serde_json::json!({ "predicate": "draws on" });
        let error = wire_create_payload(&wire, &arguments, "graph-a").expect_err("refused");
        assert!(error.contains("not a usable name"), "{error}");
        let wire = serde_json::json!({ "predicate": " supports " });
        let (_, payload) = wire_create_payload(&wire, &arguments, "graph-a").expect("trimmed and kept");
        assert_eq!(payload["predicate"], "supports");
    }

    /// Ids that cannot become IRIs are refused at the edge too, not quietly
    /// left out of RDF after a reported success.
    #[test]
    fn ids_that_cannot_become_iris_are_refused_and_ordinary_ids_pass() {
        let arguments = serde_json::json!({ "source_document_id": "a", "target_document_id": "b" });
        for bad in [
            serde_json::json!({ "wire_id": "w 1" }),
            serde_json::json!({ "source_block_id": "block one" }),
            // a mark is only emitted beside its block
            serde_json::json!({ "target_block_id": "block-1", "target_mark_id": "m>1" }),
        ] {
            let error = wire_create_payload(&bad, &arguments, "graph-a").expect_err("refused");
            assert!(error.contains("can't be stored as written"), "{bad}: {error}");
        }
        let good = serde_json::json!({
            "wire_id": "w-1", "source_block_id": "block-1a2b", "target_mark_id": "m-9",
            "target_graph_id": "default_x1", "predicate": "supports",
        });
        wire_create_payload(&good, &arguments, "graph-a").expect("ordinary ids pass");
    }

    /// A region rides only on an artifact target, parsed from the short
    /// form or the object; `fileHash` is never taken from the caller.
    #[test]
    fn a_region_needs_an_artifact_target_and_parses_both_forms() {
        let arguments = serde_json::json!({ "source_document_id": "a" });
        let wire = serde_json::json!({ "target_artifact_id": "artifact-1", "region": "L10-L20" });
        let (_, payload) = wire_create_payload(&wire, &arguments, "graph-a").expect("short form");
        assert_eq!(payload["targetRegion"], serde_json::json!({ "kind": "text-lines", "start": 10, "end": 20 }));
        let wire = serde_json::json!({ "target_artifact_id": "artifact-1", "targetRegion": { "kind": "pdf-page", "page": 3 } });
        let (_, payload) = wire_create_payload(&wire, &arguments, "graph-a").expect("object form");
        assert_eq!(payload["targetRegion"]["page"], 3);
        let wire = serde_json::json!({ "target_document_id": "b", "region": "L1-L2" });
        let error = wire_create_payload(&wire, &arguments, "graph-a").expect_err("refused");
        assert!(error.contains("requires an artifact target"), "{error}");
        let wire = serde_json::json!({ "target_artifact_id": "artifact-1", "region": "L9-L3" });
        assert!(wire_create_payload(&wire, &arguments, "graph-a").is_err());
        let wire = serde_json::json!({ "target_artifact_id": "artifact-1" });
        let (_, payload) = wire_create_payload(&wire, &arguments, "graph-a").expect("no region");
        assert!(payload["targetRegion"].is_null());
        assert!(payload.get("fileHash").is_none());
    }

    #[test]
    fn wire_create_payload_uses_item_values_before_fallback_arguments() {
        let arguments = serde_json::json!({
            "source_document_id": "source-top",
            "target_document_id": "target-top",
            "target_graph_id": "target-graph-top",
            "predicate": "relatesTo",
            "bidirectional": true,
        });
        let wire = serde_json::json!({
            "id": "wire-a",
            "sourceDocumentId": "source-item",
            "targetDocumentId": "target-item",
            "source_block_id": "source-block",
        });

        let (source_document_id, payload) =
            wire_create_payload(&wire, &arguments, "graph-a").expect("wire payload");

        assert_eq!(source_document_id, "source-item");
        assert_eq!(payload.get("wireId"), Some(&serde_json::json!("wire-a")));
        assert_eq!(
            payload.get("sourceDocumentId"),
            Some(&serde_json::json!("source-item"))
        );
        assert_eq!(
            payload.get("targetDocumentId"),
            Some(&serde_json::json!("target-item"))
        );
        assert_eq!(
            payload.get("targetGraphId"),
            Some(&serde_json::json!("target-graph-top"))
        );
        assert_eq!(
            payload.get("sourceBlockId"),
            Some(&serde_json::json!("source-block"))
        );
        assert_eq!(
            payload.get("predicate"),
            Some(&serde_json::json!("relatesTo"))
        );
        assert_eq!(payload.get("bidirectional"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn artifact_targets_accept_both_spellings_and_require_one_target() {
        for arguments in [
            serde_json::json!({ "source_document_id": "doc", "target_artifact_id": "artifact-pdf" }),
            serde_json::json!({ "sourceDocumentId": "doc", "targetArtifactId": "artifact-pdf" }),
        ] {
            let (_, payload) = wire_create_payload(&arguments, &arguments, "graph-a")
                .expect("artifact target accepted");
            assert_eq!(payload["targetArtifactId"], "artifact-pdf");
            assert_eq!(payload["targetDocumentId"], serde_json::Value::Null);
        }

        let missing = serde_json::json!({ "source_document_id": "doc" });
        let error =
            wire_create_payload(&missing, &missing, "graph-a").expect_err("missing target refused");
        assert!(error.contains("exactly one"), "{error}");

        let ambiguous = serde_json::json!({
            "source_document_id": "doc",
            "target_document_id": "doc-target",
            "target_artifact_id": "artifact-pdf",
        });
        let error = wire_create_payload(&ambiguous, &ambiguous, "graph-a")
            .expect_err("ambiguous target refused");
        assert!(error.contains("exactly one"), "{error}");

        let block_on_artifact = serde_json::json!({
            "source_document_id": "doc",
            "target_artifact_id": "artifact-pdf",
            "target_block_id": "block-not-in-artifact",
        });
        let error = wire_create_payload(&block_on_artifact, &block_on_artifact, "graph-a")
            .expect_err("block on artifact refused");
        assert!(error.contains("requires a document target"), "{error}");
    }

    #[test]
    fn wire_inputs_ignores_non_object_batch_items() {
        assert_eq!(
            wire_inputs(&serde_json::json!({ "wires": [null, { "id": "wire-a" }, "x"] })),
            vec![serde_json::json!({ "id": "wire-a" })]
        );
    }
}
