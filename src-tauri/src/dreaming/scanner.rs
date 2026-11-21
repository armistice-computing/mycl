//! The four dreamwork scans, ported from cloud-1's `scanner.py`:
//! displacement, condensation, wire inference and structural isomorphism.
//! Each produces candidates only; the Dreamer decides what they mean.
//!
//! Where cloud-1 asked Qdrant, these ask the graph's own semantic index in
//! memory: a "search" is an exact top-k over the index's unit vectors, with
//! scores put through the cycle's [`Calibration`] so cloud-1's thresholds
//! keep their meaning. Differences from cloud-1, each deliberate:
//! - A block's stored vector is its query vector. cloud-1 re-embedded the
//!   block's 200-character preview with the "clustering" task prefix.
//! - Condensation samples the whole index uniformly with the cycle's seed.
//!   cloud-1 took the first N points of a Qdrant scroll (the same oldest
//!   blocks every night) and shuffled those.
//! - A document's semantic centroid uses its first five blocks in document
//!   order. cloud-1 took whichever five points Qdrant returned first.

use super::calibration::{dot, Calibration};
use super::mechanics as m;
use super::rng::SplitMix64;
use super::wire_graph::{self, MatchedPair, WireEdge};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// cloud-1 stored a 200-character `text_preview` in Qdrant; every text a
/// scan carries is that long at most.
const PREVIEW_CHARS: usize = 200;

pub(crate) fn preview(text: &str) -> String {
    text.chars().take(PREVIEW_CHARS).collect()
}

#[derive(Debug, Clone)]
pub(crate) struct IndexedBlock<'a> {
    pub(crate) doc: &'a str,
    pub(crate) block: &'a str,
    pub(crate) content: &'a str,
    pub(crate) order: f64,
    pub(crate) vector: &'a [f32],
    /// The temporal curve at this block (0..1).
    pub(crate) temporal: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct ValuedBlock {
    pub(crate) block_id: String,
    pub(crate) doc_id: String,
    pub(crate) importance: f64,
    pub(crate) valence: f64,
    /// When it was last valued (epoch ms), for the temporal curve.
    pub(crate) last_valued_ms: Option<i64>,
    /// The temporal curve at this block (0..1), set by the cycle.
    pub(crate) temporal: f64,
    /// Touched within the graph's residue window, set by the cycle.
    pub(crate) recent: bool,
}

pub(crate) struct ScanInput<'a> {
    /// Index blocks eligible to appear in a dream (excluded documents removed).
    pub(crate) blocks: Vec<IndexedBlock<'a>>,
    pub(crate) calibration: Calibration,
    pub(crate) valued: Vec<ValuedBlock>,
    pub(crate) edges: Vec<WireEdge>,
    pub(crate) titles: HashMap<String, String>,
    pub(crate) seed: u64,
}

// Block ids and rule notes ride along for receipts and source links,
// whether or not today's prompt and journal print them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct Displacement {
    pub(crate) source_block: String,
    pub(crate) source_doc: String,
    pub(crate) source_text: String,
    pub(crate) source_importance: f64,
    pub(crate) neighbor_block: String,
    pub(crate) neighbor_doc: String,
    pub(crate) neighbor_text: String,
    pub(crate) similarity: f64,
    pub(crate) intermediary: Option<(String, String, String)>,
    pub(crate) hops: u32,
}

// Block ids and rule notes ride along for receipts and source links,
// whether or not today's prompt and journal print them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct CondensedBlock {
    pub(crate) block: String,
    pub(crate) doc: String,
    pub(crate) text: String,
}

// Block ids and rule notes ride along for receipts and source links,
// whether or not today's prompt and journal print them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct Condensation {
    pub(crate) blocks: Vec<CondensedBlock>,
    pub(crate) centroid_text: String,
    pub(crate) avg_similarity: f64,
}

// Block ids and rule notes ride along for receipts and source links,
// whether or not today's prompt and journal print them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct WireInference {
    pub(crate) docs: [String; 3],
    pub(crate) titles: [String; 3],
    pub(crate) predicate_ab: String,
    pub(crate) predicate_bc: String,
    pub(crate) inferred_predicate: String,
    pub(crate) structural_confidence: f64,
    pub(crate) requires_llm_judgment: bool,
    pub(crate) composition_note: String,
    pub(crate) block_a: String,
    pub(crate) block_b: String,
    pub(crate) block_c: String,
    pub(crate) block_a_text: String,
    pub(crate) block_c_text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Isomorphism {
    pub(crate) doc_a: String,
    pub(crate) doc_b: String,
    pub(crate) title_a: String,
    pub(crate) title_b: String,
    pub(crate) structural_similarity: f64,
    pub(crate) semantic_distance: f64,
    pub(crate) score: f64,
    pub(crate) matched: Vec<MatchedPair>,
}

#[derive(Debug, Clone)]
pub(crate) enum Finding {
    Displacement(Displacement),
    Condensation(Condensation),
    WireInference(WireInference),
    Isomorphism(Isomorphism),
}

impl Finding {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Finding::Displacement(_) => "displacement",
            Finding::Condensation(_) => "condensation",
            Finding::WireInference(_) => "wire_inference",
            Finding::Isomorphism(_) => "isomorphism",
        }
    }

    pub(crate) fn doc_ids(&self) -> BTreeSet<String> {
        match self {
            Finding::Displacement(f) => {
                let mut docs = BTreeSet::from([f.source_doc.clone(), f.neighbor_doc.clone()]);
                if let Some((_, doc, _)) = &f.intermediary {
                    docs.insert(doc.clone());
                }
                docs
            }
            Finding::Condensation(f) => f.blocks.iter().map(|b| b.doc.clone()).filter(|d| !d.is_empty()).collect(),
            Finding::WireInference(f) => f.docs.iter().cloned().collect(),
            Finding::Isomorphism(f) => BTreeSet::from([f.doc_a.clone(), f.doc_b.clone()]),
        }
    }

    /// What this finding drew on, as (document, block): a block when the
    /// finding names one, the document alone for document-level findings
    /// (isomorphisms, or a wire inference step with no block).
    pub(crate) fn sources(&self) -> BTreeSet<(String, Option<String>)> {
        let at = |doc: &str, block: &str| {
            (doc.to_string(), Some(block.to_string()).filter(|b| !b.is_empty()))
        };
        let mut sources: BTreeSet<(String, Option<String>)> = match self {
            Finding::Displacement(f) => {
                let mut s = BTreeSet::from([at(&f.source_doc, &f.source_block), at(&f.neighbor_doc, &f.neighbor_block)]);
                if let Some((block, doc, _)) = &f.intermediary {
                    s.insert(at(doc, block));
                }
                s
            }
            Finding::Condensation(f) => f.blocks.iter().map(|b| at(&b.doc, &b.block)).collect(),
            Finding::WireInference(f) => {
                [(&f.docs[0], &f.block_a), (&f.docs[1], &f.block_b), (&f.docs[2], &f.block_c)].into_iter().map(|(d, b)| at(d, b)).collect()
            }
            Finding::Isomorphism(f) => BTreeSet::from([(f.doc_a.clone(), None), (f.doc_b.clone(), None)]),
        };
        sources.retain(|(doc, _)| !doc.is_empty());
        sources
    }
}

#[derive(Debug, Default)]
pub(crate) struct ScanResults {
    pub(crate) displacements: Vec<Finding>,
    pub(crate) condensations: Vec<Finding>,
    pub(crate) wire_inferences: Vec<Finding>,
    pub(crate) isomorphisms: Vec<Finding>,
}

impl ScanResults {
    pub(crate) fn total(&self) -> usize {
        self.displacements.len() + self.condensations.len() + self.wire_inferences.len() + self.isomorphisms.len()
    }

    /// Keep only findings whose documents all pass `keep`.
    pub(crate) fn retain_docs(&mut self, keep: impl Fn(&str) -> bool) {
        for list in [&mut self.displacements, &mut self.condensations, &mut self.wire_inferences, &mut self.isomorphisms] {
            list.retain(|f| f.doc_ids().iter().all(|d| keep(d)));
        }
    }
}

/// The scan budgets for one cycle at `scale` (cloud-1's dream_handler).
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Budgets {
    pub(crate) displacement_top_k: usize,
    pub(crate) condensation_sample: usize,
    pub(crate) wire_inference_paths: usize,
    pub(crate) isomorphism_max: usize,
}

pub(crate) fn budgets(scale: f64) -> Budgets {
    Budgets {
        displacement_top_k: ((m::DISPLACEMENT_TOP_K * scale).ceil() as usize).max(1),
        condensation_sample: ((m::CONDENSATION_SAMPLE_SIZE * scale).ceil() as usize).max(10),
        wire_inference_paths: ((m::WIRE_INFERENCE_MAX_PATHS * scale).ceil() as usize).max(5),
        isomorphism_max: ((m::ISOMORPHISM_MAX_FINDINGS * scale.min(1.5)).ceil() as usize).max(1),
    }
}

struct Hit {
    index: usize,
    score: f64,
}

impl<'a> ScanInput<'a> {
    /// cloud-1's `vector_store.search(limit, min_score)`: the `limit` best
    /// blocks scoring at least `min_score`, best first, calibrated.
    fn search(&self, query: &[f32], limit: usize, min_score: f64) -> Vec<Hit> {
        let mut raw: Vec<(f32, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.vector.len() == query.len())
            .map(|(i, b)| (dot(query, b.vector), i))
            .collect();
        let take = limit.min(raw.len());
        if take == 0 {
            return Vec::new();
        }
        // Calibration is monotone, so the raw order is the calibrated order.
        raw.select_nth_unstable_by(take - 1, |a, b| b.0.total_cmp(&a.0));
        raw.truncate(take);
        raw.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        raw.into_iter()
            .map(|(s, index)| Hit { index, score: self.calibration.calibrate(s) })
            .filter(|h| h.score >= min_score)
            .collect()
    }

    fn by_block_id(&self) -> HashMap<&'a str, usize> {
        let mut map = HashMap::new();
        for (i, b) in self.blocks.iter().enumerate() {
            map.entry(b.block).or_insert(i);
        }
        map
    }

    fn title(&self, doc: &str) -> String {
        self.titles.get(doc).cloned().unwrap_or_else(|| doc.to_string())
    }
}

/// Displacement V2: charge-sensitive bands and two-hop indirection. The dream
/// points at something near something near the valued thing.
pub(crate) fn scan_displacement(input: &ScanInput, top_k: usize) -> Vec<Finding> {
    let mut seeds: Vec<&ValuedBlock> = input.valued.iter().filter(|b| b.importance >= 3.0).collect();
    // cloud-1 seeded from the most important blocks. The temporal curve
    // nudges that order: a block at the trough counts at half its importance,
    // one at either peak at full (Eschaton, 2026-10-02).
    let nudged = |b: &ValuedBlock| b.importance * (0.5 + 0.5 * b.temporal);
    seeds.sort_by(|a, b| nudged(b).total_cmp(&nudged(a)));
    seeds.truncate(top_k);
    // The day's residue: a few recently touched valued blocks seed too,
    // whatever their importance, and go first so the per-document cap does
    // not spend their documents on older seeds.
    let mut residue: Vec<&ValuedBlock> = input
        .valued
        .iter()
        .filter(|b| b.recent && b.importance > 0.0 && !seeds.iter().any(|s| s.block_id == b.block_id))
        .collect();
    residue.sort_by(|a, b| b.importance.total_cmp(&a.importance).then_with(|| a.block_id.cmp(&b.block_id)));
    residue.truncate(m::RESIDUE_SEEDS);
    let seeds: Vec<&ValuedBlock> = residue.into_iter().chain(seeds).collect();
    let valued_ids: HashSet<&str> = input.valued.iter().filter(|b| b.importance > 0.0).map(|b| b.block_id.as_str()).collect();
    let by_id = input.by_block_id();
    let mut doc_counts: HashMap<String, usize> = HashMap::new();
    let mut findings = Vec::new();
    let capped = |counts: &HashMap<String, usize>, a: &str, b: &str| {
        m::SCANNER_MAX_PER_DOC > 0
            && (counts.get(a).copied().unwrap_or(0) >= m::SCANNER_MAX_PER_DOC
                || counts.get(b).copied().unwrap_or(0) >= m::SCANNER_MAX_PER_DOC)
    };

    for seed in seeds {
        let Some(&seed_index) = by_id.get(seed.block_id.as_str()) else { continue };
        let seed_block = &input.blocks[seed_index];
        if seed_block.content.trim().is_empty() {
            continue;
        }
        let (floor, ceil) = if seed.valence < 0.0 { m::DISPLACEMENT_NEGATIVE_BAND } else { m::DISPLACEMENT_POSITIVE_BAND };
        let band: Vec<Hit> = input
            .search(seed_block.vector, m::DISPLACEMENT_NEIGHBORS * 3, floor)
            .into_iter()
            .filter(|h| input.blocks[h.index].block != seed.block_id && h.score <= ceil && h.score <= m::MAX_SIMILARITY)
            .take(m::DISPLACEMENT_NEIGHBORS)
            .collect();
        for pen in band {
            let pen_block = &input.blocks[pen.index];
            if pen_block.content.trim().is_empty() {
                continue;
            }
            let displaced = input.search(pen_block.vector, m::DISPLACEMENT_NEIGHBORS + 2, m::DISPLACEMENT_TWO_HOP_FLOOR);
            for hit in displaced {
                let target = &input.blocks[hit.index];
                if valued_ids.contains(target.block)
                    || target.block == pen_block.block
                    || target.block == seed.block_id
                    || hit.score > m::MAX_SIMILARITY
                {
                    continue;
                }
                if capped(&doc_counts, &seed.doc_id, target.doc) {
                    continue;
                }
                findings.push(Finding::Displacement(Displacement {
                    source_block: seed.block_id.clone(),
                    source_doc: seed.doc_id.clone(),
                    source_text: preview(seed_block.content),
                    source_importance: seed.importance,
                    neighbor_block: target.block.to_string(),
                    neighbor_doc: target.doc.to_string(),
                    neighbor_text: preview(target.content),
                    similarity: hit.score,
                    intermediary: Some((pen_block.block.to_string(), pen_block.doc.to_string(), preview(pen_block.content))),
                    hops: 2,
                }));
                *doc_counts.entry(seed.doc_id.clone()).or_insert(0) += 1;
                *doc_counts.entry(target.doc.to_string()).or_insert(0) += 1;
                break; // one displaced target per penumbra neighbor
            }
        }
    }
    findings
}

/// Clusters of similar blocks spread across documents.
pub(crate) fn scan_condensation(input: &ScanInput, sample_size: usize) -> Vec<Finding> {
    // A weighted sample without replacement (Efraimidis-Spirakis): each block
    // draws u^(1/w), w = 0.5 + 0.5 x its temporal value, and the largest keys
    // win. A block at either peak is twice as likely as one at the trough.
    let mut rng = SplitMix64::new(input.seed ^ 0xc0de);
    let mut keyed: Vec<(f64, usize)> = input
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let u = ((rng.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
            (u.powf(1.0 / (0.5 + 0.5 * b.temporal.clamp(0.0, 1.0))), i)
        })
        .collect();
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0));
    let order: Vec<usize> = keyed.into_iter().take(sample_size).map(|(_, i)| i).collect();
    if order.len() < m::CONDENSATION_CLUSTER_MIN {
        return Vec::new();
    }
    let mut seen: HashSet<BTreeSet<&str>> = HashSet::new();
    let mut findings = Vec::new();
    for &point in order.iter().take(sample_size / 2) {
        let center = &input.blocks[point];
        let hits: Vec<Hit> = input
            .search(center.vector, m::CONDENSATION_CLUSTER_MIN + 5, m::CONDENSATION_SIMILARITY)
            .into_iter()
            .filter(|h| h.score <= m::MAX_SIMILARITY)
            .collect();
        if hits.len() < m::CONDENSATION_CLUSTER_MIN {
            continue;
        }
        let docs: BTreeSet<&str> = hits.iter().map(|h| input.blocks[h.index].doc).collect();
        if docs.len() < 2 {
            continue;
        }
        let key: BTreeSet<&str> = hits.iter().map(|h| input.blocks[h.index].block).collect();
        if !seen.insert(key) {
            continue;
        }
        findings.push(Finding::Condensation(Condensation {
            blocks: hits
                .iter()
                .map(|h| {
                    let b = &input.blocks[h.index];
                    CondensedBlock { block: b.block.to_string(), doc: b.doc.to_string(), text: preview(b.content) }
                })
                .collect(),
            centroid_text: preview(center.content),
            avg_similarity: hits.iter().map(|h| h.score).sum::<f64>() / hits.len() as f64,
        }));
    }
    findings
}

/// Two-hop wire paths with an unstated implication, rule-backed first.
pub(crate) fn scan_wire_inference(input: &ScanInput, max_paths: usize) -> Vec<Finding> {
    let paths = wire_graph::two_hop_paths(&input.edges, max_paths * 3);
    let mut with_rules: Vec<_> = paths.iter().filter(|p| p.composition.is_some()).collect();
    with_rules.sort_by(|a, b| b.composition.unwrap().confidence.total_cmp(&a.composition.unwrap().confidence));
    let mut selected: Vec<_> = with_rules.into_iter().take(max_paths).collect();
    let remaining = max_paths.saturating_sub(selected.len());
    selected.extend(paths.iter().filter(|p| p.composition.is_none()).take(remaining));

    let by_id = input.by_block_id();
    let text_of = |block: &Option<String>| {
        block.as_deref().and_then(|id| by_id.get(id)).map(|&i| preview(input.blocks[i].content)).unwrap_or_default()
    };
    let mut doc_counts: HashMap<&str, usize> = HashMap::new();
    let mut findings = Vec::new();
    for path in selected {
        let docs = [path.ab.source_doc.as_str(), path.ab.target_doc.as_str(), path.bc.target_doc.as_str()];
        if m::SCANNER_MAX_PER_DOC > 0 && docs.iter().any(|d| doc_counts.get(d).copied().unwrap_or(0) >= m::SCANNER_MAX_PER_DOC) {
            continue;
        }
        let rule = path.composition;
        findings.push(Finding::WireInference(WireInference {
            docs: docs.map(str::to_string),
            titles: docs.map(|d| input.title(d)),
            predicate_ab: path.ab.predicate.clone(),
            predicate_bc: path.bc.predicate.clone(),
            inferred_predicate: rule.map(|r| r.inferred_predicate.to_string()).unwrap_or_else(|| "unknown".into()),
            structural_confidence: rule.map(|r| r.confidence).unwrap_or(0.1),
            requires_llm_judgment: rule.map(|r| r.requires_llm_judgment).unwrap_or(true),
            composition_note: rule.map(|r| r.note.to_string()).unwrap_or_else(|| {
                format!("No composition rule for ({}, {}). LLM should evaluate freely.", path.ab.predicate, path.bc.predicate)
            }),
            block_a: path.ab.source_block.clone().unwrap_or_default(),
            block_b: path.ab.target_block.clone().or_else(|| path.bc.source_block.clone()).unwrap_or_default(),
            block_c: path.bc.target_block.clone().unwrap_or_default(),
            block_a_text: text_of(&path.ab.source_block),
            block_c_text: text_of(&path.bc.target_block),
        }));
        if m::SCANNER_MAX_PER_DOC > 0 {
            for d in docs {
                *doc_counts.entry(d).or_insert(0) += 1;
            }
        }
    }
    findings
}

/// The same relational shape in semantically distant places.
pub(crate) fn scan_isomorphisms(input: &ScanInput, max_findings: usize) -> Vec<Finding> {
    let nets = wire_graph::ego_networks(&input.edges);
    let candidates = wire_graph::candidate_isomorphisms(&nets, m::ISOMORPHISM_MIN_SIMILARITY, m::ISOMORPHISM_MIN_EGO_SIZE);
    let mut by_doc: BTreeMap<&str, Vec<&IndexedBlock>> = BTreeMap::new();
    for b in &input.blocks {
        by_doc.entry(b.doc).or_default().push(b);
    }
    let centroids: BTreeMap<&str, Vec<f32>> =
        by_doc.iter().filter_map(|(doc, blocks)| Some((*doc, centroid(blocks)?))).collect();
    let calibration = Calibration::measure_centroids(&centroids.values().cloned().collect::<Vec<_>>(), input.seed);
    let mut doc_counts: HashMap<String, usize> = HashMap::new();
    let mut findings: Vec<Isomorphism> = Vec::new();
    for (doc_a, doc_b, structural) in candidates.into_iter().take(max_findings * 2) {
        if m::SCANNER_MAX_PER_DOC > 0
            && (doc_counts.get(&doc_a).copied().unwrap_or(0) >= m::SCANNER_MAX_PER_DOC
                || doc_counts.get(&doc_b).copied().unwrap_or(0) >= m::SCANNER_MAX_PER_DOC)
        {
            continue;
        }
        let distance = semantic_distance(&calibration, centroids.get(doc_a.as_str()), centroids.get(doc_b.as_str()));
        let score = structural * distance;
        if score < m::ISOMORPHISM_MIN_SCORE {
            continue;
        }
        let (matched, _, _) = wire_graph::neighbor_correspondence(&nets[&doc_a], &nets[&doc_b]);
        *doc_counts.entry(doc_a.clone()).or_insert(0) += 1;
        *doc_counts.entry(doc_b.clone()).or_insert(0) += 1;
        findings.push(Isomorphism {
            title_a: input.title(&doc_a),
            title_b: input.title(&doc_b),
            doc_a,
            doc_b,
            structural_similarity: structural,
            semantic_distance: distance,
            score,
            matched,
        });
    }
    findings.sort_by(|a, b| b.score.total_cmp(&a.score));
    findings.truncate(max_findings);
    findings.into_iter().map(Finding::Isomorphism).collect()
}

/// The unit mean of a document's first five blocks in document order.
fn centroid(blocks: &[&IndexedBlock]) -> Option<Vec<f32>> {
    let mut blocks: Vec<&IndexedBlock> = blocks.to_vec();
    blocks.sort_by(|x, y| x.order.total_cmp(&y.order));
    let first: Vec<&[f32]> = blocks.iter().take(5).map(|b| b.vector).collect();
    let dims = first.first()?.len();
    let mut sum = vec![0.0_f32; dims];
    for v in first.iter().filter(|v| v.len() == dims) {
        for (s, x) in sum.iter_mut().zip(v.iter()) {
            *s += x;
        }
    }
    let norm = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return None;
    }
    Some(sum.into_iter().map(|x| x / norm).collect())
}

/// 1 - cosine of the two documents' centroids, calibrated on the centroid
/// scale; 0.5 when either document has no vectors, as in cloud-1.
fn semantic_distance(calibration: &Calibration, a: Option<&Vec<f32>>, b: Option<&Vec<f32>>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else { return 0.5 };
    if a.len() != b.len() {
        return 0.5;
    }
    (1.0 - calibration.calibrate(dot(a, b))).clamp(0.0, 1.0)
}

pub(crate) fn scan_all(input: &ScanInput, budgets: Budgets) -> ScanResults {
    ScanResults {
        displacements: scan_displacement(input, budgets.displacement_top_k),
        condensations: scan_condensation(input, budgets.condensation_sample),
        wire_inferences: scan_wire_inference(input, budgets.wire_inference_paths),
        isomorphisms: scan_isomorphisms(input, budgets.isomorphism_max),
    }
}
