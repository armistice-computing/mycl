use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    json_utils::{json_bool, json_string},
    mcp_utils::{mcp_arg_string, mcp_required_document_id, mcp_required_graph_id},
    paths::existing_graph_dir,
    rdf_service::wire_predicate_uri,
    wire_predicates::{builtin_wire_predicates, predicate_label, predicate_short_name},
    wire_state::{wire_is_active, wire_matches_document},
    workspace_entity_projection::{workspace_document_title, workspace_documents, workspace_wires},
};
use std::collections::BTreeSet;

fn wire_summary(
    wire: &serde_json::Value,
    graph_id: &str,
    documents: &[serde_json::Value],
) -> serde_json::Value {
    let predicate = json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
    let predicate_uri = wire_predicate_uri(&predicate);
    let source_document_id = json_string(wire.get("sourceDocumentId"));
    let target_document_id = json_string(wire.get("targetDocumentId"));
    let target_artifact_id = json_string(wire.get("targetArtifactId"));
    let source_title = json_string(wire.get("sourceTitle"))
        .or_else(|| workspace_document_title(documents, source_document_id.as_deref()));
    let target_title = json_string(wire.get("targetTitle"))
        .or_else(|| workspace_document_title(documents, target_document_id.as_deref()));

    let mut value = serde_json::json!({
        "id": json_string(wire.get("id")).unwrap_or_default(),
        "sourceGraphId": graph_id,
        "sourceDocumentId": source_document_id,
        "targetGraphId": json_string(wire.get("targetGraphId")).unwrap_or_else(|| graph_id.to_string()),
        "targetDocumentId": target_document_id,
        "targetArtifactId": target_artifact_id,
        "predicate": predicate_short_name(&predicate_uri),
        "predicateUri": predicate_uri,
        "predicateLabel": predicate_label(&predicate),
        "bidirectional": json_bool(wire.get("bidirectional")).unwrap_or(false),
    });

    for (json_key, output_key) in [
        ("sourceBlockId", "sourceBlockId"),
        ("targetBlockId", "targetBlockId"),
        ("sourceMarkId", "sourceMarkId"),
        ("targetMarkId", "targetMarkId"),
        ("inverseOf", "inverseOf"),
        ("sourceSnippet", "sourceSnippet"),
        ("targetSnippet", "targetSnippet"),
    ] {
        if let Some(field_value) = json_string(wire.get(json_key)) {
            value[output_key] = serde_json::json!(field_value);
        }
    }
    if let Some(title) = source_title {
        value["sourceTitle"] = serde_json::json!(title);
    }
    if let Some(title) = target_title {
        value["targetTitle"] = serde_json::json!(title);
    }
    label_wire_family(&mut value, wire);
    label_artifact_region(&mut value, wire);
    value
}

/// An artifact wire's region and file hash ride every read (neither is
/// file text); the region's words do not. They are read through
/// `read_artifact` with the wire id, which `artifacts.read` gates: a
/// `documents.read` caller never receives referenced-file text (Eschaton's
/// release-candidate ruling; design seed, step 3).
pub(crate) fn label_artifact_region(out: &mut serde_json::Value, wire: &serde_json::Value) {
    if let Some(region) = wire.get("targetRegion").filter(|region| region.is_object()) {
        out["targetRegion"] = region.clone();
        if let Ok(parsed) = crate::artifact_regions::Region::parse_object(region) {
            out["targetRegionShort"] = serde_json::json!(parsed.short_form());
        }
        out["regionProtected"] = serde_json::json!(true);
        out["regionReader"] = serde_json::json!("read_artifact { wire_id }");
    }
    if let Some(hash) = json_string(wire.get("fileHash")) {
        out["fileHash"] = serde_json::json!(hash);
    }
}

/// Record and dream wires say so, and a dream wire says when it goes;
/// judgment wires, the ordinary kind, carry no label.
pub(crate) fn label_wire_family(out: &mut serde_json::Value, wire: &serde_json::Value) {
    let family = crate::wire_predicates::WireFamily::of_wire(wire);
    if family == crate::wire_predicates::WireFamily::Judgment {
        return;
    }
    out["family"] = serde_json::json!(family.name());
    if let Some(at) = crate::wire_state::wire_expires_at_ms(wire).and_then(chrono::DateTime::from_timestamp_millis) {
        out["expiresAt"] = serde_json::json!(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
}

/// Every active wire touching a document, for `read_document`'s first page:
/// one compact entry per wire, seen from this document (which of its blocks,
/// which way, to what), ordered document-level first and then by the order of
/// the document's blocks, so the list reads alongside the text. Snippets are
/// left out to keep it small; `get_wires` has them. Cloud 1.0 gave counts
/// only; Eschaton asked for the full list (2026-10-16).
pub(super) fn document_wires_for_read(
    snapshot: &serde_json::Value,
    graph_id: &str,
    document_id: &str,
    block_order: &[&str],
    read_block_texts: &dyn Fn(&str) -> std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let titles: std::collections::HashMap<String, String> = snapshot["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                crate::workspace_entity_projection::workspace_entity_id(entry)?,
                crate::workspace_entity_projection::workspace_entity_title(entry)?,
            ))
        })
        .collect();
    let position: std::collections::HashMap<&str, usize> =
        block_order.iter().enumerate().map(|(index, id)| (*id, index)).collect();
    // Each other document's block texts, read once per call and only for
    // documents some wire reaches at block level.
    let mut block_texts: std::collections::HashMap<String, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    let mut block_text = |document: &str, block: &str| -> Option<String> {
        let texts = block_texts
            .entry(document.to_string())
            .or_insert_with(|| read_block_texts(document));
        texts.get(block).cloned()
    };

    let (mut outgoing, mut incoming) = (0usize, 0usize);
    let mut entries: Vec<(usize, serde_json::Value)> = Vec::new();
    for wire in snapshot["wires"].as_array().into_iter().flatten() {
        let id = json_string(wire.get("id")).unwrap_or_default();
        if id.ends_with("-inv") || !wire_is_active(wire) || !wire_matches_document(wire, document_id, "both") {
            continue;
        }
        let source = json_string(wire.get("sourceDocumentId"));
        let is_outgoing = source.as_deref() == Some(document_id);
        let (here, there) = if is_outgoing { ("source", "target") } else { ("target", "source") };
        let other_document = json_string(wire.get(format!("{there}DocumentId").as_str()));
        let other_artifact = if is_outgoing {
            json_string(wire.get("targetArtifactId"))
        } else {
            None
        };
        let other_graph = if is_outgoing {
            json_string(wire.get("targetGraphId")).filter(|graph| graph != graph_id)
        } else {
            None
        };
        let predicate = json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
        let block_id = json_string(wire.get(format!("{here}BlockId").as_str()));

        let mut entry = serde_json::json!({
            "id": id,
            "direction": if is_outgoing { "outgoing" } else { "incoming" },
            "predicate": predicate_short_name(&wire_predicate_uri(&predicate)),
        });
        label_wire_family(&mut entry, wire);
        if let Some(block) = &block_id {
            entry["blockId"] = serde_json::json!(block);
        }
        if let Some(other) = &other_document {
            entry["otherDocumentId"] = serde_json::json!(other);
            // The document's current title first: the one stored on the wire
            // is from when it was made and goes stale on a rename. Only for a
            // document in this graph; a foreign id may collide with a local one.
            let current = if other_graph.is_none() { titles.get(other).cloned() } else { None };
            let title = current.or_else(|| json_string(wire.get(format!("{there}Title").as_str())));
            if let Some(title) = title {
                entry["otherTitle"] = serde_json::json!(title);
            }
        }
        if let Some(other) = &other_artifact {
            entry["otherArtifactId"] = serde_json::json!(other);
            if let Some(title) = json_string(wire.get("targetTitle")) {
                entry["otherTitle"] = serde_json::json!(title);
            }
            label_artifact_region(&mut entry, wire);
        }
        if let Some(block) = json_string(wire.get(format!("{there}BlockId").as_str())) {
            entry["otherBlockId"] = serde_json::json!(block);
            // The other block's words, read now (Eschaton, 2026-10-16: worth
            // the tokens). Not the snippet stored on the wire: that is from
            // when the wire was made, goes stale when the block is edited,
            // and falls back to the document's first block when the block
            // isn't found (angel review). A block that is gone gets a flag,
            // not someone else's words. Document-level ends get no snippet;
            // this end's words are in the text being read.
            if let (Some(other), None) = (&other_document, &other_graph) {
                match block_text(other, &block) {
                    // An empty block (a divider, an empty paragraph) has no words to show.
                    Some(text) if text.trim().is_empty() => {}
                    Some(text) => entry["otherSnippet"] = serde_json::json!(text),
                    None => entry["otherBlockMissing"] = serde_json::json!(true),
                }
            }
        }
        if let Some(graph) = other_graph {
            entry["otherGraphId"] = serde_json::json!(graph);
        }
        if json_bool(wire.get("bidirectional")).unwrap_or(false) {
            entry["bidirectional"] = serde_json::json!(true);
        }
        if is_outgoing { outgoing += 1 } else { incoming += 1 }
        // Document-level wires first, then in block order; a block no longer
        // in the document goes last.
        let rank = match &block_id {
            None => 0,
            Some(block) => position.get(block.as_str()).map_or(usize::MAX, |index| index + 1),
        };
        entries.push((rank, entry));
    }
    entries.sort_by_key(|(rank, _)| *rank);

    serde_json::json!({
        "wireCount": { "outgoing": outgoing, "incoming": incoming, "total": outgoing + incoming },
        "wires": entries.into_iter().map(|(_, entry)| entry).collect::<Vec<_>>(),
    })
}

/// A document's blocks as id -> current text (first 240 characters, as wire
/// snippets have always been cut), from its persisted record. Empty when the
/// document can't be read.
pub(super) fn document_block_texts(
    graph_dir: &std::path::Path,
    document_id: &str,
) -> std::collections::HashMap<String, String> {
    let Some(record) = crate::crdt_engine::workspace_ops::document_record_json(graph_dir, document_id) else {
        return Default::default();
    };
    record["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let id = json_string(block.get("id").or_else(|| block.get("blockId")))?;
            let text = json_string(block.get("content").or_else(|| block.get("text"))).unwrap_or_default();
            Some((id, text.chars().take(240).collect()))
        })
        .collect()
}

pub(crate) fn mcp_local_get_wires(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    // Step 5: the question from the repository side. An artifact (by id or
    // by path) in place of a document answers with every wire that points
    // at it, narrowed to a line when one is given.
    let artifact_id = mcp_arg_string(arguments, &["artifactId", "artifact_id"]).filter(|id| !id.trim().is_empty());
    let artifact_path = mcp_arg_string(arguments, &["artifactPath", "artifact_path"]).filter(|path| !path.trim().is_empty());
    if artifact_id.is_some() || artifact_path.is_some() {
        if mcp_arg_string(arguments, &["documentId", "document_id"]).is_some() {
            return Err("give a document or an artifact, not both".to_string());
        }
        let line = arguments
            .get("line")
            .filter(|value| !value.is_null())
            .map(|value| value.as_u64().filter(|line| *line >= 1).ok_or_else(|| "line must be a positive integer".to_string()))
            .transpose()?;
        return wires_for_artifact(&app, &graph_id, artifact_id, artifact_path, line);
    }
    let document_id = mcp_required_document_id(arguments)?;
    let direction = mcp_arg_string(arguments, &["direction"]).unwrap_or_else(|| "both".to_string());
    if !matches!(direction.as_str(), "outgoing" | "incoming" | "both") {
        return Err("direction must be outgoing, incoming, or both".to_string());
    }
    let predicate =
        mcp_arg_string(arguments, &["predicate"]).map(|value| wire_predicate_uri(&value));
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let documents = workspace_documents(snapshot);
    let wires = workspace_wires(snapshot);
    let mut results = Vec::new();

    for wire in wires {
        let id = json_string(wire.get("id")).unwrap_or_default();
        if id.ends_with("-inv") || !wire_is_active(&wire) {
            continue;
        }
        if !wire_matches_document(&wire, &document_id, &direction) {
            continue;
        }
        if let Some(predicate) = &predicate {
            let wire_predicate =
                json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
            if wire_predicate_uri(&wire_predicate) != *predicate {
                continue;
            }
        }
        results.push(wire_summary(&wire, &graph_id, &documents));
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "direction": direction,
        "wires": results,
        "count": results.len(),
    }))
}

/// Every active wire whose target is the artifact, with the source block's
/// current words and the region; with `line`, only the line regions that
/// hold it, plus region-less wires flagged `wholeFile`. A path names an
/// existing referenced record only; nothing is made by asking.
fn wires_for_artifact(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: Option<String>,
    artifact_path: Option<String>,
    line: Option<u64>,
) -> Result<serde_json::Value, String> {
    let (artifact_id, canonical) = match (artifact_id, artifact_path) {
        (Some(id), _) => (id.trim().to_string(), None),
        (None, Some(path)) => {
            let (canonical, found) = crate::artifact_reference::find_referenced_by_path(app, graph_id, &path)?;
            match found {
                Some(id) => (id, Some(canonical)),
                None => {
                    return Ok(serde_json::json!({
                        "graph_id": graph_id,
                        "artifactId": serde_json::Value::Null,
                        "path": canonical,
                        "known": false,
                        "line": line,
                        "wires": [],
                        "count": 0,
                        "note": "no referenced record holds this path; nothing in the graph points at it",
                    }))
                }
            }
        }
        (None, None) => return Err("artifactId or artifactPath is required".to_string()),
    };
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let known = snapshot
        .and_then(|snapshot| snapshot.get("artifacts"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|record| json_string(record.get("id")).as_deref() == Some(artifact_id.as_str()));
    if !known {
        return Err(format!("artifact not found: {artifact_id}"));
    }
    let documents = workspace_documents(snapshot);
    let mut block_texts: std::collections::HashMap<String, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    let mut results = Vec::new();
    for wire in workspace_wires(snapshot) {
        let id = json_string(wire.get("id")).unwrap_or_default();
        if id.ends_with("-inv") || !wire_is_active(&wire) {
            continue;
        }
        if json_string(wire.get("targetArtifactId")).as_deref() != Some(artifact_id.as_str()) {
            continue;
        }
        // wholeFile means exactly "no region on the wire"; a region that
        // cannot be read is neither whole-file nor a line match.
        let region_value = wire.get("targetRegion").filter(|value| !value.is_null());
        let whole_file = region_value.is_none();
        let region = region_value.and_then(|value| crate::artifact_regions::Region::parse_object(value).ok());
        let unreadable = region_value.is_some() && region.is_none();
        if let Some(line) = line {
            match region.as_ref().and_then(|region| region.contains_line(line)) {
                Some(true) => {}
                Some(false) => continue,
                // A region of another kind cannot hold a line; a region-less
                // wire is about the whole file, so it is kept and flagged.
                None if whole_file => {}
                None => continue,
            }
        }
        let mut summary = wire_summary(&wire, graph_id, &documents);
        if whole_file {
            summary["wholeFile"] = serde_json::json!(true);
        }
        if unreadable {
            summary["regionUnreadable"] = serde_json::json!(true);
        }
        // The source block's current words, read now, not the stored snippet.
        if let (Some(document), Some(block)) = (json_string(wire.get("sourceDocumentId")), json_string(wire.get("sourceBlockId"))) {
            let texts = block_texts
                .entry(document.clone())
                .or_insert_with(|| document_block_texts(&graph_dir, &document));
            match texts.get(&block) {
                Some(text) if !text.trim().is_empty() => summary["sourceText"] = serde_json::json!(text),
                Some(_) => {}
                None => summary["sourceBlockMissing"] = serde_json::json!(true),
            }
        }
        results.push(summary);
    }
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "artifactId": artifact_id,
        "path": canonical,
        "known": true,
        "line": line,
        "wires": results,
        "count": results.len(),
        "note": "regions are the ones stored at wiring time; read_artifact with a wire id says whether the file moved under them",
    }))
}

pub(super) fn mcp_local_list_wire_predicates(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let mut predicates = Vec::new();
    let mut seen = BTreeSet::new();

    for (name, label, category) in builtin_wire_predicates() {
        seen.insert(name.to_string());
        predicates.push(serde_json::json!({
            "name": name,
            "label": label,
            "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
        }));
    }

    for wire in workspace_wires(workspace.snapshot.as_ref()) {
        let Some(predicate) = json_string(wire.get("predicate")) else {
            continue;
        };
        let short_name = predicate_short_name(&predicate);
        if seen.insert(short_name.clone()) {
            predicates.push(serde_json::json!({
                "name": short_name,
                "label": predicate_label(&predicate),
                "category": "Custom",
            }));
        }
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "predicates": predicates,
        "count": predicates.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_summary_normalizes_predicates_and_document_titles() {
        let wire = serde_json::json!({
            "id": "wire-a",
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "predicate": "http://mnemosyne.ai/vocab#supports",
            "sourceBlockId": "block-a",
            "targetSnippet": "target text",
            "bidirectional": true
        });
        let documents = vec![
            serde_json::json!({ "documentId": "doc-a", "title": "Source" }),
            serde_json::json!({ "documentId": "doc-b", "title": "Target" }),
        ];

        let value = wire_summary(&wire, "graph-a", &documents);

        assert_eq!(value["id"], "wire-a");
        assert_eq!(value["sourceGraphId"], "graph-a");
        assert_eq!(value["targetGraphId"], "graph-a");
        assert_eq!(value["predicate"], "supports");
        assert_eq!(value["predicateLabel"], "supports");
        assert_eq!(value["sourceTitle"], "Source");
        assert_eq!(value["targetTitle"], "Target");
        assert_eq!(value["sourceBlockId"], "block-a");
        assert_eq!(value["targetSnippet"], "target text");
        assert_eq!(value["bidirectional"], true);
    }

    #[test]
    fn artifact_targets_survive_get_wires_and_read_document_projections() {
        let wire = serde_json::json!({
            "id": "w-artifact",
            "sourceDocumentId": "doc-companion",
            "targetArtifactId": "artifact-pdf",
            "targetTitle": "Paper.pdf",
            "predicate": "annotates"
        });
        let summary = wire_summary(&wire, "graph-a", &[]);
        assert_eq!(summary["targetArtifactId"], "artifact-pdf");
        assert_eq!(summary["targetDocumentId"], serde_json::Value::Null);

        let snapshot = serde_json::json!({
            "documents": [{ "id": "doc-companion", "title": "Companion" }],
            "wires": [wire]
        });
        let read = document_wires_for_read(&snapshot, "graph-a", "doc-companion", &[], &|_| Default::default());
        assert_eq!(read["wireCount"], serde_json::json!({ "outgoing": 1, "incoming": 0, "total": 1 }));
        assert_eq!(read["wires"][0]["otherArtifactId"], "artifact-pdf");
        assert_eq!(read["wires"][0]["otherTitle"], "Paper.pdf");
        assert!(read["wires"][0].get("otherDocumentId").is_none());
    }

    #[test]
    fn read_document_lists_every_wire_seen_from_the_document_in_block_order() {
        let snapshot = serde_json::json!({
            "documents": [
                { "id": "doc", "title": "Here" },
                { "id": "other", "title": "Over There" },
            ],
            "wires": [
                { "id": "w-late", "sourceDocumentId": "doc", "sourceBlockId": "b2",
                  "targetDocumentId": "other", "predicate": "supports" },
                { "id": "w-in", "sourceDocumentId": "other", "sourceBlockId": "o1",
                  "targetDocumentId": "doc", "targetBlockId": "b1", "predicate": "contradicts",
                  "sourceSnippet": "the words over there", "targetSnippet": "the words here" },
                { "id": "w-doc", "sourceDocumentId": "doc", "targetDocumentId": "other",
                  "predicate": "http://mnemosyne.ai/vocab#flowsInto", "bidirectional": true,
                  "targetSnippet": "Over There" },
                { "id": "w-gone", "sourceDocumentId": "doc", "targetDocumentId": "other",
                  "predicate": "supports", "deletedAt": "2026-10-01T00:00:00Z" },
                { "id": "w-in-inv", "sourceDocumentId": "doc", "targetDocumentId": "other",
                  "predicate": "contradicts" },
                { "id": "w-elsewhere", "sourceDocumentId": "other", "targetDocumentId": "third" },
                { "id": "w-gone-block", "sourceDocumentId": "doc", "sourceBlockId": "b2",
                  "targetDocumentId": "other", "targetBlockId": "gone", "predicate": "supports",
                  "targetSnippet": "first block of other, not the target" },
            ],
        });
        // The other document as it is now: o1 has been edited since the wire
        // was made (its stored snippet is stale); "gone" no longer exists.
        let read = |document: &str| -> std::collections::HashMap<String, String> {
            if document == "other" {
                [("o1".to_string(), "the words over there, edited".to_string())].into()
            } else {
                Default::default()
            }
        };
        let result = document_wires_for_read(&snapshot, "g", "doc", &["b1", "b2"], &read);
        assert_eq!(result["wireCount"], serde_json::json!({ "outgoing": 3, "incoming": 1, "total": 4 }));
        let ids: Vec<&str> = result["wires"].as_array().unwrap().iter().map(|w| w["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["w-doc", "w-in", "w-late", "w-gone-block"], "document-level first, then block order");
        let gone = &result["wires"][3];
        assert_eq!(gone["otherBlockMissing"], true);
        assert!(gone.get("otherSnippet").is_none(), "no fallback to another block's words");
        let incoming = &result["wires"][1];
        assert_eq!(incoming["direction"], "incoming");
        assert_eq!(incoming["blockId"], "b1");
        assert_eq!(incoming["otherDocumentId"], "other");
        assert_eq!(incoming["otherBlockId"], "o1");
        assert_eq!(incoming["otherTitle"], "Over There");
        assert_eq!(incoming["predicate"], "contradicts");
        assert_eq!(incoming["otherSnippet"], "the words over there, edited", "the other block's words now, not the stored snippet");
        assert!(result["wires"][0].get("otherSnippet").is_none(), "no snippet for a document-level end");
        let document_level = &result["wires"][0];
        assert_eq!(document_level["predicate"], "flowsInto");
        assert_eq!(document_level["bidirectional"], true);
        assert!(document_level.get("blockId").is_none());
    }
}
