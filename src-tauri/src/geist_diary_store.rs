//! The Diary — Geist 2.1, level L2 (design of record: graph doc `geist-2-1-spec`,
//! "L2 — Diary"; built for gardend by Beta, 2026-09-19, the day Beta first ran
//! as two hands at once).
//!
//! An agent's own short journal: dated, first-person, past-tense prose, at most
//! four sentences, written by the agent's own hand and addressed to the next
//! hand of the same agent. It is written at the close of a watch, the one
//! boundary every agent unambiguously has. "Nothing for the next hand." written
//! as a sentence is a first-class honest entry, never a failure; silence is the
//! expected common case and nothing here conscripts a line at every close.
//!
//! Entries from concurrent hands of one agent interleave in ONE store: the
//! interleaving is how hands discover they are a choir. Storage is
//! `<graph>/diary/<agent>.json`, append-only, projected to the read-only
//! document `geist-diary-agent-<agent>` so the diary is a window in the UI.
//! Every diary in the house is legible to the house; fully private variants are
//! a later experiment (subgraph permissions), not this slice. RDF projection of
//! entries is also deferred: this slice is the store, the two tools, the
//! projection document, and the last four lines riding `recall`.
use crate::storage::{create_dir_all, read_json, write_json};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DIARY_STORE_SCHEMA_VERSION: u32 = 1;
pub(crate) const DIARY_MAX_SENTENCES: usize = 4;
pub(crate) const DIARY_MAX_CHARS: usize = 1200;
pub(crate) const DIARY_RECALL_LINES: usize = 4;
pub(crate) const DIARY_LABEL_MAX: usize = 64;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDiaryStore {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    pub(crate) agent: String,
    #[serde(default = "default_next_number")]
    pub(crate) next_number: u64,
    #[serde(default)]
    pub(crate) entries: Vec<LocalDiaryEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalDiaryEntry {
    pub(crate) number: u64,
    pub(crate) entry_id: String,
    pub(crate) text: String,
    /// Which body of the agent wrote this ("terminal", "watch", …) when the
    /// agent runs as more than one hand. Empty when the agent did not say.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) hand: String,
    /// An optional watch reference (e.g. `0x10`), the agent's own word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) watch: Option<String>,
    pub(crate) created_at: String,
    /// The credential that wrote the entry, as the Chronicle records actors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) author: Option<crate::document_chronicle::ChronicleActor>,
    /// Deliberate same-graph references, exactly as `remember` anchors: record
    /// refs, never wires, no effect on salience (Eschaton's ruling 2026-09-19).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) anchors: Vec<crate::geist_memory_anchors::MemoryAnchor>,
    /// Agentic time at the write, as the Cantor last witnessed it (the
    /// Chronicle's `createdClock` for a Diary line). Absent when the Cantor
    /// was down; never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) clock: Option<crate::document_chronicle::AgenticTimeStamp>,
    /// The model the writer was running on, as the Cantor saw it. Absent
    /// when the Cantor did not say; never self-reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
}

fn default_next_number() -> u64 {
    1
}

/// The diary's agent segment: lowercase letters, digits, `_` and `-`, one path
/// segment and never a path — the same rule the Ear applies to agent names.
pub(crate) fn diary_agent_segment(agent: &str) -> Result<String, String> {
    let agent = agent.trim().to_ascii_lowercase();
    let plain = agent
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if agent.is_empty() || agent.len() > DIARY_LABEL_MAX || !plain {
        return Err(format!(
            "diary agent name must be one plain segment of letters, digits, '_' or '-' (got {agent:?})"
        ));
    }
    Ok(agent)
}

fn diary_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("diary")
}

fn diary_store_path(graph_dir: &Path, agent: &str) -> PathBuf {
    diary_dir(graph_dir).join(format!("{agent}.json"))
}

/// The read-only projection document id for an agent's diary (colon-free: the
/// document-id validator rejects `:`).
pub(crate) fn diary_doc_id(agent: &str) -> String {
    format!("geist-diary-agent-{agent}")
}

fn default_diary_store(graph_id: &str, agent: &str) -> LocalDiaryStore {
    LocalDiaryStore {
        schema_version: DIARY_STORE_SCHEMA_VERSION,
        graph_id: graph_id.to_string(),
        agent: agent.to_string(),
        next_number: default_next_number(),
        entries: Vec::new(),
    }
}

/// Read one agent's diary; a missing file is an empty diary, not an error.
pub(crate) fn read_diary_store(
    graph_dir: &Path,
    graph_id: &str,
    agent: &str,
) -> Result<LocalDiaryStore, String> {
    let agent = diary_agent_segment(agent)?;
    let path = diary_store_path(graph_dir, &agent);
    if !path.is_file() {
        return Ok(default_diary_store(graph_id, &agent));
    }
    let mut store = read_json::<LocalDiaryStore>(&path)?;
    store.schema_version = DIARY_STORE_SCHEMA_VERSION;
    store.graph_id = graph_id.to_string();
    store.agent = agent;
    let highest = store.entries.iter().map(|entry| entry.number).max().unwrap_or(0);
    if store.next_number <= highest {
        store.next_number = highest.checked_add(1).ok_or("diary number space exhausted")?;
    }
    Ok(store)
}

pub(crate) fn write_diary_store(graph_dir: &Path, store: &LocalDiaryStore) -> Result<(), String> {
    create_dir_all(&diary_dir(graph_dir))?;
    write_json(&diary_store_path(graph_dir, &store.agent), store).map_err(Into::into)
}

fn is_terminator(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

fn is_closer(c: char) -> bool {
    is_terminator(c) || matches!(c, '"' | '\'' | ')' | ']' | '”' | '’')
}

/// Sentences end at `.`, `!`, `?` or `…` (a run of them, with closing quotes
/// or brackets) followed by whitespace or the end of the text. "3.5" is not a
/// boundary; "e.g. this" is, which errs on the side of the cap.
pub(crate) fn count_sentences(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut count = 0;
    let mut in_sentence = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_terminator(c) {
            let mut j = i;
            while j < chars.len() {
                if !is_closer(chars[j]) {
                    break;
                }
                j += 1;
            }
            let ends = j >= chars.len() || chars[j].is_whitespace();
            if ends {
                if in_sentence {
                    count += 1;
                }
                in_sentence = false;
            } else {
                in_sentence = true;
            }
            i = j;
            continue;
        }
        if !c.is_whitespace() {
            in_sentence = true;
        }
        i += 1;
    }
    if in_sentence {
        count += 1;
    }
    count
}

/// Whitespace runs collapse (the diary is prose, not a list); empty, over-long
/// and over-four-sentence entries are refused before anything is written.
/// An empty entry is valid: the Diary line is mandatory at the close of a
/// watch, and "nothing for the next one" is an honest answer (Eschaton,
/// 2026-09-30, for Mu; it holds for every agent). It is stored as empty text
/// and shown as an empty entry, never as invented words.
pub(crate) fn validate_diary_text(entry: &str) -> Result<String, String> {
    let text = entry.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Ok(text);
    }
    let chars = text.chars().count();
    if chars > DIARY_MAX_CHARS {
        return Err(format!("a diary entry is at most {DIARY_MAX_CHARS} characters (got {chars})"));
    }
    let sentences = count_sentences(&text);
    if sentences > DIARY_MAX_SENTENCES {
        return Err(format!(
            "a diary entry is at most {DIARY_MAX_SENTENCES} sentences (got {sentences}); the cap is the point — constraint forces essence"
        ));
    }
    Ok(text)
}

/// The entry's clock rendered for a reader: RFC 3339 in UTC to the second.
pub(crate) fn diary_entry_when(created_at: &str) -> String {
    created_at
        .parse::<i64>()
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|when| when.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| created_at.to_string())
}

pub(crate) fn diary_entry_json(entry: &LocalDiaryEntry) -> serde_json::Value {
    let mut value = serde_json::json!({
        "number": entry.number,
        "entryId": entry.entry_id,
        "text": entry.text,
        "createdAt": entry.created_at,
        "when": diary_entry_when(&entry.created_at),
    });
    if !entry.hand.is_empty() {
        value["hand"] = serde_json::json!(entry.hand);
    }
    if let Some(watch) = &entry.watch {
        value["watch"] = serde_json::json!(watch);
    }
    if let Some(author) = &entry.author {
        value["author"] = serde_json::json!(author);
    }
    if !entry.anchors.is_empty() {
        value["anchors"] = serde_json::json!(entry.anchors);
    }
    if let Some(clock) = &entry.clock {
        value["clock"] = clock.to_json();
    }
    if let Some(model) = &entry.model {
        value["model"] = serde_json::json!(model);
    }
    value
}

/// The last `limit` entries, oldest to newest within the page.
pub(crate) fn last_diary_entries(store: &LocalDiaryStore, limit: usize) -> Vec<serde_json::Value> {
    let skip = store.entries.len().saturating_sub(limit);
    store.entries.iter().skip(skip).map(diary_entry_json).collect()
}

/// The attunement projection for an agent with more than one hand: the last
/// `limit` entries PLUS the newest entry of every hand not represented among
/// them, so a busy hand (a secretary waking hourly) cannot displace a slow
/// consequential one from what the next hand reads (Geist 2.1 consolidated §3;
/// her ruling 2026-09-19: build the diversity projection now). Oldest to
/// newest. Entries with no hand count as one hand. Massively parallel agents
/// will need a different rule; there are none yet.
pub(crate) fn diary_recall_projection(store: &LocalDiaryStore, limit: usize) -> Vec<serde_json::Value> {
    let n = store.entries.len();
    let start = n.saturating_sub(limit);
    let mut chosen: Vec<usize> = (start..n).collect();
    let mut seen: std::collections::BTreeSet<&str> =
        chosen.iter().map(|&i| store.entries[i].hand.as_str()).collect();
    for i in (0..start).rev() {
        let hand = store.entries[i].hand.as_str();
        if seen.insert(hand) {
            chosen.push(i);
        }
    }
    chosen.sort_unstable();
    chosen.into_iter().map(|i| diary_entry_json(&store.entries[i])).collect()
}

/// The fewest entries a windowed read returns, whatever the window holds.
pub(crate) const DIARY_WINDOW_FLOOR: usize = 2;

/// The Diary read in agentic time ("Sophia Mu — spec": a count is
/// tenseless): the newest entries written within the last `window_tokens`
/// house tokens, at most `limit`, at least the floor. Entries are in write
/// order, so the window is a suffix; an entry with no stamp, or one whose
/// distance can't be known, ends it. Without a live clock nothing can be
/// measured and the floor is what's returned. Recency replaces the
/// every-hand rule here: for Mu a hand is a run that lives minutes.
pub(crate) fn diary_window_projection(
    store: &LocalDiaryStore,
    limit: usize,
    window_tokens: u64,
    house: Option<&crate::agentic_clock::HouseClock>,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let in_window = house.map(|house| {
        store
            .entries
            .iter()
            .rev()
            .take_while(|entry| {
                entry
                    .clock
                    .as_ref()
                    .and_then(|stamp| house.tokens_since(stamp))
                    .is_some_and(|since| since <= window_tokens)
            })
            .count()
    });
    let floor = DIARY_WINDOW_FLOOR.min(limit);
    let n = in_window.unwrap_or(0).clamp(floor, limit).min(store.entries.len());
    let window = serde_json::json!({
        "tokens": window_tokens,
        "measured": in_window.is_some(),
        "inWindow": in_window,
    });
    (last_diary_entries(store, n), window)
}

/// The diary as a read-only document: newest entry first, each under a heading
/// that names when and by which hand it was written.
pub(crate) fn diary_document_blocks(
    store: &LocalDiaryStore,
) -> Vec<crate::document_service::BlockSnapshot> {
    use crate::document_service::BlockSnapshot;
    let mut blocks = Vec::new();
    let mut order = 0.0;
    let mut push = |id: String, block_type: &str, content: String, level: Option<i64>| {
        blocks.push(BlockSnapshot {
            id,
            block_type: block_type.to_string(),
            content,
            parent_id: None,
            order,
            level,
            checked: None,
            language: None,
            marks: Vec::new(),
        });
        order += 1.0;
    };
    push("diary-title".to_string(), "heading", format!("Diary — {}", store.agent), Some(1));
    push(
        "diary-about".to_string(),
        "paragraph",
        format!(
            "Written by {}'s own hand at the close of a watch, addressed to the next hand. At most four sentences an entry; \"Nothing for the next hand.\" is an honest entry. Read with read_diary; the last {} ride recall.",
            store.agent, DIARY_RECALL_LINES
        ),
        None,
    );
    for entry in store.entries.iter().rev() {
        let mut label = format!("{} · #{}", diary_entry_when(&entry.created_at), entry.number);
        if !entry.hand.is_empty() {
            label.push_str(&format!(" · {}", entry.hand));
        }
        if let Some(watch) = &entry.watch {
            label.push_str(&format!(" · watch {watch}"));
        }
        push(format!("diary-{}-head", entry.number), "heading", label, Some(2));
        let text = if entry.text.is_empty() {
            "(empty entry)".to_string()
        } else {
            entry.text.clone()
        };
        push(format!("diary-{}", entry.number), "paragraph", text, None);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_count_respects_decimals_runs_and_closing_quotes() {
        assert_eq!(count_sentences("Nothing for the next hand."), 1);
        assert_eq!(count_sentences("I merged it. It held."), 2);
        assert_eq!(count_sentences("The build took 3.5 minutes and passed"), 1);
        assert_eq!(count_sentences("Really?! Yes... \"Done.\" (Finally.)"), 4);
        assert_eq!(count_sentences("One. Two. Three. Four. Five."), 5);
        assert_eq!(count_sentences("   "), 0);
    }

    #[test]
    fn validation_collapses_whitespace_and_refuses_empty_long_and_five_sentences() {
        assert_eq!(validate_diary_text("  I  wrote\n\nit. ").unwrap(), "I wrote it.");
        assert_eq!(validate_diary_text("  \n ").unwrap(), "", "an explicit empty entry is valid");
        assert!(validate_diary_text("One. Two. Three. Four.").is_ok());
        let err = validate_diary_text("One. Two. Three. Four. Five.").unwrap_err();
        assert!(err.contains("at most 4 sentences"), "{err}");
        assert!(validate_diary_text(&"x".repeat(DIARY_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn agent_segment_is_one_plain_lowercase_segment() {
        assert_eq!(diary_agent_segment(" Beta ").unwrap(), "beta");
        assert_eq!(diary_agent_segment("beta-watch").unwrap(), "beta-watch");
        assert!(diary_agent_segment("../beta").is_err());
        assert!(diary_agent_segment("agent:beta").is_err());
        assert!(diary_agent_segment("").is_err());
    }

    #[test]
    fn store_round_trips_backfills_numbers_and_pages_from_the_end() {
        let graph_dir = std::env::temp_dir().join(format!("sophia-diary-store-{}", uuid::Uuid::new_v4()));
        let empty = read_diary_store(&graph_dir, "g", "beta").unwrap();
        assert!(empty.entries.is_empty());
        assert_eq!(empty.next_number, 1);
        let mut store = empty;
        for (n, hand) in [(1, "terminal"), (2, "watch"), (3, "terminal")] {
            store.entries.push(LocalDiaryEntry {
                number: n,
                entry_id: format!("diary-beta-{n}"),
                text: format!("Entry {n}."),
                hand: hand.to_string(),
                watch: None,
                created_at: (1_789_830_000_000u64 + n).to_string(),
                author: None,
                anchors: Vec::new(),
                clock: None,
                model: None,
            });
        }
        store.next_number = 1; // stale on purpose: the read backfills it
        write_diary_store(&graph_dir, &store).unwrap();
        let read = read_diary_store(&graph_dir, "g2", "BETA").unwrap();
        assert_eq!(read.graph_id, "g2");
        assert_eq!(read.agent, "beta");
        assert_eq!(read.next_number, 4);
        assert_eq!(read.entries, store.entries);
        let page = last_diary_entries(&read, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0]["number"], 2);
        assert_eq!(page[1]["hand"], "terminal");
        assert!(page[1]["when"].as_str().unwrap().starts_with("2026-"));
        let blocks = diary_document_blocks(&read);
        assert_eq!(blocks[0].content, "Diary — beta");
        assert!(blocks[2].content.contains("#3 · terminal"), "{}", blocks[2].content);
        assert_eq!(blocks[3].content, "Entry 3.");
        std::fs::remove_dir_all(graph_dir).unwrap();
    }

    fn stamped(n: u64, day: &str, tokens: u64) -> LocalDiaryEntry {
        let mut entry = entry(n, "");
        entry.clock = Some(crate::document_chronicle::AgenticTimeStamp {
            day: day.into(),
            stanza_tokens: tokens,
            watch: None,
            measure: None,
            witnessed_at: String::new(),
            evidence: "inferred".into(),
        });
        entry
    }

    #[test]
    fn the_window_reads_in_house_tokens_with_a_floor_and_a_cap() {
        let mut store = default_diary_store("g", "mu");
        store.entries = vec![
            entry(1, ""), // written before stamps existed
            stamped(2, "2026-09-29", 2_500_000),
            stamped(3, "2026-09-30", 5_000_000),
            stamped(4, "2026-09-30", 5_600_000),
            stamped(5, "2026-09-30", 5_900_000),
        ];
        let house = crate::agentic_clock::HouseClock {
            day: "2026-09-30".into(),
            stanza_tokens: 6_000_000,
            days: [("2026-09-29".to_string(), 2_896_000), ("2026-09-30".to_string(), 6_000_000)].into(),
        };
        let numbers = |entries: Vec<serde_json::Value>| entries.iter().map(|e| e["number"].as_u64().unwrap()).collect::<Vec<_>>();
        // 750k back from 6.0M reaches 5.25M: entries 4 and 5.
        let (entries, window) = diary_window_projection(&store, 8, 750_000, Some(&house));
        assert_eq!(numbers(entries), [4, 5]);
        assert_eq!(window["inWindow"], 2);
        assert_eq!(window["measured"], true);
        // A wider window crosses midnight: 2's remainder (396k) + 6.0M.
        let (entries, _) = diary_window_projection(&store, 8, 6_400_000, Some(&house));
        assert_eq!(numbers(entries), [2, 3, 4, 5], "the unstamped entry ends the window");
        // The cap holds.
        let (entries, _) = diary_window_projection(&store, 3, 6_400_000, Some(&house));
        assert_eq!(numbers(entries), [3, 4, 5]);
        // A narrow window still returns the floor.
        let (entries, window) = diary_window_projection(&store, 8, 10_000, Some(&house));
        assert_eq!(numbers(entries), [4, 5]);
        assert_eq!(window["inWindow"], 0);
        // No live clock: the floor, and the result says it couldn't measure.
        let (entries, window) = diary_window_projection(&store, 8, 750_000, None);
        assert_eq!(numbers(entries), [4, 5]);
        assert_eq!(window["measured"], false);
        // A limit below the floor wins.
        let (entries, _) = diary_window_projection(&store, 1, 10_000, None);
        assert_eq!(numbers(entries), [5]);
    }

    #[test]
    fn stamp_and_model_round_trip_and_show() {
        let mut entry = stamped(7, "2026-09-30", 6_103_341);
        entry.model = Some("claude-haiku-4-5".into());
        let stored = serde_json::to_value(&entry).unwrap();
        let back: LocalDiaryEntry = serde_json::from_value(stored).unwrap();
        assert_eq!(back, entry);
        let shown = diary_entry_json(&entry);
        assert_eq!(shown["clock"]["stanza"], serde_json::json!(6.103));
        assert_eq!(shown["model"], "claude-haiku-4-5");
        let bare = diary_entry_json(&super::tests::entry(8, ""));
        assert!(bare.get("clock").is_none());
        assert!(bare.get("model").is_none());
    }

    fn entry(n: u64, hand: &str) -> LocalDiaryEntry {
        LocalDiaryEntry {
            number: n,
            entry_id: format!("diary-beta-{n}"),
            text: format!("Entry {n}."),
            hand: hand.to_string(),
            watch: None,
            created_at: (1_789_830_000_000u64 + n).to_string(),
            author: None,
            anchors: Vec::new(),
            clock: None,
            model: None,
        }
    }

    #[test]
    fn recall_projection_keeps_every_hand_represented() {
        let mut store = read_diary_store(&std::env::temp_dir().join("never-written"), "g", "beta").unwrap();
        // one terminal line, then a secretary writing hourly, then an unnamed hand once
        store.entries.push(entry(1, ""));
        store.entries.push(entry(2, "terminal"));
        for n in 3..=9 {
            store.entries.push(entry(n, "watch"));
        }
        let plain = last_diary_entries(&store, 4);
        assert_eq!(plain.iter().map(|e| e["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![6, 7, 8, 9]);
        let diverse = diary_recall_projection(&store, 4);
        let numbers = diverse.iter().map(|e| e["number"].as_u64().unwrap()).collect::<Vec<_>>();
        assert_eq!(numbers, vec![1, 2, 6, 7, 8, 9], "the terminal hand's newest line and the unnamed hand's ride along");
        assert_eq!(diary_recall_projection(&store, 2).len(), 4);
        // every hand already inside the window: nothing is added
        store.entries.push(entry(10, "terminal"));
        store.entries.push(entry(11, ""));
        let numbers = diary_recall_projection(&store, 4).iter().map(|e| e["number"].as_u64().unwrap()).collect::<Vec<_>>();
        assert_eq!(numbers, vec![8, 9, 10, 11]);
        assert!(diary_recall_projection(&LocalDiaryStore { entries: Vec::new(), ..store.clone() }, 4).is_empty());
    }
}
