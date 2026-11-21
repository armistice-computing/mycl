use crate::{
    salience_value_store::{
        cumulative_importance, cumulative_valence, local_value_composite_score, round4,
        LocalBlockValueRecord, LocalValueConfig,
    },
    salience_wire_context::{
        document_created_age_days, value_timestamp_age_days, value_wire_stats,
    },
};
use serde::Serialize;

// Wire shape emits both snake_case and camelCase aliases for every field
// because hosted clients consume snake_case while local UI consumes camelCase.
// We therefore declare the struct twice — once per casing — and flatten both
// into a single envelope. Field semantics are identical.
#[derive(Debug, Clone, Serialize)]
struct LocalBlockValueScoresSnake {
    document_id: String,
    block_id: String,
    cumulative_importance: f64,
    cumulative_valence: f64,
    raw_importance_sum: f64,
    raw_valence_sum: f64,
    importance_count: usize,
    valence_count: usize,
    combined_importance: f64,
    combined_valence: f64,
    composite_score: f64,
    valuation_count: usize,
    last_valuated_at: Option<String>,
    last_valuated: String,
    user_importance: Option<f64>,
    user_valence: Option<f64>,
    tags: Vec<String>,
    block_wire_count: usize,
    doc_wire_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalBlockValueScoresCamel {
    document_id: String,
    block_id: String,
    cumulative_importance: f64,
    cumulative_valence: f64,
    raw_importance_sum: f64,
    raw_valence_sum: f64,
    importance_count: usize,
    valence_count: usize,
    combined_importance: f64,
    combined_valence: f64,
    composite_score: f64,
    valuation_count: usize,
    last_valuated_at: Option<String>,
    last_valuated: String,
    user_importance: Option<f64>,
    user_valence: Option<f64>,
    block_wire_count: usize,
    doc_wire_count: usize,
}

/// Local block valuation scores. The wire shape carries every field under both
/// snake_case and camelCase aliases so hosted clients (snake_case) and local UI
/// (camelCase) can read the same payload. The two flattened sub-structs are
/// type-safe; serde merges them into one flat object on serialize.
#[derive(Debug, Clone, Serialize)]
pub(super) struct LocalBlockValueScores {
    /// Composite score, exposed for callers that filter/sort without
    /// re-deserializing the JSON envelope.
    #[serde(skip)]
    pub(super) composite_score: f64,
    #[serde(flatten)]
    snake: LocalBlockValueScoresSnake,
    #[serde(flatten)]
    camel: LocalBlockValueScoresCamel,
}

#[derive(Debug, Clone, Serialize)]
struct UnvaluatedWiredBlockSnake {
    document_id: String,
    block_id: String,
    cumulative_importance: f64,
    cumulative_valence: f64,
    raw_importance_sum: f64,
    raw_valence_sum: f64,
    importance_count: usize,
    valence_count: usize,
    composite_score: f64,
    block_wire_count: usize,
    doc_wire_count: usize,
    last_valuated_at: Option<String>,
    last_valuated: String,
    user_importance: Option<f64>,
    user_valence: Option<f64>,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnvaluatedWiredBlockCamel {
    document_id: String,
    block_id: String,
    cumulative_importance: f64,
    cumulative_valence: f64,
    raw_importance_sum: f64,
    raw_valence_sum: f64,
    importance_count: usize,
    valence_count: usize,
    composite_score: f64,
    block_wire_count: usize,
    doc_wire_count: usize,
    last_valuated_at: Option<String>,
    last_valuated: String,
    user_importance: Option<f64>,
    user_valence: Option<f64>,
}

/// Same dual-casing envelope as [`LocalBlockValueScores`] but for wire-tagged
/// blocks that haven't been valuated yet — counts and sums are zero.
#[derive(Debug, Clone, Serialize)]
pub(super) struct UnvaluatedWiredBlock {
    #[serde(skip)]
    pub(super) composite_score: f64,
    #[serde(flatten)]
    snake: UnvaluatedWiredBlockSnake,
    #[serde(flatten)]
    camel: UnvaluatedWiredBlockCamel,
}

fn local_block_value_json(
    record: &LocalBlockValueRecord,
    config: &LocalValueConfig,
    block_wire_count: usize,
    doc_wire_count: usize,
    doc_age_days: f64,
    wire_age_days: f64,
) -> LocalBlockValueScores {
    let combined_raw_importance = record.raw_importance_sum + record.user_importance.unwrap_or(0.0);
    let combined_raw_valence = record.raw_valence_sum + record.user_valence.unwrap_or(0.0);
    let combined_importance = if record.user_importance == Some(0.0) {
        0.0
    } else if combined_raw_importance > 0.0 {
        cumulative_importance(combined_raw_importance)
    } else {
        record.cumulative_importance
    };
    let combined_valence = if combined_raw_valence != 0.0 {
        cumulative_valence(combined_raw_valence)
    } else {
        record.cumulative_valence
    };
    let composite_score = local_value_composite_score(
        if record.importance_count == 0
            && combined_raw_importance == 0.0
            && record.user_importance.is_none()
        {
            crate::salience_value_scoring::DEFAULT_UNVALUATED_IMPORTANCE
        } else {
            combined_importance
        },
        combined_valence,
        record.user_importance,
        doc_age_days,
        block_wire_count,
        doc_wire_count,
        wire_age_days,
        config,
    );

    let last_valuated_at = if record.last_valuated_at.is_empty() {
        None
    } else {
        Some(record.last_valuated_at.clone())
    };

    let snake = LocalBlockValueScoresSnake {
        document_id: record.document_id.clone(),
        block_id: record.block_id.clone(),
        cumulative_importance: round4(record.cumulative_importance),
        cumulative_valence: round4(record.cumulative_valence),
        raw_importance_sum: round4(record.raw_importance_sum),
        raw_valence_sum: round4(record.raw_valence_sum),
        importance_count: record.importance_count,
        valence_count: record.valence_count,
        combined_importance: round4(combined_importance),
        combined_valence: round4(combined_valence),
        composite_score,
        valuation_count: record.valuation_count,
        last_valuated_at: last_valuated_at.clone(),
        last_valuated: record.last_valuated_at.clone(),
        user_importance: record.user_importance,
        user_valence: record.user_valence,
        tags: record.tags.clone(),
        block_wire_count,
        doc_wire_count,
    };
    let camel = LocalBlockValueScoresCamel {
        document_id: record.document_id.clone(),
        block_id: record.block_id.clone(),
        cumulative_importance: round4(record.cumulative_importance),
        cumulative_valence: round4(record.cumulative_valence),
        raw_importance_sum: round4(record.raw_importance_sum),
        raw_valence_sum: round4(record.raw_valence_sum),
        importance_count: record.importance_count,
        valence_count: record.valence_count,
        combined_importance: round4(combined_importance),
        combined_valence: round4(combined_valence),
        composite_score,
        valuation_count: record.valuation_count,
        last_valuated_at,
        last_valuated: record.last_valuated_at.clone(),
        user_importance: record.user_importance,
        user_valence: record.user_valence,
        block_wire_count,
        doc_wire_count,
    };

    LocalBlockValueScores {
        composite_score,
        snake,
        camel,
    }
}

pub(super) fn local_block_value_json_with_context(
    record: &LocalBlockValueRecord,
    config: &LocalValueConfig,
    wires: &[serde_json::Value],
    created_at: &crate::salience_wire_context::DocumentCreatedAt,
    now_ms: u128,
) -> LocalBlockValueScores {
    let (block_wire_count, doc_wire_count, newest_wire) =
        value_wire_stats(wires, &record.document_id, &record.block_id);
    let doc_age_days = document_created_age_days(
        created_at,
        &record.document_id,
        if record.last_valuated_at.is_empty() {
            None
        } else {
            Some(record.last_valuated_at.as_str())
        },
        now_ms,
    );
    let wire_age_days = value_timestamp_age_days(newest_wire.as_deref(), now_ms);
    local_block_value_json(
        record,
        config,
        block_wire_count,
        doc_wire_count,
        doc_age_days,
        wire_age_days,
    )
}

pub(super) fn local_unvaluated_wired_block_json(
    document_id: &str,
    block_id: &str,
    config: &LocalValueConfig,
    block_wire_count: usize,
    doc_wire_count: usize,
    doc_age_days: f64,
    wire_age_days: f64,
) -> UnvaluatedWiredBlock {
    let composite_score = local_value_composite_score(
        crate::salience_value_scoring::DEFAULT_UNVALUATED_IMPORTANCE,
        0.0,
        None,
        doc_age_days,
        block_wire_count,
        doc_wire_count,
        wire_age_days,
        config,
    );
    let snake = UnvaluatedWiredBlockSnake {
        document_id: document_id.to_string(),
        block_id: block_id.to_string(),
        cumulative_importance: 0.0,
        cumulative_valence: 0.0,
        raw_importance_sum: 0.0,
        raw_valence_sum: 0.0,
        importance_count: 0,
        valence_count: 0,
        composite_score,
        block_wire_count,
        doc_wire_count,
        last_valuated_at: None,
        last_valuated: String::new(),
        user_importance: None,
        user_valence: None,
        tags: Vec::new(),
    };
    let camel = UnvaluatedWiredBlockCamel {
        document_id: document_id.to_string(),
        block_id: block_id.to_string(),
        cumulative_importance: 0.0,
        cumulative_valence: 0.0,
        raw_importance_sum: 0.0,
        raw_valence_sum: 0.0,
        importance_count: 0,
        valence_count: 0,
        composite_score,
        block_wire_count,
        doc_wire_count,
        last_valuated_at: None,
        last_valuated: String::new(),
        user_importance: None,
        user_valence: None,
    };

    UnvaluatedWiredBlock {
        composite_score,
        snake,
        camel,
    }
}

#[cfg(test)]
mod unified_baseline_tests {
    use super::*;

    #[test]
    fn absent_and_explicit_zero_are_different_even_with_valence_only_history() {
        let config = crate::salience_value_config::default_value_config();
        let mut record = LocalBlockValueRecord::default();
        let baseline = local_block_value_json(&record, &config, 0, 0, 30.0, 9999.0).composite_score;
        record.valence_count = 1;
        assert_eq!(
            local_block_value_json(&record, &config, 0, 0, 30.0, 9999.0).composite_score,
            baseline
        );
        record.importance_count = 1;
        assert!(
            local_block_value_json(&record, &config, 0, 0, 30.0, 9999.0).composite_score < baseline
        );
        assert_eq!(
            local_unvaluated_wired_block_json("doc", "block", &config, 0, 0, 30.0, 9999.0)
                .composite_score,
            baseline
        );
    }
}
