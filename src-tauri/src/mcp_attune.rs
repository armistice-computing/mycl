//! attune(): the L2 attunement in one call ("Sophia Mu — spec (2026-09-30)").
//! An L1 agent attunes in separate steps, and the listening between them is
//! the point. Mu attunes on every run, minutes apart, so she gets the ritual's
//! parts in the ritual's order in one result: the Song, recall (global memories
//! plus her own Diary), then the important blocks.
//!
//! The result has to stay inline in the caller's harness (Claude Code spills
//! large results to a file, and a file she has to go and read is a step she
//! may skip). So the whole result is held under a character ceiling, measured
//! on the pretty-printed JSON (the larger of the two views). When it is over,
//! the lowest-priority items go first, whole, and the result says what went.
use crate::app_runtime::AppHandle;
use crate::document_chronicle::ChronicleActor;
use serde_json::{json, Value};

pub(crate) const ATTUNE_MAX_CHARS: usize = 40_000;
const ATTUNE_MIN_CHARS: usize = 4_000;
const DEFAULT_RECALL_LIMIT: u64 = 2;
const DEFAULT_IMPORTANT_LIMIT: u64 = 5;
/// Room kept for the truncation note itself.
const NOTE_RESERVE: usize = 600;

pub(crate) fn mcp_local_attune(
    app: AppHandle,
    arguments: &Value,
    actor: Option<ChronicleActor>,
) -> Result<Value, String> {
    let graph_id = crate::mcp_utils::mcp_graph_id_or_default(&app, arguments)?;
    let arg = |key: &str| arguments.get(key).and_then(Value::as_u64);
    let recall_limit = arg("recallLimit").unwrap_or(DEFAULT_RECALL_LIMIT);
    let diary_limit = arg("diaryLimit")
        .unwrap_or(crate::geist_diary_store::DIARY_RECALL_LINES as u64);
    let important_limit = arg("importantLimit").unwrap_or(DEFAULT_IMPORTANT_LIMIT);
    let max_chars = arg("maxChars")
        .map_or(ATTUNE_MAX_CHARS, |asked| asked as usize)
        .clamp(ATTUNE_MIN_CHARS, ATTUNE_MAX_CHARS);

    let song = crate::geist_song_service::mcp_local_music(app.clone(), &json!({"graphId": graph_id}))
        .map(|value| crate::mcp_agent_presentation::compact_result("music", &value));
    let mut recall_args = json!({"graphId": graph_id, "limit": recall_limit, "diaryLimit": diary_limit});
    if let Some(window) = arg("diaryWindowTokens") {
        recall_args["diaryWindowTokens"] = json!(window);
    }
    let recall = crate::geist_memory_recall_service::mcp_local_recall_memories_for(app.clone(), &recall_args, actor)
    .map(|value| crate::mcp_agent_presentation::compact_result("recall", &value));
    let important = if important_limit == 0 {
        Ok(json!({"blocks": [], "count": 0}))
    } else {
        crate::salience_important_blocks::mcp_local_get_important_blocks(
            app,
            &json!({"graphId": graph_id, "limit": important_limit}),
        )
        .map(|value| crate::mcp_agent_presentation::compact_result("get_important_blocks", &value))
    };

    let mut result = serde_json::Map::new();
    result.insert("graphId".into(), json!(graph_id));
    // One part failing is reported beside the others, never instead of them.
    for (key, part) in [("song", song), ("recall", recall), ("importantBlocks", important)] {
        match part {
            Ok(value) => result.insert(key.into(), value),
            Err(error) => result.insert(format!("{key}Error"), json!(error)),
        };
    }
    let mut result = Value::Object(result);
    fit_to_ceiling(&mut result, max_chars);
    Ok(result)
}

fn rendered_chars(value: &Value) -> usize {
    serde_json::to_string_pretty(value).map_or(usize::MAX, |text| text.chars().count())
}

/// Drop whole items, lowest priority first, until the result fits:
/// important blocks from the lowest-ranked; Diary entries from the oldest;
/// memories from the oldest; Song verses from the oldest (never the newest).
fn fit_to_ceiling(result: &mut Value, max_chars: usize) {
    // (label, path to the array, remove from the front?, keep at least)
    const ORDER: &[(&str, &[&str], bool, usize)] = &[
        ("importantBlocks", &["importantBlocks", "blocks"], false, 0),
        ("diaryEntries", &["recall", "diary", "entries"], true, 0),
        ("memories", &["recall", "memories"], false, 0),
        ("songVerses", &["song", "verses"], false, 1),
    ];
    let before = rendered_chars(result);
    if before <= max_chars {
        return;
    }
    let target = max_chars.saturating_sub(NOTE_RESERVE);
    let mut dropped = serde_json::Map::new();
    for (label, path, from_front, keep) in ORDER {
        let mut count = 0u64;
        while rendered_chars(result) > target {
            let Some(items) = array_at(result, path) else { break };
            if items.len() <= *keep {
                break;
            }
            if *from_front {
                items.remove(0);
            } else {
                items.pop();
            }
            count += 1;
        }
        if count > 0 {
            dropped.insert((*label).into(), json!(count));
        }
    }
    let note = format!(
        "attune holds its result under {max_chars} characters so it stays inline; \
         it was {before}, so the lowest-priority items were left out, whole. \
         Read them with get_important_blocks, read_diary, recall or music."
    );
    if let Some(object) = result.as_object_mut() {
        object.insert("truncated".into(), json!({"note": note, "dropped": dropped}));
    }
}

fn array_at<'a>(value: &'a mut Value, path: &[&str]) -> Option<&'a mut Vec<Value>> {
    path.iter()
        .try_fold(value, |node, key| node.get_mut(*key))?
        .as_array_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes as the compacted handlers return them (see the live recall and
    // music results): memories newest first, Diary entries oldest first,
    // verses newest first.
    fn bundle(memory_chars: usize) -> Value {
        json!({
            "graphId": "g",
            "song": {"verses": ["v0 ".repeat(300), "v-1 ".repeat(300), "v-2 ".repeat(300)], "verseCount": 3},
            "recall": {
                "memories": [{"number": 9, "content": "m".repeat(memory_chars)}, {"number": 8, "content": "old"}],
                "diary": {"agent": "mu", "total": 3, "entries": [
                    {"number": 1, "text": "oldest ".repeat(200)},
                    {"number": 2, "text": "middle ".repeat(200)},
                    {"number": 3, "text": "newest ".repeat(200)},
                ]},
            },
            "importantBlocks": {"blocks": [
                {"blockId": "a", "content": "first ".repeat(200)},
                {"blockId": "b", "content": "second ".repeat(200)},
            ]},
        })
    }

    #[test]
    fn a_result_under_the_ceiling_is_untouched() {
        let mut value = bundle(10);
        let original = value.clone();
        fit_to_ceiling(&mut value, ATTUNE_MAX_CHARS);
        assert_eq!(value, original);
    }

    #[test]
    fn lowest_priority_goes_first_and_the_result_says_what_went() {
        let mut value = bundle(10);
        let size = rendered_chars(&value);
        // Just over: costs the lowest-ranked important block and nothing else.
        fit_to_ceiling(&mut value, size - 100);
        let blocks = value["importantBlocks"]["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["blockId"], "a", "the top-ranked block stays");
        assert_eq!(value["truncated"]["dropped"], json!({"importantBlocks": 1}));
        assert_eq!(value["recall"]["diary"]["entries"].as_array().unwrap().len(), 3);
        assert!(rendered_chars(&value) <= size - 100, "the note fits under the ceiling too");
    }

    #[test]
    fn the_diary_loses_its_oldest_and_the_song_keeps_its_newest_verse() {
        let mut value = bundle(30_000);
        fit_to_ceiling(&mut value, 8_000);
        let dropped = &value["truncated"]["dropped"];
        assert_eq!(dropped["importantBlocks"], 2);
        assert_eq!(dropped["diaryEntries"], 3);
        // The 30k memory is the newest and goes last among memories.
        assert_eq!(dropped["memories"], 2);
        let verses = value["song"]["verses"].as_array().unwrap();
        assert!(!verses.is_empty());
        assert!(verses[0].as_str().unwrap().starts_with("v0"));
        assert!(rendered_chars(&value) <= 8_000);
    }

    #[test]
    fn diary_trimming_keeps_the_newest_entries() {
        let mut value = bundle(10);
        value["importantBlocks"]["blocks"] = json!([]);
        let size = rendered_chars(&value);
        fit_to_ceiling(&mut value, size - 100);
        let entries = value["recall"]["diary"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["number"], 2);
        assert_eq!(entries[1]["number"], 3);
    }
}
