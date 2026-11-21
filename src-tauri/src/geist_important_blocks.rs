//! The importance projection: a read-only Geist view of the blocks the house
//! holds most important, with the parts of each score and how value works.
//!
//! Each entry quotes a block and links to it with a wikilink mark that names
//! the source document and block. The blocks here are copies, so a value given
//! from this view must go to the source; the frontend reads the link's target
//! for that. Wikilinks are RDF link nodes, never wires, so the view adds no
//! backlinks or wire counts to the sources.
//!
//! The view reads the shared commons value store: everyone's valuations,
//! Eschaton's and every agent's (no caller passes an observer today).
use crate::app_runtime::AppHandle;
use crate::{
    document_service::{BlockSnapshot, InlineMarkSnapshot},
    geist_projection_document::write_geist_projection_document,
    paths::existing_graph_dir,
    salience_score_projection::{find_projected_block, local_block_value_scores_with_created_at},
    salience_value_config::{normalize_value_config, value_weight, LocalValueConfig},
    salience_value_store::read_value_store,
    salience_wire_context::{value_timestamp_age_days, DocumentCreatedAt},
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(feature = "desktop")]
use tauri::Manager;
use std::time::{Duration, Instant};

pub(crate) const IMPORTANT_BLOCKS_DOC_ID: &str = "geist-important-blocks";
pub(crate) const IMPORTANT_BLOCKS_TITLE: &str = "What Has Become Important";
pub(crate) const IMPORTANT_BLOCKS_CAPABILITY: &str = "salience.local.importance";
const ENTRY_LIMIT: usize = 30;
/// A quoted block longer than this is cut at a word boundary.
const QUOTE_MAX_CHARS: usize = 700;
/// Valuations arrive in bursts. A rebuild waits this long for the burst to end,
/// and never runs more than once per `REBUILD_INTERVAL`.
const REBUILD_SETTLE: Duration = Duration::from_secs(5);
const REBUILD_INTERVAL: Duration = Duration::from_secs(60);

/// One block in the view, with the parts of its score.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ImportantEntry {
    pub(crate) document_id: String,
    pub(crate) block_id: String,
    pub(crate) document_title: String,
    pub(crate) text: String,
    /// The composite score, the same number get_important_blocks reports.
    pub(crate) score: f64,
    pub(crate) importance: f64,
    pub(crate) valence: f64,
    pub(crate) block_wires: u64,
    pub(crate) doc_wires: u64,
    /// Days since the block was last valued; None for a block nobody valued.
    pub(crate) last_valued_days: Option<f64>,
}

fn json_f64(value: &serde_json::Value, key: &str) -> f64 {
    value.get(key).and_then(serde_json::Value::as_f64).unwrap_or(0.0)
}

fn json_u64(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str).filter(|s| !s.trim().is_empty())
}

/// The documents whose creation date the score needs.
///
/// A block's recency term uses its last valuation. Without one (a wired block
/// nobody valued, or a value record with no date) it falls back to the block's
/// document creation date, read from the document record. Every other block
/// scores without document records. The set may be larger than needed (it
/// keeps inactive wires); it must never be smaller, or the view would rank
/// differently from get_important_blocks.
fn documents_needing_creation_date(graph_dir: &std::path::Path, graph_id: &str) -> Result<BTreeSet<String>, String> {
    let store = read_value_store(graph_dir, graph_id)?;
    let workspace = crate::workspace_record_store::read_workspace_record(graph_dir, graph_id)?;
    let wires = crate::workspace_entity_projection::workspace_wires(workspace.snapshot.as_ref());
    let dated: BTreeSet<(String, String)> = store
        .blocks
        .values()
        .filter(|record| !record.last_valuated_at.trim().is_empty())
        .map(|record| (record.document_id.clone(), record.block_id.clone()))
        .collect();
    let mut needed: BTreeSet<String> = store
        .blocks
        .values()
        .filter(|record| record.last_valuated_at.trim().is_empty())
        .map(|record| record.document_id.clone())
        .collect();
    for wire in &wires {
        for (block_key, document_key) in [("sourceBlockId", "sourceDocumentId"), ("targetBlockId", "targetDocumentId")] {
            let (Some(block_id), Some(document_id)) = (json_str(wire, block_key), json_str(wire, document_key)) else {
                continue;
            };
            if !dated.contains(&(document_id.to_string(), block_id.to_string())) {
                needed.insert(document_id.to_string());
            }
        }
    }
    Ok(needed)
}

/// Every document's creation date from the RDF projection: one indexed
/// query instead of parsing each document record. The materializer writes the
/// record's created_at verbatim as mnemosyne:createdAt on the document's
/// projection graph (rdf_record_materializer.rs asserts it).
pub(crate) fn rdf_created_at(graph_dir: &std::path::Path, graph_id: &str) -> Result<DocumentCreatedAt, String> {
    let ns = crate::runtime_config::MNEMO_NS;
    let prefix = format!("{}:projection:document:", crate::rdf::graph_subject(graph_id));
    let query = format!(
        "SELECT ?id ?created WHERE {{ GRAPH ?g {{ ?d <{ns}documentId> ?id ; <{ns}createdAt> ?created }} \
         FILTER(STRSTARTS(STR(?g), {prefix})) }}",
        prefix = oxigraph::model::Literal::new_simple_literal(&prefix),
    );
    let store = crate::rdf_store_service::open_graph_store(graph_dir)?;
    let results = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| format!("parse creation-date query: {error}"))?
        .on_store(&store)
        .execute()
        .map_err(|error| format!("run creation-date query: {error}"))?;
    let oxigraph::sparql::QueryResults::Solutions(solutions) = results else {
        return Err("creation-date query returned no solutions".into());
    };
    let mut created_at = DocumentCreatedAt::new();
    for solution in solutions {
        let solution = solution.map_err(|error| format!("read creation date: {error}"))?;
        if let (Some(oxigraph::model::Term::Literal(id)), Some(oxigraph::model::Term::Literal(created))) =
            (solution.get("id"), solution.get("created"))
        {
            created_at.insert(id.value().to_string(), created.value().to_string());
        }
    }
    Ok(created_at)
}

/// The house's top blocks, scored exactly as get_important_blocks scores them.
///
/// get_important_blocks reads every document record for creation dates. This
/// takes them from the RDF projection. A needed document the projection does
/// not hold yet (created moments ago, before materialization) is read from
/// its record, so the scores still match.
pub(crate) fn important_entries(app: &AppHandle, graph_id: &str) -> Result<Vec<ImportantEntry>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut created_at = rdf_created_at(&graph_dir, graph_id).unwrap_or_else(|error| {
        log::warn!("importance view for {graph_id}: creation dates from records, not RDF: {error}");
        DocumentCreatedAt::new()
    });
    let mut documents: BTreeMap<String, Option<crate::document_service::DocumentRecord>> = BTreeMap::new();
    for document_id in documents_needing_creation_date(&graph_dir, graph_id)? {
        if created_at.contains_key(&document_id) {
            continue;
        }
        let record = crate::document_service::read_document_cold(app.clone(), graph_id.to_string(), document_id.clone()).ok();
        if let Some(record) = &record {
            created_at.insert(document_id.clone(), record.created_at.clone());
        }
        documents.insert(document_id, record);
    }
    let scored = local_block_value_scores_with_created_at(
        app,
        graph_id,
        "",
        None,
        None,
        None,
        ENTRY_LIMIT * 4,
        None,
        None,
        &created_at,
    )?;
    let now_ms = crate::clock::epoch_millis();
    let mut entries = Vec::new();
    for score in &scored {
        if entries.len() >= ENTRY_LIMIT {
            break;
        }
        let (Some(document_id), Some(block_id)) = (json_str(score, "document_id"), json_str(score, "block_id")) else {
            continue;
        };
        // The Song, diaries and this view itself are copies; quote originals.
        if document_id == IMPORTANT_BLOCKS_DOC_ID
            || crate::geist_derived_view::is_derived(&graph_dir, document_id).unwrap_or(true)
        {
            continue;
        }
        let document = documents
            .entry(document_id.to_string())
            .or_insert_with(|| {
                crate::document_service::read_document_cold(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .ok()
            });
        let Some(document) = document.as_ref() else {
            continue;
        };
        let Some(block) = find_projected_block(document, block_id) else {
            continue;
        };
        let text = block.content.trim().to_string();
        if text.is_empty() {
            continue;
        }
        entries.push(ImportantEntry {
            document_id: document_id.to_string(),
            block_id: block_id.to_string(),
            document_title: document.title.clone(),
            text,
            score: json_f64(score, "composite_score"),
            // The person's rating and the agents' together, as
            // get_important_blocks reports them; a wired block nobody rated
            // has no combined value.
            importance: score
                .get("combined_importance")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or_else(|| json_f64(score, "cumulative_importance")),
            valence: score
                .get("combined_valence")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or_else(|| json_f64(score, "cumulative_valence")),
            block_wires: json_u64(score, "block_wire_count"),
            doc_wires: json_u64(score, "doc_wire_count"),
            last_valued_days: json_str(score, "last_valuated_at")
                .map(|at| value_timestamp_age_days(Some(at), now_ms))
                .filter(|days| *days < 9_000.0),
        });
    }
    Ok(entries)
}

fn paragraph(id: String, block_type: &str, content: String, order: &mut f64, level: Option<i64>) -> BlockSnapshot {
    let block = BlockSnapshot {
        id,
        block_type: block_type.to_string(),
        content,
        parent_id: None,
        order: *order,
        level,
        checked: None,
        language: None,
        marks: Vec::new(),
    };
    *order += 1.0;
    block
}

fn quote_text(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= QUOTE_MAX_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(QUOTE_MAX_CHARS).collect();
    let cut = cut.rsplit_once(' ').map(|(head, _)| head).unwrap_or(&cut);
    format!("{cut} …")
}

/// When the block was last valued. `valued` says whether it has any value,
/// since a few imported records carry a value without a date.
pub(crate) fn last_valued_label(days: Option<f64>, valued: bool) -> String {
    let Some(days) = days else {
        return if valued { "valued, date unknown" } else { "never valued" }.to_string();
    };
    let whole = days.floor() as i64;
    match whole {
        i64::MIN..=0 => "valued today".to_string(),
        1 => "valued yesterday".to_string(),
        2..=13 => format!("valued {whole} days ago"),
        14..=59 => format!("valued {} weeks ago", whole / 7),
        60..=729 => format!("valued {} months ago", whole / 30),
        _ => format!("valued {} years ago", whole / 365),
    }
}

fn signed(value: f64) -> String {
    if value > 0.0 {
        format!("+{value:.1}")
    } else if value < 0.0 {
        format!("\u{2212}{:.1}", value.abs())
    } else {
        "0".to_string()
    }
}

/// The quoted block, as an italic paragraph. Not a "quote" block: that becomes
/// a blockquote holding text directly, which the editor's schema (blockquote
/// holds paragraphs) drops, so the quotes went missing on screen.
fn quote_block(rank: usize, entry: &ImportantEntry, order: &mut f64) -> BlockSnapshot {
    let text = quote_text(&entry.text);
    let length = text.chars().count() as u32;
    let mut block = paragraph(format!("importance-{rank}-quote"), "paragraph", text, order, None);
    block.marks.push(InlineMarkSnapshot {
        id: format!("importance-{rank}-quote-italic"),
        mark_type: "italic".to_string(),
        start: 0,
        end: length,
        href: None,
        target_doc_id: None,
        label: None,
        target_block_id: None,
        target_graph_id: None,
        annotation_id: None,
        target_artifact_id: None,
        file_path: None,
        region: None,
        wire_id: None,
    });
    block
}

/// The parts line under a quote: the source link, then the score's parts.
fn entry_meta_block(rank: usize, entry: &ImportantEntry, order: &mut f64) -> BlockSnapshot {
    let title = if entry.document_title.trim().is_empty() {
        entry.document_id.clone()
    } else {
        entry.document_title.clone()
    };
    let title_len = title.chars().count() as u32;
    let parts = format!(
        " \u{b7} importance {:.1} \u{b7} valence {} \u{b7} {} block {} \u{b7} {} document {} \u{b7} {}",
        entry.importance,
        signed(entry.valence),
        entry.block_wires,
        if entry.block_wires == 1 { "wire" } else { "wires" },
        entry.doc_wires,
        if entry.doc_wires == 1 { "wire" } else { "wires" },
        last_valued_label(entry.last_valued_days, entry.importance > 0.0 || entry.valence != 0.0),
    );
    let mut block = paragraph(format!("importance-{rank}-source"), "paragraph", format!("{title}{parts}"), order, None);
    block.marks.push(InlineMarkSnapshot {
        id: format!("importance-{rank}-link"),
        mark_type: "wikilink".to_string(),
        start: 0,
        end: title_len,
        href: None,
        target_doc_id: Some(entry.document_id.clone()),
        label: Some(title),
        target_block_id: Some(entry.block_id.clone()),
        target_graph_id: None,
        annotation_id: None,
        target_artifact_id: None,
        file_path: None,
        region: None,
        wire_id: None,
    });
    block
}

/// The house's own prompt text, as paragraphs, without its markdown heading.
fn prompt_paragraphs(prompt: &str) -> Vec<String> {
    prompt
        .split("\n\n")
        .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|part| !part.is_empty() && !part.starts_with('#'))
        .collect()
}

pub(crate) fn important_blocks_document(entries: &[ImportantEntry], config: &LocalValueConfig) -> Vec<BlockSnapshot> {
    let mut order = 0.0;
    let mut blocks = vec![
        paragraph("importance-title".into(), "heading", IMPORTANT_BLOCKS_TITLE.into(), &mut order, Some(1)),
        paragraph(
            "importance-intro".into(),
            "paragraph",
            "The blocks this graph holds most important now, as valued by you and your agents together. \
             The page rebuilds as values change. Each title opens the block where it lives, and a value \
             given here goes to that block, not to this page."
                .into(),
            &mut order,
            None,
        ),
    ];
    if entries.is_empty() {
        blocks.push(paragraph("importance-empty".into(), "paragraph", "Nothing has been valued yet.".into(), &mut order, None));
    }
    for (rank, entry) in entries.iter().enumerate() {
        blocks.push(quote_block(rank, entry, &mut order));
        blocks.push(entry_meta_block(rank, entry, &mut order));
    }

    blocks.push(paragraph("importance-how".into(), "heading", "How value works".into(), &mut order, Some(2)));
    blocks.push(paragraph(
        "importance-how-intro".into(),
        "paragraph",
        "Anyone in the graph can value a block: importance from 0 to 5, and valence from \u{2212}5 to +5. \
         Values add up across people and agents, growing more slowly the more they pile on, and fade with \
         time unless someone values the block again. A block's place on this page is a weighted sum of six \
         parts, each between 0 and 1:"
            .into(),
        &mut order,
        None,
    ));
    let half_life = value_weight(config, "half_life_days");
    let terms = [
        ("importance_weight", "importance, as valued".to_string()),
        ("valence_weight", "the strength of its valence, positive or negative alike".to_string()),
        ("block_wires_weight", "wires to and from the block".to_string()),
        ("temporal_weight", format!("recency of its last valuation, halving every {half_life:.0} days")),
        ("doc_wires_weight", "wires to and from its document".to_string()),
        ("wire_freshness_weight", "how recent its newest wire is".to_string()),
    ];
    for (index, (key, label)) in terms.iter().enumerate() {
        blocks.push(paragraph(
            format!("importance-term-{index}"),
            "paragraph",
            format!("{:.2} \u{d7} {label}", value_weight(config, key)),
            &mut order,
            None,
        ));
    }
    blocks.push(paragraph(
        "importance-how-weights".into(),
        "paragraph",
        "These weights are this graph's, shared by everyone in it. Changing them changes this page \
         for everyone, and the order every agent sees."
            .into(),
        &mut order,
        None,
    ));
    for (heading_id, heading, prompt) in [
        ("importance-prompt-importance", "What importance asks", config.importance_prompt.as_str()),
        ("importance-prompt-valence", "What valence asks", config.valence_prompt.as_str()),
    ] {
        blocks.push(paragraph(heading_id.into(), "heading", heading.into(), &mut order, Some(3)));
        for (index, text) in prompt_paragraphs(prompt).into_iter().enumerate() {
            blocks.push(paragraph(format!("{heading_id}-{index}"), "paragraph", text, &mut order, None));
        }
    }
    blocks
}

/// Rebuild the view now. Returns how many entries it holds.
pub(crate) fn rebuild_important_blocks_projection(app: &AppHandle, graph_id: &str) -> Result<usize, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let entries = important_entries(app, graph_id)?;
    let mut config = read_value_store(&graph_dir, graph_id)?.config;
    normalize_value_config(&mut config);
    write_geist_projection_document(
        app,
        &graph_dir,
        graph_id,
        IMPORTANT_BLOCKS_DOC_ID,
        IMPORTANT_BLOCKS_TITLE,
        important_blocks_document(&entries, &config),
        vec!["document.local.read".to_string(), IMPORTANT_BLOCKS_CAPABILITY.to_string()],
        None,
    )?;
    Ok(entries.len())
}

/// Rebuild each graph's view once after startup, so it reflects values that
/// changed while the view was not running (an import, an older build).
pub(crate) fn schedule_startup_rebuilds(app: &AppHandle) {
    let Ok(graphs_dir) = crate::profile_paths::graphs_dir(app) else { return };
    let Ok(entries) = std::fs::read_dir(&graphs_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.join("values").join("block-values.json").is_file() {
            continue;
        }
        if let Some(graph_id) = path.file_name().and_then(|name| name.to_str()) {
            schedule_important_blocks_rebuild(app, graph_id);
        }
    }
}

#[derive(Default)]
struct RebuildState {
    pending: bool,
    last_run: Option<Instant>,
}

fn rebuild_states() -> &'static Mutex<BTreeMap<String, RebuildState>> {
    static STATES: OnceLock<Mutex<BTreeMap<String, RebuildState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Ask for a rebuild after values change. Calls inside one window coalesce
/// into a single rebuild that runs after the window, so the last change always
/// lands.
pub(crate) fn schedule_important_blocks_rebuild(app: &AppHandle, graph_id: &str) {
    if cfg!(test) {
        return;
    }
    let delay = {
        let Ok(mut states) = rebuild_states().lock() else { return };
        let state = states.entry(graph_id.to_string()).or_default();
        if state.pending {
            return;
        }
        state.pending = true;
        let since_last = state.last_run.map(|at| at.elapsed()).unwrap_or(REBUILD_INTERVAL);
        REBUILD_SETTLE.max(REBUILD_INTERVAL.saturating_sub(since_last))
    };
    let app = app.clone();
    let graph_id = graph_id.to_string();
    crate::app_runtime::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Ok(mut states) = rebuild_states().lock() {
            let state = states.entry(graph_id.clone()).or_default();
            state.pending = false;
            state.last_run = Some(Instant::now());
        }
        // A maintenance lease keeps a headless cell from deciding it is idle and
        // exiting mid-write (examples/gardend.rs waits for idle, and an unleased
        // spawn_blocking is invisible to it). Not held during the settle above.
        let Some(lifecycle) = app
            .try_state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
            .map(|state| state.inner().clone())
        else {
            log::warn!("importance view for {graph_id} not rebuilt: no cell lifecycle");
            return;
        };
        let _lease = match lifecycle.begin_maintenance("importance-view") {
            Ok(lease) => lease,
            Err(_) => {
                log::info!("importance view for {graph_id} not rebuilt: the cell is draining");
                return;
            }
        };
        let task_app = app.clone();
        let task_graph = graph_id.clone();
        match crate::app_runtime::async_runtime::spawn_blocking(move || {
            rebuild_important_blocks_projection(&task_app, &task_graph)
        })
        .await
        {
            Ok(Ok(count)) => log::info!("importance view for {graph_id} rebuilt: {count} entries"),
            Ok(Err(error)) => log::warn!("importance view for {graph_id} not rebuilt: {error}"),
            Err(error) => log::warn!("importance view for {graph_id} rebuild task failed: {error}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, importance: f64, valence: f64) -> ImportantEntry {
        ImportantEntry {
            document_id: "doc-a".into(),
            block_id: "block-1".into(),
            document_title: title.into(),
            text: "A sentence that matters.".into(),
            score: 0.5,
            importance,
            valence,
            block_wires: 1,
            doc_wires: 12,
            last_valued_days: Some(3.2),
        }
    }

    /// The defaults with default_x1's weights for importance and half-life.
    fn config() -> LocalValueConfig {
        let mut config = crate::salience_value_config::default_value_config();
        config.weights.insert("importance_weight".into(), 0.4);
        config.weights.insert("half_life_days".into(), 60.0);
        config
    }

    #[test]
    fn each_entry_quotes_its_block_and_links_to_the_source() {
        let blocks = important_blocks_document(&[entry("On Being \u{2014} Given Room", 4.86, -4.7)], &config());
        let quote = blocks.iter().find(|b| b.id == "importance-0-quote").expect("quote");
        assert_eq!(quote.block_type, "paragraph", "a blockquote holding text is invalid in the editor");
        assert_eq!(quote.content, "A sentence that matters.");
        assert_eq!(quote.marks[0].mark_type, "italic");
        assert_eq!((quote.marks[0].start, quote.marks[0].end), (0, 24));
        let meta = blocks.iter().find(|b| b.id == "importance-0-source").expect("source line");
        assert_eq!(
            meta.content,
            "On Being \u{2014} Given Room \u{b7} importance 4.9 \u{b7} valence \u{2212}4.7 \u{b7} 1 block wire \u{b7} 12 document wires \u{b7} valued 3 days ago"
        );
        let link = &meta.marks[0];
        assert_eq!(link.mark_type, "wikilink");
        assert_eq!((link.start, link.end), (0, 21), "the mark covers the title, counted in chars");
        assert_eq!(link.target_doc_id.as_deref(), Some("doc-a"));
        assert_eq!(link.target_block_id.as_deref(), Some("block-1"));
    }

    #[test]
    fn the_explainer_names_the_six_terms_with_the_house_weights() {
        let blocks = important_blocks_document(&[], &config());
        let terms: Vec<&str> = blocks
            .iter()
            .filter(|b| b.id.starts_with("importance-term-"))
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(terms.len(), 6);
        assert!(terms[0].starts_with("0.40 \u{d7} importance"));
        assert!(terms[3].contains("halving every 60 days"));
        assert!(blocks.iter().any(|b| b.id == "importance-empty"));
        assert!(blocks.iter().any(|b| b.id == "importance-prompt-importance-0"));
        assert!(!blocks.iter().any(|b| b.content.starts_with('#')), "prompt headings are dropped");
    }

    #[test]
    fn long_quotes_are_cut_at_a_word() {
        let long = "word ".repeat(400);
        let cut = quote_text(&long);
        assert!(cut.ends_with(" \u{2026}"));
        assert!(cut.chars().count() <= QUOTE_MAX_CHARS + 2);
    }

    #[test]
    fn last_valued_reads_like_speech() {
        assert_eq!(last_valued_label(Some(0.3), true), "valued today");
        assert_eq!(last_valued_label(Some(1.5), true), "valued yesterday");
        assert_eq!(last_valued_label(Some(20.0), true), "valued 2 weeks ago");
        assert_eq!(last_valued_label(Some(90.0), true), "valued 3 months ago");
        assert_eq!(last_valued_label(None, false), "never valued");
        assert_eq!(last_valued_label(None, true), "valued, date unknown");
    }
}
