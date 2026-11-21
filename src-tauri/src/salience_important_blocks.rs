use crate::app_runtime::AppHandle;
use crate::{
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    salience_score_projection::{find_projected_block, local_block_value_scores_for},
};
use std::collections::BTreeMap;

pub(super) fn mcp_local_get_important_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let document_filter = mcp_arg_string(arguments, &["document_id", "documentId"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 5).clamp(1, 20);
    let valence_filter = mcp_arg_string(arguments, &["valence"]);
    let mut scored = local_block_value_scores_for(
        &app,
        &graph_id,
        &observer,
        document_filter.as_deref(),
        None,
        None,
        // Values outlive documents: a scored block whose document is gone is
        // skipped below, so rank extra candidates and truncate after the skip.
        // Otherwise `limit: 5` can return 4.
        (limit * 4).max(20),
        None,
        valence_filter.as_deref(),
    )?;
    let mut document_map = BTreeMap::new();

    let mut blocks = Vec::new();
    for score in scored.drain(..) {
        let document_id = score
            .get("document_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let block_id = score
            .get("block_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let document = document_map.entry(document_id.clone()).or_insert_with(|| {
            crate::document_service::read_document_cold(
                app.clone(),
                graph_id.clone(),
                document_id.clone(),
            )
            .ok()
        });
        let Some(document) = document.as_ref() else {
            continue;
        };
        let Some(block) = find_projected_block(document, &block_id) else {
            continue;
        };
        let content = block.content;
        let (importance, valence) = reported_values(&score);
        blocks.push(serde_json::json!({
            "content": content.clone(),
            "text": content,
            "document": document.title,
            "score": score.get("composite_score").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "importance": importance,
            "valence": valence,
            // The person's own rating, when they gave one; null otherwise.
            "userImportance": score.get("user_importance").cloned().unwrap_or(serde_json::Value::Null),
            "userValence": score.get("user_valence").cloned().unwrap_or(serde_json::Value::Null),
            "block_id": block_id,
            "blockId": block_id,
            "doc_id": document_id,
            "docId": document_id,
            "document_id": document_id,
            "documentId": document_id,
            "block_wires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "blockWires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "doc_wires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "docWires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "tags": score.get("tags").cloned().unwrap_or_else(|| serde_json::json!([])),
        }));
    }
    blocks.sort_by(|left, right| {
        let left_score = left
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let right_score = right
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    blocks.truncate(limit);
    Ok(serde_json::json!({
        "blocks": blocks,
        "count": blocks.len(),
        "source": "local-value-store",
        "value_store_available": true,
        "valueStoreAvailable": true,
    }))
}

/// The importance and valence a block is reported with: the person's rating
/// and the agents' ratings combined, which is what the block was ranked by
/// and what the person sees beside it. A wired block nobody has rated has no
/// combined value and reports the agents' (zero).
fn reported_values(score: &serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    let pick = |combined: &str, agents: &str| {
        score
            .get(combined)
            .filter(|value| value.is_number())
            .or_else(|| score.get(agents))
            .cloned()
            .unwrap_or_else(|| serde_json::json!(0.0))
    };
    (
        pick("combined_importance", "cumulative_importance"),
        pick("combined_valence", "cumulative_valence"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_block_the_person_rated_reports_the_combined_values() {
        // The person gave 4 and +2; no agent has rated it.
        let row = json!({
            "cumulative_importance": 0.0, "cumulative_valence": 0.0,
            "combined_importance": 2.3219, "combined_valence": 1.585,
            "user_importance": 4.0, "user_valence": 2.0,
        });
        assert_eq!(reported_values(&row), (json!(2.3219), json!(1.585)));
    }

    #[test]
    fn a_wired_block_nobody_rated_reports_zero() {
        let row = json!({ "cumulative_importance": 0.0, "cumulative_valence": 0.0 });
        assert_eq!(reported_values(&row), (json!(0.0), json!(0.0)));
    }
}
