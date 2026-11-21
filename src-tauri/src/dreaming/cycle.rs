//! One dream cycle for one graph (cloud-1 `dream_handler.py`):
//! maturity, scale, scans, charge, weave, articulation, journal, memory.
//!
//! Split so the decisions can be tested and run without the live app:
//! [`load`] reads a graph directory's files (read-only), [`prepare`] is pure
//! and produces every Dreamer input, [`articulate`] calls the dreamer, and
//! [`write_results`] publishes the journal and the cycle memory.

use super::articulate::{bundle_prompt, parse_reply, Articulation};
use super::astrology::{dream_context, DreamContext};
use super::calibration::Calibration;
use super::journal::{journal_doc_id, journal_markdown, markdown_blocks, JournalEntry};
use super::mechanics::{self as m, dream_scale, max_bundles_for_scale, Mechanics};
use super::rng::stable_hash;
use super::scanner::{self, budgets, Budgets, IndexedBlock, ScanInput, ValuedBlock};
use super::weave::{self, Bundle};
use crate::dreamer::{call_dreamer, ConsentGate, DreamReceipt, DreamRequest, DreamerConfig};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
#[cfg(feature = "desktop")]
use tauri::Manager;

/// The trigger a cycle runs under, stamped on its receipt.
pub(crate) const MANUAL: &str = "manual";
pub(crate) const SCHEDULED: &str = "scheduled";

/// What the cycle remembers between nights: the documents it last dreamed
/// about, kept out of the next cycle (cloud-1's dream-state.json).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DreamState {
    #[serde(default)]
    pub(crate) suppressed_doc_ids: Vec<String>,
    #[serde(default)]
    pub(crate) cycle_date: String,
    #[serde(default)]
    pub(crate) dream_count: usize,
}

pub(crate) fn dream_state_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join("dreaming").join("dream-state.json")
}

/// A graph's files as one cycle needs them. Everything is read, nothing is
/// written or repaired.
pub(crate) struct Loaded {
    pub(crate) graph_id: String,
    pub(crate) index: Arc<crate::semantic_index::SemanticIndexFile>,
    pub(crate) documents: Vec<Value>,
    pub(crate) folders: Vec<Value>,
    pub(crate) wires: Vec<Value>,
    pub(crate) values: Vec<ValuedBlock>,
    pub(crate) derived: HashSet<String>,
    /// Documents the person keeps out of their dreams (exclusions.json).
    pub(crate) excluded: HashSet<String>,
    /// Each document's last edit (epoch ms), for the temporal curve.
    pub(crate) doc_edited: HashMap<String, i64>,
    /// This graph's dream settings (the residue window).
    pub(crate) settings: super::graph_settings::GraphSettings,
    pub(crate) state: DreamState,
}

pub(crate) fn load(graph_dir: &Path, graph_id: &str) -> Result<Loaded, String> {
    let index = crate::semantic_search_cache::read_cached_index(graph_dir)
        .map_err(|error| format!("dreaming needs the semantic index: {error}"))?;
    let workspace = crate::workspace_record_store::read_workspace_record(graph_dir, graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let store = crate::salience_value_store::read_value_store_for(graph_dir, graph_id, "")?;
    let values = store
        .blocks
        .values()
        .filter(|r| !r.last_valuated_at.trim().is_empty())
        .map(|r| ValuedBlock {
            block_id: r.block_id.clone(),
            doc_id: r.document_id.clone(),
            importance: r.cumulative_importance,
            valence: r.cumulative_valence,
            last_valued_ms: super::temporal::parse_ms_str(&r.last_valuated_at),
            temporal: 0.0,
            recent: false,
        })
        .collect();
    let mut derived = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(graph_dir.join("narrative/derived-views")) {
        for entry in entries.flatten() {
            if let Some(id) = entry.file_name().to_str().and_then(|n| n.strip_suffix(".json")) {
                derived.insert(id.to_string());
            }
        }
    }
    let state_path = dream_state_path(graph_dir);
    let state = if state_path.is_file() {
        crate::storage::read_json(&state_path).unwrap_or_default()
    } else {
        DreamState::default()
    };
    let documents = crate::workspace_entity_projection::workspace_documents(snapshot);
    let folders = crate::workspace_entity_projection::workspace_folders(snapshot);
    // A broken exclusions file stops the cycle: it must not dream on
    // documents the person meant to keep out.
    let excluded = super::exclusions::current_closure(graph_dir, graph_id)?;
    let doc_edited = super::temporal::document_edit_times(graph_dir, chrono::Utc::now().timestamp_millis());
    Ok(Loaded {
        graph_id: graph_id.to_string(),
        index,
        documents,
        folders,
        excluded,
        doc_edited,
        settings: super::graph_settings::read(graph_dir),
        wires: crate::workspace_entity_projection::workspace_wires(snapshot),
        values,
        derived,
        state,
    })
}

/// Everything decided before the dreamer is asked.
pub(crate) struct Prepared {
    pub(crate) status: &'static str,
    pub(crate) seed: u64,
    pub(crate) scale: f64,
    pub(crate) budgets: Option<Budgets>,
    pub(crate) counts: Value,
    pub(crate) calibration: Value,
    /// This graph's settings as they ran.
    pub(crate) graph_settings: Value,
    pub(crate) context: Option<DreamContext>,
    pub(crate) titles: HashMap<String, String>,
    pub(crate) bundles: Vec<Bundle>,
    pub(crate) prompts: Vec<String>,
}

impl Prepared {
    fn stopped(status: &'static str, seed: u64, counts: Value) -> Self {
        Prepared {
            status,
            seed,
            scale: 0.0,
            budgets: None,
            counts,
            calibration: json!(null),
            graph_settings: json!(null),
            context: None,
            titles: HashMap::new(),
            bundles: Vec::new(),
            prompts: Vec::new(),
        }
    }
}

/// `now_ms` is when the cycle runs, for the temporal curve.
pub(crate) fn prepare(loaded: &Loaded, mechanics: &Mechanics, date: NaiveDate, now_ms: i64) -> Prepared {
    let graph_id = &loaded.graph_id;
    let seed = stable_hash(&format!("{graph_id}|{date}"));
    let edges = super::wire_graph::edges_from_workspace_wires(&loaded.wires);
    if edges.len() < m::GRAPH_MATURITY_MIN_WIRES {
        return Prepared::stopped("immature", seed, json!({ "wires": edges.len() }));
    }
    if loaded.values.len() < m::GRAPH_MATURITY_MIN_VALUATIONS {
        return Prepared::stopped("immature", seed, json!({ "valuations": loaded.values.len() }));
    }

    let mut titles = HashMap::new();
    for doc in &loaded.documents {
        if !crate::wire_state::wire_is_active(doc) {
            continue;
        }
        if let Some(id) = doc.get("id").and_then(Value::as_str) {
            let title = doc.get("title").and_then(Value::as_str).unwrap_or(id);
            titles.insert(id.to_string(), title.to_string());
        }
    }
    let scale = dream_scale(&mechanics.scaling, titles.len(), edges.len());
    let budgets = budgets(scale);

    // Every dream journal, including ones imported from cloud-1 under that
    // graph's own id (default_x1 holds cloud's `default-dream-journal`): a
    // journal quotes its sources, so dreaming on it dreams on copies.
    let is_journal = |doc: &str| doc.ends_with("-dream-journal");
    let suppressed: HashSet<&str> = if m::DREAM_MEMORY_ENABLED {
        loaded.state.suppressed_doc_ids.iter().map(String::as_str).collect()
    } else {
        HashSet::new()
    };
    let eligible = |doc: &str| {
        titles.contains_key(doc)
            && !is_journal(doc)
            && !suppressed.contains(doc)
            && !loaded.derived.contains(doc)
            && !loaded.excluded.contains(doc)
    };

    let clock = super::temporal::Clock {
        curve: &mechanics.temporal_curve,
        now_ms,
        doc_edited: &loaded.doc_edited,
        block_valued: loaded.values.iter().filter_map(|v| Some((v.block_id.as_str(), v.last_valued_ms?))).collect(),
    };
    // The day's residue: material touched within this graph's window.
    let residue_days = loaded.settings.residue_days();
    let residue_slot = residue_days > 0.0;
    let recent = |doc: &str, block: Option<&str>| {
        residue_slot && clock.age_days(doc, block).is_some_and(|days| days <= residue_days)
    };
    let dims = loaded.index.manifest.dimensions;
    let blocks: Vec<IndexedBlock> = loaded
        .index
        .blocks
        .iter()
        .filter(|b| b.vector.len() == dims && dims > 0 && eligible(&b.document_id))
        .map(|b| IndexedBlock {
            doc: &b.document_id,
            block: &b.block_id,
            content: &b.content,
            order: b.order,
            vector: &b.vector,
            temporal: clock.temporal(&b.document_id, Some(&b.block_id)),
        })
        .collect();
    let vectors: Vec<&[f32]> = blocks.iter().map(|b| b.vector).collect();
    let calibration = Calibration::measure(&vectors, seed);

    let input = ScanInput {
        blocks,
        calibration,
        // Seeds come only from documents a dream may use (cloud-1 spent seed
        // slots on suppressed documents and then skipped them).
        valued: loaded
            .values
            .iter()
            .filter(|v| eligible(&v.doc_id))
            .map(|v| ValuedBlock {
                temporal: clock.temporal(&v.doc_id, Some(&v.block_id)),
                recent: recent(&v.doc_id, Some(&v.block_id)),
                ..v.clone()
            })
            .collect(),
        // Scanners see only wires between documents a dream may use. cloud-1
        // scanned every wire and dropped findings afterwards, so suppressed
        // documents used up the path budget; with exclusions that would
        // also let an excluded document shape a dream.
        edges: edges.iter().filter(|e| eligible(&e.source_doc) && eligible(&e.target_doc)).cloned().collect(),
        titles: titles.clone(),
        seed,
    };
    let mut results = scanner::scan_all(&input, budgets);
    let scanned = results.total();
    // Wire and isomorphism scans see the whole wire graph; drop what names
    // a deleted, suppressed, derived or journal document.
    results.retain_docs(eligible);
    let mut counts = json!({
        "documents": titles.len(),
        "wires": edges.len(),
        "eligibleWires": input.edges.len(),
        "valuedBlocks": input.valued.len(),
        "indexedBlocks": input.blocks.len(),
        "suppressedDocuments": suppressed.len(),
        "excludedDerivedViews": loaded.derived.len(),
        "excludedByPerson": loaded.excluded.len(),
        "findingsScanned": scanned,
        "displacements": results.displacements.len(),
        "condensations": results.condensations.len(),
        "wireInferences": results.wire_inferences.len(),
        "isomorphisms": results.isomorphisms.len(),
    });
    let calibration_summary = input.calibration.summary();
    if results.total() == 0 {
        let mut stopped = Prepared::stopped("no_findings", seed, counts);
        stopped.scale = scale;
        stopped.budgets = Some(budgets);
        stopped.calibration = calibration_summary;
        stopped.graph_settings = loaded.settings.receipt();
        return stopped;
    }

    // The blocks (or, for wire and isomorphism findings, documents) a
    // finding is made of.
    let parts = |finding: &super::scanner::Finding| -> Vec<(String, Option<String>)> {
        use super::scanner::Finding as F;
        match finding {
            F::Displacement(f) => {
                let mut parts = vec![
                    (f.source_doc.clone(), Some(f.source_block.clone())),
                    (f.neighbor_doc.clone(), Some(f.neighbor_block.clone())),
                ];
                if let Some((block, doc, _)) = &f.intermediary {
                    parts.push((doc.clone(), Some(block.clone())));
                }
                parts
            }
            F::Condensation(f) => f.blocks.iter().map(|b| (b.doc.clone(), Some(b.block.clone()))).collect(),
            F::WireInference(f) => f.docs.iter().map(|d| (d.clone(), None)).collect(),
            F::Isomorphism(f) => vec![(f.doc_a.clone(), None), (f.doc_b.clone(), None)],
        }
    };
    // A finding's temporal value is the mean over its parts.
    let finding_temporal = |finding: &super::scanner::Finding| {
        let values: Vec<f64> = parts(finding).iter().map(|(d, b)| clock.temporal(d, b.as_deref())).collect();
        if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
    };
    let mut ranked = weave::rank(results, &mechanics.charge_weights, &finding_temporal);
    // A finding is residue when any of its parts is recent.
    for item in &mut ranked {
        item.residue = parts(&item.finding).iter().any(|(d, b)| recent(d, b.as_deref()));
    }
    counts["residueCandidates"] = json!(ranked.iter().filter(|r| r.residue).count());
    let bundles = weave::weave(&ranked, max_bundles_for_scale(&mechanics.scaling, scale), residue_slot);
    counts["bundles"] = json!(bundles.len());
    counts["residueSlotsFilled"] = json!(bundles.iter().filter(|b| b.residue_slot).count());
    let context = mechanics.astrology_enabled.then(|| dream_context(date, &format!("local:{graph_id}")));
    let prompts = bundles.iter().map(|b| bundle_prompt(b, context.as_ref(), &titles)).collect();
    Prepared {
        status: if bundles.is_empty() { "no_bundles" } else { "prepared" },
        seed,
        scale,
        budgets: Some(budgets),
        counts,
        calibration: calibration_summary,
        graph_settings: loaded.settings.receipt(),
        context,
        titles,
        bundles,
        prompts,
    }
}

/// One bundle's trip to the dreamer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Articulated {
    pub(crate) articulation: Option<Articulation>,
    pub(crate) receipts: Vec<DreamReceipt>,
}

/// Ask the dreamer for each bundle. A reply that ran but carried no JSON
/// object is asked once more (cloud-1's two attempts); a run that failed
/// (exit code, timeout, cancel, consent, transport) is not repeated.
pub(crate) async fn articulate(
    app: &crate::app_runtime::AppHandle,
    dreamer: &DreamerConfig,
    mechanics: &Mechanics,
    prepared: &Prepared,
    consent: ConsentGate,
    cancel: Arc<AtomicBool>,
) -> Vec<Articulated> {
    let mut out = Vec::new();
    for prompt in &prepared.prompts {
        let request = DreamRequest { system: mechanics.dreamer_prompt.clone(), user: prompt.clone() };
        let mut receipts = Vec::new();
        let mut articulation = None;
        for _ in 0..m::ARTICULATION_ATTEMPTS {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let reply = call_dreamer(app, dreamer, &request, consent.clone(), cancel.clone(), crate::dreamer::DEFAULT_TIMEOUT).await;
            let unparseable = reply.receipt.error.as_deref() == Some(crate::dreamer::NO_JSON_ERROR);
            receipts.push(reply.receipt);
            if let Some(json) = reply.json {
                articulation = Some(parse_reply(&json));
                break;
            }
            if !unparseable {
                break;
            }
        }
        out.push(Articulated { articulation, receipts });
    }
    out
}

/// The journal entries the cycle keeps, in bundle order.
pub(crate) fn journal_entries(prepared: &Prepared, articulated: &[Articulated]) -> Vec<JournalEntry> {
    prepared
        .bundles
        .iter()
        .zip(articulated)
        .filter_map(|(bundle, a)| {
            let art = a.articulation.as_ref().filter(|art| art.accepted())?;
            Some(JournalEntry {
                narrative: art.narrative.clone(),
                confidence: art.confidence,
                is_navel: art.is_navel,
                total_charge: bundle.total_charge,
                findings: bundle.findings.clone(),
            })
        })
        .collect()
}

pub(crate) fn journal_date_label(date: NaiveDate) -> String {
    date.format("%B %d, %Y").to_string()
}

/// The documents tonight's dreams touched: tomorrow's suppression list.
pub(crate) fn next_state(entries: &[JournalEntry], date: NaiveDate) -> DreamState {
    let docs: BTreeSet<String> = entries.iter().flat_map(|e| e.findings.iter().flat_map(|r| r.finding.doc_ids())).collect();
    DreamState { suppressed_doc_ids: docs.into_iter().collect(), cycle_date: date.to_string(), dream_count: entries.len() }
}

/// What ran, as a receipt can show it: edited values in full, the prompt by
/// hash and length (it is also readable on the Dreaming page).
fn mechanics_receipt(mechanics: &Mechanics, prepared: &Prepared) -> Value {
    let mut edited_values = serde_json::Map::new();
    for field in &mechanics.edited {
        let value = match *field {
            "chargeWeights" => json!(mechanics.charge_weights),
            "scaling" => json!(mechanics.scaling),
            "astrologyEnabled" => json!(mechanics.astrology_enabled),
            "temporalCurve" => json!(mechanics.temporal_curve),
            "dreamerPrompt" => json!({ "chars": mechanics.dreamer_prompt.chars().count() }),
            _ => continue,
        };
        edited_values.insert(field.to_string(), value);
    }
    json!({
        "edited": mechanics.edited,
        "defaults": mechanics.edited.is_empty(),
        "temporalCurve": mechanics.temporal_curve,
        "editedValues": edited_values,
        "dreamerPromptHash": format!("{:016x}", stable_hash(&mechanics.dreamer_prompt)),
        "astrology": prepared.context,
    })
}

/// The receipt of a whole cycle: what ran (defaults vs edited), what was
/// found, what was asked, what came back.
pub(crate) fn cycle_receipt(
    graph_id: &str,
    date: NaiveDate,
    mechanics: &Mechanics,
    prepared: &Prepared,
    articulated: &[Articulated],
    status: &str,
    journal: Option<&str>,
) -> Value {
    let dreams: Vec<Value> = prepared
        .bundles
        .iter()
        .enumerate()
        .map(|(i, bundle)| {
            let a = articulated.get(i);
            json!({
                "kinds": bundle.kinds_in_order(),
                "residueSlot": bundle.residue_slot,
                "totalCharge": bundle.total_charge,
                "charges": bundle.findings.iter().map(|r| r.charge).collect::<Vec<_>>(),
                "promptChars": prepared.prompts.get(i).map(|p| p.chars().count()),
                "articulation": a.and_then(|a| a.articulation.clone()),
                "accepted": a.and_then(|a| a.articulation.as_ref()).is_some_and(|art| art.accepted()),
                "dreamerReceipts": a.map(|a| a.receipts.clone()).unwrap_or_default(),
            })
        })
        .collect();
    json!({
        "graphId": graph_id,
        "date": date.to_string(),
        "status": status,
        "seed": format!("{:016x}", prepared.seed),
        "scale": prepared.scale,
        "budgets": prepared.budgets,
        "counts": prepared.counts,
        "calibration": prepared.calibration,
        "graphSettings": prepared.graph_settings,
        "mechanics": mechanics_receipt(mechanics, prepared),
        "dreams": dreams,
        "journal": journal,
    })
}

const JOURNAL_FOLDER: &str = "agent-scratchpad";

/// Where the journal should be placed this time: cloud-1 kept it in the
/// _sophia folder. Placed when it has no entry yet or sits at the root, and
/// only when that folder exists in this graph.
fn journal_parent(loaded: &Loaded, journal: &str) -> Option<&'static str> {
    let snapshot_folders = loaded.folders.iter().any(|f| f.get("id").and_then(Value::as_str) == Some(JOURNAL_FOLDER));
    let entry = loaded.documents.iter().find(|d| d.get("id").and_then(Value::as_str) == Some(journal));
    let unplaced = entry.is_none_or(|e| e.get("parentId").is_none_or(Value::is_null));
    (snapshot_folders && unplaced).then_some(JOURNAL_FOLDER)
}

/// An ordinary (non-derived) document already holds the journal's id: the
/// Geist write would refuse, so the cycle must stop before paying the dreamer.
fn journal_blocked(graph_dir: &Path, journal: &str) -> Result<bool, String> {
    let ordinary = graph_dir.join("documents").join(journal).join("document.json").is_file();
    Ok(ordinary && !crate::geist_derived_view::is_derived(graph_dir, journal)?)
}

/// Publish the journal as a read-only Geist view and remember tonight's
/// documents. Blocking: call it from a blocking thread.
pub(crate) fn write_results(
    app: &crate::app_runtime::AppHandle,
    graph_dir: &Path,
    graph_id: &str,
    markdown: &str,
    block_prefix: &str,
    state: &DreamState,
    parent: Option<&str>,
) -> Result<String, String> {
    let doc_id = journal_doc_id(graph_id);
    // The dream state goes first: if the journal write then fails, tomorrow
    // still moves on to other documents.
    if m::DREAM_MEMORY_ENABLED {
        let path = dream_state_path(graph_dir);
        crate::storage::create_dir_all(path.parent().ok_or("dream state path")?).map_err(|e| e.to_string())?;
        crate::storage::write_json(&path, state).map_err(|e| e.to_string())?;
    }
    crate::geist_projection_document::write_geist_projection_document(
        app,
        graph_dir,
        graph_id,
        &doc_id,
        "Dream Journal",
        markdown_blocks(markdown, block_prefix),
        vec!["dreaming.local.journal".to_string()],
        None,
    )?;
    if let Some(parent) = parent {
        let now = crate::clock::epoch_millis() as f64;
        let entry = json!({ "documentId": doc_id, "title": "Dream Journal", "readOnly": true,
                            "parentId": parent, "order": now, "updatedAt": now });
        crate::app_runtime::async_runtime::block_on(crate::crdt_engine::workspace_ops::upsert_document_entry(
            app, graph_id, &entry, "dreaming",
        ))?;
    }
    Ok(doc_id)
}

/// The last cycle's receipt, kept beside the dream state. Best effort.
fn keep_receipt(graph_dir: &Path, receipt: &Value) {
    let path = graph_dir.join("dreaming").join("last-cycle.json");
    if let Some(dir) = path.parent() {
        let _ = crate::storage::create_dir_all(dir);
    }
    if let Err(error) = crate::storage::write_json(&path, receipt) {
        log::warn!("dreaming: could not keep the cycle receipt: {error}");
    }
}

/// One cycle per graph at a time: a second press, or the scheduler meeting
/// a hand-run cycle, is refused rather than spending the dreamer twice.
struct InFlight(String);

impl InFlight {
    fn claim(graph_id: &str) -> Option<Self> {
        let mut running = in_flight().lock().unwrap_or_else(|p| p.into_inner());
        running.insert(graph_id.to_string()).then(|| InFlight(graph_id.to_string()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        in_flight().lock().unwrap_or_else(|p| p.into_inner()).remove(&self.0);
    }
}

fn in_flight() -> &'static std::sync::Mutex<HashSet<String>> {
    static RUNNING: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// The cycle's receipt in the job registry, beside every other local job.
/// Best effort: the job id, or why it wasn't kept.
fn record_job(app: &crate::app_runtime::AppHandle, graph_id: &str, started_ms: u128, receipt: &Value) -> Result<String, String> {
    let jobs_dir = crate::profile_paths::jobs_dir(app)?;
    let jobs = crate::local_jobs::LocalJobRegistry::new(jobs_dir).map_err(crate::app_error::AppError::message)?;
    jobs.insert_finished("dream-cycle", Some(graph_id.to_string()), started_ms, Ok(receipt.clone()), "application/json")
        .map(|record| record.job_id)
        .map_err(|error| {
            log::warn!("dreaming: could not record the cycle job: {error}");
            error.message()
        })
}

/// Record the job and say on the receipt where it went.
fn with_job(app: &crate::app_runtime::AppHandle, graph_id: &str, started_ms: u128, mut receipt: Value) -> (Value, Option<String>) {
    match record_job(app, graph_id, started_ms, &receipt) {
        Ok(job) => {
            receipt["jobId"] = json!(job);
            (receipt, Some(job))
        }
        Err(error) => {
            receipt["jobError"] = json!(error);
            (receipt, None)
        }
    }
}

/// The graph's write authority, held only around writes into the graph: at
/// the reservation, when settling, and through publication; never across
/// the dreamer wait.
pub(crate) async fn graph_write_lease(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
) -> Result<Option<crate::crdt_engine::persistence_coordinator::HotWriteLease>, String> {
    match app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>() {
        Some(coordinator) => coordinator.acquire_hot_write(graph_id).await.map(Some),
        None => Ok(None),
    }
}

/// What must be unchanged before a cycle writes into the graph again after
/// the dreamer wait: the in-process generation (delete, recreate), the
/// durable incarnation (a graph recreated under the same id, across
/// restarts) and the restore-point index (every restore first captures a
/// backup point there). Checked under the graph write lease.
#[derive(Debug, Clone, PartialEq)]
struct GraphFence {
    generation: Option<u64>,
    incarnation: Option<String>,
    restore_index: Option<u64>,
}

impl GraphFence {
    fn capture(app: &crate::app_runtime::AppHandle, graph_id: &str, graph_dir: &Path) -> Result<Self, String> {
        let generation = match app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>() {
            Some(coordinator) => Some(coordinator.generation(graph_id)?),
            None => None,
        };
        let (_, record) = crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
            .map_err(crate::app_error::AppError::message)?;
        let index = crate::time_travel_paths::restore_point_index_path(graph_dir);
        let restore_index = match std::fs::read(&index) {
            Ok(bytes) => Some(super::rng::stable_hash(&String::from_utf8_lossy(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("the restore-point index can't be read: {error}")),
        };
        Ok(GraphFence { generation, incarnation: record.incarnation_id, restore_index })
    }

    fn verify(&self, app: &crate::app_runtime::AppHandle, graph_id: &str, graph_dir: &Path) -> Result<(), String> {
        if let (Some(expected), Some(coordinator)) =
            (self.generation, app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>())
        {
            coordinator.require_generation(graph_id, expected)?;
        }
        let now = GraphFence::capture(app, graph_id, graph_dir)?;
        if now.incarnation != self.incarnation {
            return Err("the graph was replaced during the cycle".into());
        }
        if now.restore_index != self.restore_index {
            return Err("the graph was restored (or a restore point was taken) during the cycle".into());
        }
        Ok(())
    }
}

/// The consent gate a cycle actually sends under: none of the documents in
/// tonight's prepared dreams may be excluded now (exclusions as currently
/// stored, folders as currently arranged), and the caller's gate (for a
/// scheduled cycle, the graph's nightly consent) must open. Asked before
/// every send to the dreamer and, under the graph write lease, once more
/// before publication.
pub(crate) fn cycle_gate(
    consent: ConsentGate,
    graph_dir: PathBuf,
    graph_id: String,
    prepared_docs: Arc<BTreeSet<String>>,
) -> ConsentGate {
    Arc::new(move || {
        let excluded = super::exclusions::current_closure(&graph_dir, &graph_id)?;
        if prepared_docs.iter().any(|doc| excluded.contains(doc)) {
            return Err("a document in tonight's dreams is now kept out of dreaming; nothing more is sent or published".into());
        }
        consent()
    })
}

/// Test seams. Production cycles run with the default: the real reader, the
/// real dreamer, nothing between the publication checks and the write.
pub(crate) type LoadHook = Arc<dyn Fn(&Path, &str) -> Result<Loaded, String> + Send + Sync>;
pub(crate) type ArticulateHook = Arc<
    dyn Fn(Vec<String>, ConsentGate) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Articulated>> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub(crate) struct CycleHooks {
    pub(crate) load: Option<LoadHook>,
    pub(crate) articulate: Option<ArticulateHook>,
    /// Runs inside the publication worker, lease held, checks passed.
    pub(crate) before_publish: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// One full cycle for one graph: by hand ("dream now", trigger `MANUAL`) or
/// from the nightly scheduler (`SCHEDULED`). `dry_run` stops before the
/// dreamer is asked and returns the prompts that would have been sent.
pub(crate) async fn dream_now(
    app: crate::app_runtime::AppHandle,
    graph_id: String,
    dry_run: bool,
    consent: ConsentGate,
    cancel: Arc<AtomicBool>,
    trigger: &'static str,
) -> Result<Value, String> {
    run_cycle(app, graph_id, dry_run, consent, cancel, trigger, CycleHooks::default()).await
}

/// Order: claim the graph; for a scheduled cycle, recheck that it is due;
/// under the graph write lease, capture the fence and reserve the start in
/// the schedule record (a cycle that can't, doesn't run); read and prepare;
/// ask the dreamer, each send behind the gate; then publish from a worker
/// that owns the claim, the cell lease and the graph write lease until it
/// is done, so a cancelled caller can't let a restore, delete or second
/// cycle in while it writes.
pub(crate) async fn run_cycle(
    app: crate::app_runtime::AppHandle,
    graph_id: String,
    dry_run: bool,
    consent: ConsentGate,
    cancel: Arc<AtomicBool>,
    trigger: &'static str,
    hooks: CycleHooks,
) -> Result<Value, String> {
    let claim = InFlight::claim(&graph_id).ok_or("a dream cycle is already running for this graph")?;
    // Hold the cell open for the whole cycle, dreamer wait included: a
    // headless cell exits when idle, and an idle drain sees only leases.
    let maintenance = match crate::cell_lifecycle::process_lifecycle() {
        Some(lifecycle) => Some(lifecycle.begin_maintenance("dream-cycle").map_err(|_| "the cell is shutting down")?),
        None => None,
    };
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    // Under the claim: a hand-run cycle that just finished can't slip in
    // between the scheduler's check and this one.
    if trigger == SCHEDULED {
        if let Some(reason) = super::scheduler::scheduled_admission(&graph_dir)? {
            return Ok(json!({ "graphId": graph_id, "status": "not_due", "reason": reason, "trigger": trigger }));
        }
    }
    let mechanics = m::read_mechanics(&app)?;
    let date = chrono::Local::now().date_naive();
    let started_ms = crate::clock::epoch_millis();
    let reservation = Reservation { started_ms, trigger };

    // The reservation, under the graph lease, before anything is read or sent.
    let (fence, dream_wires_cleared) = {
        let _write = graph_write_lease(&app, &graph_id).await?;
        let fence = GraphFence::capture(&app, &graph_id, &graph_dir)?;
        if !dry_run {
            super::scheduler::write_cycle_log(&graph_dir, &reservation.log("running", None))
                .map_err(|e| format!("the dream schedule record can't be written, so the cycle did not start: {e}"))?;
        }
        // Last night's dreamtOf wires go first, before the graph is read
        // (Eschaton, 2026-10-19). A failure doesn't stop the night: the
        // scanners never read dream wires, and the next cycle clears again.
        let cleared = if dry_run {
            Value::Null
        } else {
            match super::dream_wires::clear(&app, &graph_id, &graph_dir).await {
                Ok(removed) => json!({ "removed": removed }),
                Err(error) => {
                    log::warn!("dreaming: could not clear last night's dream wires: {error}");
                    json!({ "error": error })
                }
            }
        };
        (fence, cleared)
    };

    let (work_dir, work_id, work_mechanics, load_hook) = (graph_dir.clone(), graph_id.clone(), mechanics.clone(), hooks.load.clone());
    let prepared_result = crate::app_runtime::async_runtime::spawn_blocking(move || {
        let mut loaded = match load_hook {
            Some(hook) => hook(&work_dir, &work_id)?,
            None => load(&work_dir, &work_id)?,
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut prepared = prepare(&loaded, &work_mechanics, date, now_ms);
        // cloud-1 never let suppression go: a small graph whose documents
        // were all dreamed last night would stay empty forever.
        let starved = matches!(prepared.status, "no_findings" | "no_bundles");
        let cleared = starved && !loaded.state.suppressed_doc_ids.is_empty();
        if cleared {
            loaded.state = DreamState::default();
            prepared = prepare(&loaded, &work_mechanics, date, now_ms);
        }
        Ok::<_, String>((loaded, prepared, cleared))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let (loaded, prepared, suppression_cleared) = match prepared_result {
        Ok(done) => done,
        Err(error) => {
            if !dry_run {
                settle_log(&app, &graph_id, &graph_dir, &fence, reservation.log("failed", None)).await;
            }
            return Err(error);
        }
    };
    let prepared_docs: Arc<BTreeSet<String>> =
        Arc::new(prepared.bundles.iter().flat_map(|b| b.findings.iter().flat_map(|r| r.finding.doc_ids())).collect());
    let gate = cycle_gate(consent, graph_dir.clone(), graph_id.clone(), prepared_docs);

    let base = |status: &str, articulated: &[Articulated]| {
        let mut receipt = cycle_receipt(&graph_id, date, &mechanics, &prepared, articulated, status, None);
        receipt["trigger"] = json!(trigger);
        if !dream_wires_cleared.is_null() {
            receipt["dreamWiresCleared"] = dream_wires_cleared.clone();
        }
        if suppression_cleared {
            receipt["suppressionCleared"] = json!(true);
        }
        receipt
    };

    if dry_run {
        let mut receipt = base(prepared.status, &[]);
        receipt["dryRun"] = json!(true);
        receipt["system"] = json!(mechanics.dreamer_prompt);
        receipt["prompts"] = json!(prepared.prompts);
        return Ok(receipt);
    }
    if prepared.status != "prepared" {
        return Ok(settle(&app, &graph_id, &graph_dir, &fence, base(prepared.status, &[]), reservation).await);
    }
    let journal = journal_doc_id(&graph_id);
    if journal_blocked(&graph_dir, &journal)? {
        let mut receipt = base("journal_blocked", &[]);
        receipt["error"] = json!(format!("an ordinary document already has the journal's id ({journal}); the dreamer was not asked"));
        return Ok(settle(&app, &graph_id, &graph_dir, &fence, receipt, reservation).await);
    }
    let articulated = match &hooks.articulate {
        Some(hook) => hook(prepared.prompts.clone(), gate.clone()).await,
        None => {
            let dreamer = match crate::dreamer::read_dreamer_config(&app) {
                Ok(DreamerConfig::None) => Err("choose a dreamer before dreaming".to_string()),
                other => other,
            };
            let dreamer = match dreamer {
                Ok(dreamer) => dreamer,
                Err(error) => {
                    settle_log(&app, &graph_id, &graph_dir, &fence, reservation.log("failed", None)).await;
                    return Err(error);
                }
            };
            articulate(&app, &dreamer, &mechanics, &prepared, gate.clone(), cancel.clone()).await
        }
    };
    if cancel.load(Ordering::SeqCst) {
        // A cancelled cycle leaves last night's journal and memory as they were.
        return Ok(settle(&app, &graph_id, &graph_dir, &fence, base("cancelled", &articulated), reservation).await);
    }
    let entries = journal_entries(&prepared, &articulated);
    if entries.is_empty() {
        // cloud-1 left the last journal in place when every dream was vetoed.
        return Ok(settle(&app, &graph_id, &graph_dir, &fence, base("all_vetoed", &articulated), reservation).await);
    }
    let markdown = journal_markdown(&entries, &journal_date_label(date), prepared.context.as_ref(), &prepared.titles);
    let dream_wires = super::dream_wires::wire_payloads(&journal, &entries, chrono::Utc::now().timestamp_millis());
    let mut template = base("publishing", &articulated);
    template["journalMarkdown"] = json!(markdown);
    let publication = Publication {
        app: app.clone(),
        graph_id: graph_id.clone(),
        graph_dir: graph_dir.clone(),
        markdown,
        block_prefix: format!("dream-{}-{:08x}", date.format("%Y%m%d"), prepared.seed as u32),
        state: next_state(&entries, date),
        dream_wires,
        parent: journal_parent(&loaded, &journal),
        fence,
        gate,
        reservation,
        before_publish: hooks.before_publish.clone(),
    };
    // The worker owns everything that keeps others out of this graph; if
    // this future is dropped, they are released only when it finishes.
    let worker = crate::app_runtime::async_runtime::spawn_blocking(move || {
        let _claim = claim;
        let _maintenance = maintenance;
        publication.run(template)
    });
    worker.await.map_err(|e| format!("the publication worker failed: {e}"))
}

/// When a cycle started and under which trigger, for the schedule record.
#[derive(Clone, Copy)]
struct Reservation {
    started_ms: u128,
    trigger: &'static str,
}

impl Reservation {
    fn log(&self, status: &str, job: Option<String>) -> super::scheduler::CycleLog {
        super::scheduler::CycleLog {
            last_started_ms: Some(self.started_ms as i64),
            last_trigger: self.trigger.to_string(),
            last_status: status.to_string(),
            last_job_id: job,
        }
    }
}

/// Everything publication needs, owned, so it can run on a blocking worker.
struct Publication {
    app: crate::app_runtime::AppHandle,
    graph_id: String,
    graph_dir: PathBuf,
    markdown: String,
    block_prefix: String,
    state: DreamState,
    /// tonight's dreamtOf wires, written after the journal.
    dream_wires: Vec<(String, serde_json::Map<String, Value>)>,
    parent: Option<&'static str>,
    fence: GraphFence,
    gate: ConsentGate,
    reservation: Reservation,
    before_publish: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Publication {
    /// Under the graph write lease for its whole length: the fence, the gate
    /// (consent and exclusions, linearized with their setters, which take the
    /// same lease), then the journal and every aux file.
    fn run(self, mut receipt: Value) -> Value {
        let lease = crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(&self.app, &self.graph_id);
        let _write = match lease {
            Ok(write) => write,
            Err(error) => {
                receipt["status"] = json!("graph_changed");
                receipt["error"] = json!(error);
                return finish_unpublished(&self.app, &self.graph_id, self.reservation.started_ms, receipt);
            }
        };
        if let Err(error) = self.fence.verify(&self.app, &self.graph_id, &self.graph_dir) {
            // Nothing is written into a graph that was deleted, replaced or
            // restored while the dreamer worked; the job receipt keeps the dreams.
            receipt["status"] = json!("graph_changed");
            receipt["error"] = json!(error);
            return finish_unpublished(&self.app, &self.graph_id, self.reservation.started_ms, receipt);
        }
        if let Err(reason) = (self.gate)() {
            // Consent withdrawn or a dreamed document excluded after the last
            // send: the night is discarded (Eschaton, 2026-10-07). The receipt
            // keeps the paid-for dreams.
            receipt["status"] = json!("withdrawn");
            receipt["withdrawnBecause"] = json!(reason);
            return settle_locked(&self.app, &self.graph_id, &self.graph_dir, receipt, self.reservation);
        }
        if let Some(hook) = &self.before_publish {
            hook();
        }
        match write_results(&self.app, &self.graph_dir, &self.graph_id, &self.markdown, &self.block_prefix, &self.state, self.parent) {
            Ok(journal) => {
                receipt["status"] = json!("completed");
                receipt["journal"] = json!(journal);
                // The journal stands on its own; its links are a
                // convenience, so a failure is reported, not fatal.
                let now_ms = chrono::Utc::now().timestamp_millis();
                receipt["dreamWires"] =
                    match super::dream_wires::publish(&self.app, &self.graph_id, &self.graph_dir, &self.dream_wires, now_ms) {
                        Ok(created) => json!({ "created": created }),
                        Err(error) => {
                            log::warn!("dreaming: the journal's dream wires were not written: {error}");
                            json!({ "error": error })
                        }
                    };
                if let Some(map) = receipt.as_object_mut() {
                    map.remove("journalMarkdown");
                }
            }
            Err(error) => {
                // The dreams were paid for: the receipt keeps them.
                receipt["status"] = json!("write_failed");
                receipt["error"] = json!(error);
            }
        }
        settle_locked(&self.app, &self.graph_id, &self.graph_dir, receipt, self.reservation)
    }
}

/// Keep a receipt beside the graph and in the job registry, and close the
/// schedule record, with the write lease already held and the fence checked.
fn settle_locked(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    receipt: Value,
    reservation: Reservation,
) -> Value {
    let (receipt, job) = with_job(app, graph_id, reservation.started_ms, receipt);
    keep_receipt(graph_dir, &receipt);
    let status = receipt["status"].as_str().unwrap_or("unknown");
    if let Err(error) = super::scheduler::write_cycle_log(graph_dir, &reservation.log(status, job)) {
        log::warn!("dreaming: could not close the schedule record: {error}");
    }
    receipt
}

/// Take the write lease and check the fence, then settle. If the graph
/// changed, nothing is written into it; the job registry still keeps the
/// receipt. No await sits between the lease and the writes.
async fn settle(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    fence: &GraphFence,
    receipt: Value,
    reservation: Reservation,
) -> Value {
    let Ok(write) = graph_write_lease(app, graph_id).await else {
        return finish_unpublished(app, graph_id, reservation.started_ms, receipt);
    };
    if fence.verify(app, graph_id, graph_dir).is_err() {
        drop(write);
        return finish_unpublished(app, graph_id, reservation.started_ms, receipt);
    }
    let receipt = settle_locked(app, graph_id, graph_dir, receipt, reservation);
    drop(write);
    receipt
}

/// Close the schedule record after a failure, if the graph is still the one
/// the cycle reserved.
async fn settle_log(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    fence: &GraphFence,
    log: super::scheduler::CycleLog,
) {
    let Ok(_write) = graph_write_lease(app, graph_id).await else { return };
    if fence.verify(app, graph_id, graph_dir).is_ok() {
        if let Err(error) = super::scheduler::write_cycle_log(graph_dir, &log) {
            log::warn!("dreaming: could not close the schedule record: {error}");
        }
    }
}

/// A receipt for a graph the cycle may no longer write into: the job
/// registry (in the profile, not the graph) keeps it.
fn finish_unpublished(app: &crate::app_runtime::AppHandle, graph_id: &str, started_ms: u128, receipt: Value) -> Value {
    let mut receipt = receipt;
    receipt["graphWritesSkipped"] = json!(true);
    with_job(app, graph_id, started_ms, receipt).0
}

/// A small synthetic graph for tests: five four-document wire chains, a..t.
#[cfg(test)]
pub(crate) fn synthetic(graph_id: &str, state: DreamState, excluded: &[&str]) -> Loaded {
    use crate::semantic_index::{SemanticIndexFile, SemanticIndexManifest};
    // Wire chains: enough two-hop paths for wire inference to fill a
    // dream on two nights running.
    let chain = |docs: &[&str]| -> Vec<Value> {
        docs.windows(2).enumerate().map(|(i, w)| json!({
            "id": format!("w-{}-{i}", w[0]), "sourceDocumentId": w[0], "targetDocumentId": w[1],
            "predicate": "http://mnemosyne.ai/vocab#flowsInto",
        })).collect()
    };
    let mut wires = Vec::new();
    for c in ["abcd", "efgh", "ijkl", "mnop", "qrst"] {
        wires.extend(chain(&c.chars().map(|x| x.to_string()).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>()));
    }
    let documents = "abcdefghijklmnopqrst".chars().map(|c| json!({ "id": c.to_string(), "title": format!("Doc {c}") })).collect();
    Loaded {
        graph_id: graph_id.into(),
        index: Arc::new(SemanticIndexFile {
            manifest: SemanticIndexManifest {
                generation: String::new(), schema_version: 1, graph_id: graph_id.into(), provider_id: "test".into(),
                model_id: "test".into(), dimensions: 4, block_count: 0, document_count: 0,
                indexed_at: String::new(), index_path: String::new(),
            },
            blocks: Vec::new(),
            entities: Vec::new(),
        }),
        documents,
        folders: Vec::new(),
        wires,
        values: Vec::new(),
        derived: HashSet::new(),
        excluded: excluded.iter().map(|d| d.to_string()).collect(),
        doc_edited: HashMap::new(),
        settings: Default::default(),
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreamer::AppPreset;

    const NOW: i64 = 1_791_000_000_000;

    fn synthetic(state: DreamState, excluded: &[&str]) -> Loaded {
        super::synthetic("g", state, excluded)
    }

    fn dreamed_docs(prepared: &Prepared) -> BTreeSet<String> {
        prepared.bundles.iter().flat_map(|b| b.findings.iter().flat_map(|r| r.finding.doc_ids())).collect()
    }

    #[test]
    fn last_nights_documents_stay_out_of_tonights_dreams() {
        let date: NaiveDate = "2026-10-02".parse().unwrap();
        let first = prepare(&synthetic(DreamState::default(), &[]), &Mechanics::default(), date, NOW);
        assert_eq!(first.status, "prepared");
        let first_docs = dreamed_docs(&first);
        assert!(first_docs.contains("c"), "{first_docs:?}");
        // Tomorrow, with tonight's documents remembered, none of them returns.
        let entries: Vec<JournalEntry> = first.bundles.iter().map(|b| JournalEntry {
            narrative: "x".into(), confidence: 0.8, is_navel: false, total_charge: b.total_charge, findings: b.findings.clone(),
        }).collect();
        let state = next_state(&entries, date);
        assert_eq!(state.suppressed_doc_ids.iter().cloned().collect::<BTreeSet<_>>(), first_docs);
        let second = prepare(&synthetic(state, &[]), &Mechanics::default(), date.succ_opt().unwrap(), NOW);
        let second_docs = dreamed_docs(&second);
        assert!(!second_docs.is_empty(), "the other chain still dreams");
        assert!(second_docs.is_disjoint(&first_docs), "{second_docs:?} repeats {first_docs:?}");
    }

    #[test]
    fn excluded_documents_never_reach_a_dream() {
        let date: NaiveDate = "2026-10-02".parse().unwrap();
        let prepared = prepare(&synthetic(DreamState::default(), &["c", "j"]), &Mechanics::default(), date, NOW);
        let docs = dreamed_docs(&prepared);
        assert!(!docs.is_empty());
        assert!(!docs.contains("c") && !docs.contains("j"), "{docs:?}");
        assert_eq!(prepared.counts["excludedByPerson"], 2);
    }

    #[test]
    fn a_recent_document_takes_the_residue_slot() {
        let date: NaiveDate = "2026-10-02".parse().unwrap();
        let day = super::super::temporal::DAY_MS as i64;
        let mut loaded = synthetic(DreamState::default(), &[]);
        // One document touched yesterday; the rest long ago.
        for doc in "abcdefghijklmnopqrst".chars() {
            loaded.doc_edited.insert(doc.to_string(), NOW - 400 * day);
        }
        loaded.doc_edited.insert("c".into(), NOW - day);
        let prepared = prepare(&loaded, &Mechanics::default(), date, NOW);
        assert_eq!(prepared.status, "prepared");
        // Dreams are ordered by charge, so the residue dream may be any of them.
        let with_residue: Vec<&Bundle> = prepared.bundles.iter().filter(|b| b.residue_slot).collect();
        assert_eq!(with_residue.len(), 1);
        let slot = &with_residue[0].findings[0];
        assert!(slot.residue);
        assert!(slot.finding.doc_ids().contains("c"), "{:?}", slot.finding.doc_ids());
        assert_eq!(prepared.counts["residueSlotsFilled"], 1);
        // A window shorter than a day leaves the slot empty.
        loaded.settings.residue_days = Some(0.5);
        let short = prepare(&loaded, &Mechanics::default(), date, NOW);
        assert!(short.bundles.iter().all(|b| !b.residue_slot));
        // 0 turns the slot off: cloud-1's weaver.
        loaded.settings.residue_days = Some(0.0);
        let off = prepare(&loaded, &Mechanics::default(), date, NOW);
        assert_eq!(off.counts["residueCandidates"], 0);
    }

    #[test]
    fn excluding_a_dreamed_document_closes_the_gate_for_the_rest_of_the_cycle() {
        let dir = std::env::temp_dir().join(format!("dream-cgate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("graph.json"), r#"{"graphId":"g","title":"g","origin":"local","providerId":"local","localPath":"","createdAt":"0","updatedAt":"0","capabilities":[]}"#).unwrap();
        let open: ConsentGate = Arc::new(|| Ok("test".to_string()));
        let dreamed: Arc<BTreeSet<String>> = Arc::new(["c".to_string(), "d".to_string()].into());
        let gate = cycle_gate(open.clone(), dir.clone(), "g".into(), dreamed);
        assert!(gate().is_ok());
        // Excluding something tonight's dreams don't use leaves the gate open.
        let excl = |docs: &[&str]| super::super::exclusions::Exclusions { folders: vec![], documents: docs.iter().map(|d| d.to_string()).collect() };
        super::super::exclusions::write(&dir, &excl(&["z"])).unwrap();
        assert!(gate().is_ok());
        // Excluding a dreamed document while the dreamer works closes it.
        super::super::exclusions::write(&dir, &excl(&["z", "d"])).unwrap();
        assert!(gate().is_err());
        // A broken file closes it too.
        std::fs::write(super::super::exclusions::exclusions_path(&dir), "{").unwrap();
        assert!(gate().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real cycle against a real graph directory, read-only: load, prepare,
    /// and (when GARDEN_DREAM_LIVE_DREAMER names one) ask that dreamer, then
    /// print the journal it would write. Nothing is written anywhere.
    ///   GARDEN_DREAM_LIVE_GRAPH_DIR=".../graphs/default_x1" GARDEN_DREAM_LIVE_DREAMER=kimi \
    ///   cargo test --release --no-default-features --features headless --lib -- --ignored live_dream --nocapture
    #[test]
    #[ignore]
    fn live_dream_from_a_real_graph() {
        let Ok(dir) = std::env::var("GARDEN_DREAM_LIVE_GRAPH_DIR") else { return };
        let graph_dir = PathBuf::from(dir);
        let graph_id = graph_dir.file_name().unwrap().to_string_lossy().to_string();
        let started = std::time::Instant::now();
        let loaded = load(&graph_dir, &graph_id).expect("load");
        eprintln!("loaded in {:?}: {} index blocks, {} documents, {} wires, {} valued, {} derived views",
            started.elapsed(), loaded.index.blocks.len(), loaded.documents.len(), loaded.wires.len(),
            loaded.values.len(), loaded.derived.len());
        let mechanics = Mechanics::default();
        let date = std::env::var("GARDEN_DREAM_LIVE_DATE").ok().and_then(|d| d.parse().ok())
            .unwrap_or_else(|| chrono::Local::now().date_naive());
        let started = std::time::Instant::now();
        let prepared = prepare(&loaded, &mechanics, date, chrono::Utc::now().timestamp_millis());
        eprintln!("prepared in {:?}: {}", started.elapsed(),
            serde_json::to_string_pretty(&cycle_receipt(&graph_id, date, &mechanics, &prepared, &[], prepared.status, None)).unwrap());
        for (i, prompt) in prepared.prompts.iter().enumerate() {
            eprintln!("\n===== PROMPT {} =====\n{prompt}", i + 1);
        }
        let dreamer = match std::env::var("GARDEN_DREAM_LIVE_DREAMER").as_deref() {
            Ok("kimi") => DreamerConfig::App { preset: AppPreset::Kimi, program: None, model: None, args: vec![], accept_reduced_isolation: false },
            Ok("claude") => DreamerConfig::App { preset: AppPreset::Claude, program: None, model: Some("haiku".into()), args: vec![], accept_reduced_isolation: false },
            _ => return,
        };
        let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = std::env::temp_dir().join(format!("dream-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&profile).unwrap();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let consent: ConsentGate = Arc::new(|| Ok("live test".to_string()));
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let articulated = runtime.block_on(articulate(&app, &dreamer, &mechanics, &prepared, consent, Arc::new(AtomicBool::new(false))));
        let entries = journal_entries(&prepared, &articulated);
        let receipt = cycle_receipt(&graph_id, date, &mechanics, &prepared, &articulated, "live", None);
        eprintln!("\n===== DREAMS =====\n{}", serde_json::to_string_pretty(&receipt["dreams"]).unwrap());
        eprintln!("\n===== JOURNAL =====\n{}", journal_markdown(&entries, &journal_date_label(date), prepared.context.as_ref(), &prepared.titles));
        let _ = std::fs::remove_dir_all(&profile);
    }
}
