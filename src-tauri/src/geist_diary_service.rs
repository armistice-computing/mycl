//! The two Diary tools and the lines that ride `recall` (see `geist_diary_store`).
//! The author is always the connection's credential: an `agent` argument is an
//! attribution label at most, never an authorization claim, exactly as `remember`
//! treats it. Owner sessions have no diary — the diary belongs to the agent type.
use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    document_chronicle::ChronicleActor,
    geist_diary_store::{
        diary_agent_segment, diary_doc_id, diary_document_blocks, diary_entry_json,
        diary_recall_projection, last_diary_entries, read_diary_store, validate_diary_text,
        write_diary_store, LocalDiaryEntry, LocalDiaryStore, DIARY_LABEL_MAX, DIARY_RECALL_LINES,
    },
    geist_projection_document::write_geist_projection_document,
    graph_service::touch_graph_updated_at,
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
};

fn agent_of(actor: &Option<ChronicleActor>) -> Option<String> {
    actor
        .as_ref()
        .filter(|actor| actor.kind == "agent")
        .map(|actor| actor.name.clone())
}

fn label(arguments: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    let Some(value) = arguments.get(key) else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| format!("{key} must be a string"))?
        .trim()
        .to_string();
    if text.chars().count() > DIARY_LABEL_MAX {
        return Err(format!("{key} is at most {DIARY_LABEL_MAX} characters"));
    }
    Ok(Some(text).filter(|text| !text.is_empty()))
}

fn project_diary(app: &AppHandle, graph_dir: &std::path::Path, store: &LocalDiaryStore) -> Result<(), String> {
    let document_id =
        crate::legacy_geist_import::projection_document_id(graph_dir, &diary_doc_id(&store.agent))?;
    write_geist_projection_document(
        app,
        graph_dir,
        &store.graph_id,
        &document_id,
        &format!("Diary — {}", store.agent),
        diary_document_blocks(store),
        vec![
            "document.local.read".to_string(),
            "document.local.materialize.rdf".to_string(),
            "narrative.local.diary".to_string(),
        ],
        None,
    )
}

pub(super) fn mcp_local_diary(
    app: AppHandle,
    arguments: &serde_json::Value,
    actor: Option<ChronicleActor>,
) -> Result<serde_json::Value, String> {
    let object = arguments.as_object().ok_or("diary arguments must be an object")?;
    for key in object.keys() {
        if !matches!(key.as_str(), "entry" | "hand" | "watch" | "anchors" | "graph_id" | "graphId" | "agent") {
            return Err(format!("diary does not accept '{key}'; the entry goes in 'entry'"));
        }
    }
    let agent = agent_of(&actor)
        .ok_or("the diary belongs to an agent credential: this connection has no agent identity")?;
    let agent = diary_agent_segment(&agent)?;
    // Required, but it may be empty: an explicit "" is the honest null.
    let entry = match arguments.get("entry") {
        Some(serde_json::Value::String(entry)) => entry.clone(),
        _ => return Err(
            "entry is required: up to four sentences, or an empty string when there is nothing for the next one".into(),
        ),
    };
    let text = validate_diary_text(&entry)?;
    let hand = label(arguments, "hand")?.unwrap_or_default();
    let watch = label(arguments, "watch")?;
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    // Validated against committed state BEFORE a number is allocated, as remember does.
    let anchors = crate::geist_memory_anchors::validate_anchors(&app, &graph_id, arguments.get("anchors"))?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let _guard = crate::legacy_geist_import::mutation_guard()?;
    let mut store = read_diary_store(&graph_dir, &graph_id, &agent)?;
    let number = store.next_number;
    let next = number.checked_add(1).ok_or("diary number space exhausted")?;
    let (clock, model) = actor
        .as_ref()
        .map(crate::agentic_clock::read_stamp_and_model)
        .unwrap_or_default();
    let entry = LocalDiaryEntry {
        number,
        entry_id: format!("diary-{agent}-{number}"),
        text,
        hand,
        watch,
        created_at: timestamp(),
        author: actor,
        anchors,
        clock,
        model,
    };
    store.entries.push(entry.clone());
    store.next_number = next;
    write_diary_store(&graph_dir, &store)?;
    // The entry is committed; a projection failure must not read as a failed
    // write and invite a duplicate line.
    let mut warnings = Vec::new();
    if let Err(error) = project_diary(&app, &graph_dir, &store) {
        warnings.push(error);
    }
    if let Err(error) = touch_graph_updated_at(&graph_dir) {
        warnings.push(error.to_string());
    }
    if let Err(error) = touch_profile_updated_at(&app) {
        warnings.push(error);
    }
    let mut result = diary_entry_json(&entry);
    result["ok"] = serde_json::json!(true);
    result["success"] = serde_json::json!(true);
    result["agent"] = serde_json::json!(agent);
    result["entries"] = serde_json::json!(store.entries.len());
    result["warnings"] = serde_json::json!(warnings);
    result["source"] = serde_json::json!("local-diary-store");
    Ok(result)
}

pub(super) fn mcp_local_read_diary(
    app: AppHandle,
    arguments: &serde_json::Value,
    actor: Option<ChronicleActor>,
) -> Result<serde_json::Value, String> {
    let object = arguments.as_object().ok_or("read_diary arguments must be an object")?;
    for key in object.keys() {
        if !matches!(key.as_str(), "agent" | "limit" | "allHands" | "graph_id" | "graphId") {
            return Err(format!("read_diary does not accept '{key}'"));
        }
    }
    let agent = label(arguments, "agent")?
        .or_else(|| agent_of(&actor))
        .ok_or("agent is required when the connection has no agent identity")?;
    let agent = diary_agent_segment(&agent)?;
    let limit = mcp_arg_usize(arguments, &["limit"], DIARY_RECALL_LINES).clamp(1, 50);
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let store = read_diary_store(&graph_dir, &graph_id, &agent)?;
    let entries = if arguments.get("allHands").and_then(serde_json::Value::as_bool) == Some(true) {
        diary_recall_projection(&store, limit)
    } else {
        last_diary_entries(&store, limit)
    };
    Ok(serde_json::json!({
        "agent": agent,
        "entries": entries,
        "count": entries.len(),
        "total": store.entries.len(),
        "source": "local-diary-store",
    }))
}

/// The caller's recent diary lines, for `recall` to carry; `None` when the
/// connection is not an agent, the diary is empty (silence is the common case),
/// or the caller asked for none. `limit` is recall's `diaryLimit`: 4 by default,
/// 2 for lean L2 attunement (her ruling 2026-09-19), 0 to leave it out. The
/// projection keeps every hand represented (see `diary_recall_projection`).
pub(crate) fn diary_recall_lines(
    graph_dir: &std::path::Path,
    graph_id: &str,
    actor: &Option<ChronicleActor>,
    limit: usize,
    window_tokens: Option<u64>,
) -> Option<serde_json::Value> {
    if limit == 0 {
        return None;
    }
    let agent = diary_agent_segment(&agent_of(actor)?).ok()?;
    let store = read_diary_store(graph_dir, graph_id, &agent).ok()?;
    // A windowed read (Mu) always says what window it used, even before her
    // first entry; the plain read stays silent when there is nothing.
    if store.entries.is_empty() && window_tokens.is_none() {
        return None;
    }
    let limit = limit.min(50);
    let Some(window_tokens) = window_tokens else {
        return Some(serde_json::json!({
            "agent": agent,
            "total": store.entries.len(),
            "entries": diary_recall_projection(&store, limit),
        }));
    };
    let house = crate::agentic_clock::read_house_clock();
    let (entries, window) =
        crate::geist_diary_store::diary_window_projection(&store, limit, window_tokens, house.as_ref());
    Some(serde_json::json!({
        "agent": agent,
        "total": store.entries.len(),
        "entries": entries,
        "window": window,
    }))
}
