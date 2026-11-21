use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    json_utils::{json_bool, json_string},
    mcp_wire_read_service::{mcp_local_get_wires, mcp_local_list_wire_predicates},
    paths::existing_graph_dir,
    rdf_service::wire_predicate_uri,
    wire_predicates::predicate_label,
    wire_state::wire_is_active,
    workspace_entity_projection::workspace_wires,
};
use std::collections::BTreeSet;

pub(super) fn hosted_wire_predicates(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let local =
        mcp_local_list_wire_predicates(app.clone(), &serde_json::json!({ "graphId": graph_id }))?;
    Ok(local
        .get("predicates")
        .and_then(serde_json::Value::as_array)
        .map(|predicates| {
            predicates
                .iter()
                .map(|predicate| {
                    let name = json_string(predicate.get("name")).unwrap_or_default();
                    let category = json_string(predicate.get("category"));
                    serde_json::json!({
                        "uri": wire_predicate_uri(&name),
                        "label": json_string(predicate.get("label")).unwrap_or_else(|| predicate_label(&name)),
                        "category": category,
                        "builtin": category.as_deref() != Some("Custom"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default())
}

fn hosted_wire_summary_from_local(
    wire: &serde_json::Value,
    document_id: &str,
    direction: &str,
) -> serde_json::Value {
    let outgoing = direction == "outgoing";
    let other_document_id = if outgoing {
        json_string(wire.get("targetDocumentId"))
    } else {
        json_string(wire.get("sourceDocumentId"))
    };
    let other_artifact_id = if outgoing {
        json_string(wire.get("targetArtifactId"))
    } else {
        None
    };
    let other_graph_id = if outgoing {
        json_string(wire.get("targetGraphId"))
    } else {
        json_string(wire.get("sourceGraphId"))
    };
    let mut summary = serde_json::json!({
        "id": json_string(wire.get("id")).unwrap_or_default(),
        "predicate": json_string(wire.get("predicateUri").or_else(|| wire.get("predicate"))).unwrap_or_else(|| wire_predicate_uri("relatedTo")),
        "predicate_label": json_string(wire.get("predicateLabel")).unwrap_or_else(|| predicate_label("relatedTo")),
        "other_document_id": if other_artifact_id.is_some() { None } else { other_document_id.or_else(|| Some(document_id.to_string())) },
        "other_artifact_id": other_artifact_id,
        "other_graph_id": other_graph_id.unwrap_or_else(|| json_string(wire.get("targetGraphId")).unwrap_or_default()),
        "other_block_id": if outgoing { json_string(wire.get("targetBlockId")) } else { json_string(wire.get("sourceBlockId")) },
        "other_title": if outgoing { json_string(wire.get("targetTitle")) } else { json_string(wire.get("sourceTitle")) },
        "other_snippet": if outgoing { json_string(wire.get("targetSnippet")) } else { json_string(wire.get("sourceSnippet")) },
        "local_block_id": if outgoing { json_string(wire.get("sourceBlockId")) } else { json_string(wire.get("targetBlockId")) },
        "local_snippet": if outgoing { json_string(wire.get("sourceSnippet")) } else { json_string(wire.get("targetSnippet")) },
        "bidirectional": json_bool(wire.get("bidirectional")).unwrap_or(false),
        "snapshot_at": json_string(wire.get("snapshotAt").or_else(|| wire.get("createdAt"))),
    });
    // The region an outgoing artifact wire names, and the file's hash; the
    // text stays behind read_artifact (step 3).
    if other_artifact_id.is_some() {
        if let Some(region) = wire.get("targetRegion").filter(|region| region.is_object()) {
            summary["target_region"] = region.clone();
            summary["region_protected"] = serde_json::json!(true);
        }
        if let Some(hash) = json_string(wire.get("fileHash")) {
            summary["file_hash"] = serde_json::json!(hash);
        }
    }
    summary
}

pub(super) fn hosted_wires_for_document(
    app: AppHandle,
    graph_id: &str,
    document_id: &str,
    direction: &str,
) -> Result<serde_json::Value, String> {
    let local = mcp_local_get_wires(
        app,
        &serde_json::json!({
            "graphId": graph_id,
            "documentId": document_id,
            "direction": direction,
        }),
    )?;
    Ok(serde_json::Value::Array(
        local
            .get("wires")
            .and_then(serde_json::Value::as_array)
            .map(|wires| {
                wires
                    .iter()
                    .map(|wire| hosted_wire_summary_from_local(wire, document_id, direction))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    ))
}

pub(super) fn hosted_wired_blocks(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Vec<String>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let mut block_ids = BTreeSet::new();
    for wire in workspace_wires(workspace.snapshot.as_ref()) {
        if !wire_is_active(&wire) {
            continue;
        }
        if json_string(wire.get("sourceDocumentId")).as_deref() != Some(document_id) {
            continue;
        }
        if let Some(block_id) = json_string(wire.get("sourceBlockId")) {
            block_ids.insert(block_id);
        }
    }
    Ok(block_ids.into_iter().collect())
}

pub(super) fn hosted_wire_bundle(
    app: AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<serde_json::Value, String> {
    let outgoing = hosted_wires_for_document(app.clone(), graph_id, document_id, "outgoing")?;
    let incoming = hosted_wires_for_document(app.clone(), graph_id, document_id, "incoming")?;
    let wired_block_ids = hosted_wired_blocks(&app, graph_id, document_id)?;
    Ok(serde_json::json!({
        "outgoing_wires": outgoing.as_array().cloned().unwrap_or_default(),
        "incoming_wires": incoming.as_array().cloned().unwrap_or_default(),
        "wired_block_ids": wired_block_ids,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_wire_summary_maps_local_and_other_sides() {
        let wire = serde_json::json!({
            "id": "wire-a",
            "predicateUri": "http://mnemosyne.ai/vocab#partOf",
            "predicateLabel": "is part of",
            "sourceDocumentId": "doc-a",
            "targetGraphId": "graph-b",
            "targetDocumentId": "doc-b",
            "sourceBlockId": "block-a",
            "targetBlockId": "block-b",
            "sourceSnippet": "source text",
            "targetSnippet": "target text",
            "targetTitle": "Target",
            "createdAt": "1000"
        });

        let outgoing = hosted_wire_summary_from_local(&wire, "doc-a", "outgoing");
        assert_eq!(outgoing["other_document_id"], "doc-b");
        assert_eq!(outgoing["other_graph_id"], "graph-b");
        assert_eq!(outgoing["local_block_id"], "block-a");
        assert_eq!(outgoing["other_block_id"], "block-b");
        assert_eq!(outgoing["other_title"], "Target");
        assert_eq!(outgoing["snapshot_at"], "1000");

        let incoming = hosted_wire_summary_from_local(&wire, "doc-b", "incoming");
        assert_eq!(incoming["other_document_id"], "doc-a");
        assert_eq!(incoming["local_block_id"], "block-b");
        assert_eq!(incoming["other_block_id"], "block-a");
    }

    #[test]
    fn hosted_wire_summary_preserves_an_artifact_target() {
        let wire = serde_json::json!({
            "id": "w-artifact",
            "sourceDocumentId": "doc-companion",
            "targetArtifactId": "artifact-pdf",
            "targetTitle": "Paper.pdf",
            "predicate": "annotates"
        });
        let outgoing = hosted_wire_summary_from_local(&wire, "doc-companion", "outgoing");
        assert_eq!(outgoing["other_artifact_id"], "artifact-pdf");
        assert_eq!(outgoing["other_document_id"], serde_json::Value::Null);
        assert_eq!(outgoing["other_title"], "Paper.pdf");
    }
}
