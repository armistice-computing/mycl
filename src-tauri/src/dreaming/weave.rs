//! The five-layer charge cascade (cloud-1 `charge.py`) and the weaver that
//! groups charged findings into dreams (cloud-1 `weaver.py`).
//!
//! | Layer | Signal              | Method                            |
//! |-------|---------------------|-----------------------------------|
//! | 1     | Embedding intensity | gate: score >= 0.3                |
//! | 2     | Structural distance | inverse wire-path length          |
//! | 3     | Valuation asymmetry | importance gap + valence opposed  |
//! | 4     | Temporal            | Eschaton's two-peaked curve       |
//! | 5     | Articulability, inv | lower confidence, higher charge   |
//!
//! cloud-1 never fed layer 4; here it carries the curve in `temporal.rs`.
//! The weights are one of the four mechanics a person may edit.

use super::mechanics::{ChargeWeights, NAVEL_CEILING, NAVEL_FLOOR, WEAVER_INCLUDE_NAVEL, WEAVER_MAX_PER_BUNDLE, WEAVER_MIN_PER_BUNDLE};
use super::scanner::{Finding, ScanResults};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ChargeInput {
    pub(crate) confidence: f64,
    pub(crate) embedding_score: f64,
    pub(crate) wire_path_length: Option<u32>,
    pub(crate) importance_a: f64,
    pub(crate) importance_b: f64,
    pub(crate) valence_a: f64,
    pub(crate) valence_b: f64,
    /// The temporal curve's value for this finding (0..1).
    pub(crate) temporal: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Charge {
    pub(crate) layers: [f64; 5],
    pub(crate) composite: f64,
    pub(crate) is_navel: bool,
}

pub(crate) fn score(input: ChargeInput, w: &ChargeWeights) -> Charge {
    let l1 = if input.embedding_score >= 0.3 { 1.0 } else { 0.0 };
    let l2 = match input.wire_path_length {
        None => 1.0,
        Some(n) if n <= 1 => 0.1,
        Some(n) => (1.0 - 1.0 / f64::from(n)).min(1.0),
    };
    let importance_gap = (input.importance_a - input.importance_b).abs() / 5.0;
    let (va, vb) = (input.valence_a, input.valence_b);
    let valence = if (va > 0.0 && vb < 0.0) || (va < 0.0 && vb > 0.0) {
        0.5
    } else if va == 0.0 || vb == 0.0 {
        0.25
    } else {
        0.0
    };
    let l3 = (importance_gap + valence).min(1.0);
    let l4 = input.temporal.clamp(0.0, 1.0);
    let l5 = 1.0 - input.confidence;
    let mut composite = w.embedding * l1 + w.structural * l2 + w.valuation * l3 + w.temporal * l4 + w.articulability * l5;
    if l1 == 0.0 {
        composite = 0.0;
    }
    Charge {
        layers: [l1, l2, l3, l4, l5],
        composite,
        is_navel: NAVEL_FLOOR <= input.confidence && input.confidence < NAVEL_CEILING,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Ranked {
    pub(crate) finding: Finding,
    pub(crate) charge: Charge,
    /// Made partly of material touched within the residue window.
    pub(crate) residue: bool,
}

/// Every finding with its charge, highest first. The confidences here are
/// cloud-1's pre-articulation placeholders.
pub(crate) fn rank(results: ScanResults, weights: &ChargeWeights, temporal: &dyn Fn(&Finding) -> f64) -> Vec<Ranked> {
    let mut items = Vec::new();
    let ScanResults { displacements, condensations, wire_inferences, isomorphisms } = results;
    for finding in displacements.into_iter().chain(condensations).chain(wire_inferences).chain(isomorphisms) {
        let input = match &finding {
            Finding::Displacement(f) => ChargeInput {
                confidence: 0.6,
                embedding_score: f.similarity,
                wire_path_length: (f.hops > 1).then_some(f.hops),
                importance_a: f.source_importance,
                ..Default::default()
            },
            Finding::Condensation(f) => ChargeInput { confidence: 0.6, embedding_score: f.avg_similarity, ..Default::default() },
            Finding::WireInference(f) => ChargeInput {
                confidence: f.structural_confidence,
                embedding_score: 1.0,
                wire_path_length: Some(2),
                ..Default::default()
            },
            Finding::Isomorphism(f) => ChargeInput {
                confidence: 0.7,
                embedding_score: 1.0,
                importance_a: f.semantic_distance * 5.0,
                importance_b: (1.0 - f.structural_similarity) * 5.0,
                ..Default::default()
            },
        };
        let input = ChargeInput { temporal: temporal(&finding), ..input };
        items.push(Ranked { charge: score(input, weights), finding, residue: false });
    }
    // Stable, like Python's sort: equal charges keep scan order.
    items.sort_by(|a, b| b.charge.composite.total_cmp(&a.charge.composite));
    items
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Bundle {
    pub(crate) findings: Vec<Ranked>,
    pub(crate) total_charge: f64,
    pub(crate) docs: BTreeSet<String>,
    /// The first finding fills the residue slot.
    pub(crate) residue_slot: bool,
}

impl Bundle {
    fn add(&mut self, item: &Ranked) {
        self.total_charge += item.charge.composite;
        self.docs.extend(item.finding.doc_ids());
        self.findings.push(item.clone());
    }

    fn kinds(&self) -> BTreeSet<&'static str> {
        self.findings.iter().map(|r| r.finding.kind()).collect()
    }

    pub(crate) fn kinds_in_order(&self) -> Vec<&'static str> {
        self.findings.iter().map(|r| r.finding.kind()).collect()
    }
}

/// Group ranked findings into at most `max_bundles` dreams: with
/// `residue_slot`, first the strongest residue finding for each dream (the
/// slot stays empty when there is none); then round-robin by cross-type,
/// cross-document preference; then one low-charge "navel" finding from below
/// the pool per dream. Without `residue_slot` this is cloud-1's weaver.
pub(crate) fn weave(ranked: &[Ranked], max_bundles: usize, residue_slot: bool) -> Vec<Bundle> {
    if max_bundles == 0 || ranked.is_empty() {
        return Vec::new();
    }
    let scene_slots = WEAVER_MAX_PER_BUNDLE - usize::from(WEAVER_INCLUDE_NAVEL) - usize::from(residue_slot);
    let mut bundles = vec![Bundle::default(); max_bundles];
    let mut assigned = vec![false; ranked.len()];

    if residue_slot {
        // Highest charge first; one residue finding per dream.
        for bundle in bundles.iter_mut() {
            if let Some(i) = (0..ranked.len()).find(|&i| !assigned[i] && ranked[i].residue) {
                assigned[i] = true;
                bundle.add(&ranked[i]);
                bundle.residue_slot = true;
            }
        }
    }

    // The pool is the strongest findings still unassigned, enough to fill
    // every scene slot.
    let pool: Vec<usize> = (0..ranked.len()).filter(|&i| !assigned[i]).take(max_bundles * scene_slots).collect();
    for _ in 0..scene_slots {
        for bundle in bundles.iter_mut() {
            if let Some(best) = pick_best(ranked, &pool, bundle, &assigned) {
                assigned[best] = true;
                bundle.add(&ranked[best]);
            }
        }
    }

    let below_pool = pool.last().map_or(0, |&last| last + 1);
    if WEAVER_INCLUDE_NAVEL && ranked.len() > below_pool {
        let candidates: Vec<usize> = (below_pool..ranked.len()).rev().collect();
        for bundle in bundles.iter_mut() {
            if bundle.findings.len() >= WEAVER_MAX_PER_BUNDLE {
                continue;
            }
            let disjoint = candidates
                .iter()
                .copied()
                .find(|&i| !assigned[i] && ranked[i].finding.doc_ids().is_disjoint(&bundle.docs));
            let chosen = disjoint.or_else(|| candidates.iter().copied().find(|&i| !assigned[i]));
            if let Some(i) = chosen {
                assigned[i] = true;
                bundle.add(&ranked[i]);
            }
        }
    }

    let mut bundles: Vec<Bundle> = bundles.into_iter().filter(|b| b.findings.len() >= WEAVER_MIN_PER_BUNDLE).collect();
    bundles.sort_by(|a, b| b.total_charge.total_cmp(&a.total_charge));
    bundles
}

fn pick_best(ranked: &[Ranked], pool: &[usize], bundle: &Bundle, assigned: &[bool]) -> Option<usize> {
    let kinds = bundle.kinds();
    let mut best: Option<(usize, f64)> = None;
    for &i in pool {
        if assigned[i] {
            continue;
        }
        let item = &ranked[i];
        let mut s = item.charge.composite;
        if !kinds.contains(item.finding.kind()) {
            s += 2.0;
        }
        if item.finding.doc_ids().is_disjoint(&bundle.docs) {
            s += 1.0;
        }
        if best.is_none_or(|(_, b)| s > b) {
            best = Some((i, s));
        }
    }
    best.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreaming::mechanics::DEFAULT_CHARGE_WEIGHTS as W;
    use crate::dreaming::scanner::{Condensation, CondensedBlock, Displacement, Isomorphism, WireInference};

    #[test]
    fn charge_layers_match_cloud_cascade() {
        // A two-hop displacement from an importance-5 seed, placeholder
        // confidence 0.6: 0.1*1 + 0.25*0.5 + 0.2*min(1, 1+0.25) + 0 + 0.25*0.4
        let c = score(ChargeInput { confidence: 0.6, embedding_score: 0.55, wire_path_length: Some(2), importance_a: 5.0, ..Default::default() }, &W);
        assert_eq!(c.layers, [1.0, 0.5, 1.0, 0.0, 0.4]);
        assert!((c.composite - 0.525).abs() < 1e-9);
        // The embedding gate zeroes everything.
        let gated = score(ChargeInput { confidence: 0.1, embedding_score: 0.2, ..Default::default() }, &W);
        assert_eq!(gated.composite, 0.0);
        assert!(score(ChargeInput { confidence: 0.4, embedding_score: 1.0, ..Default::default() }, &W).is_navel);
        // The temporal layer carries the curve's value at its weight.
        let fed = score(ChargeInput { confidence: 0.6, embedding_score: 0.55, wire_path_length: Some(2), importance_a: 5.0, temporal: 1.0, ..Default::default() }, &W);
        assert!((fed.composite - (0.525 + 0.2)).abs() < 1e-9);
    }

    fn disp(src: &str, nbr: &str, sim: f64) -> Finding {
        Finding::Displacement(Displacement {
            source_block: format!("{src}-b"), source_doc: src.into(), source_text: "s".into(), source_importance: 4.0,
            neighbor_block: format!("{nbr}-b"), neighbor_doc: nbr.into(), neighbor_text: "n".into(), similarity: sim,
            intermediary: None, hops: 2,
        })
    }
    fn cond(docs: &[&str]) -> Finding {
        Finding::Condensation(Condensation {
            blocks: docs.iter().map(|d| CondensedBlock { block: format!("{d}-c"), doc: (*d).into(), text: "t".into() }).collect(),
            centroid_text: "t".into(), avg_similarity: 0.7,
        })
    }
    fn wire(a: &str, b: &str, c: &str) -> Finding {
        Finding::WireInference(WireInference {
            docs: [a.into(), b.into(), c.into()], titles: [a.into(), b.into(), c.into()],
            predicate_ab: "supports".into(), predicate_bc: "supports".into(), inferred_predicate: "supports".into(),
            structural_confidence: 0.3, requires_llm_judgment: true, composition_note: String::new(),
            block_a: String::new(), block_b: String::new(), block_c: String::new(),
            block_a_text: String::new(), block_c_text: String::new(),
        })
    }
    fn iso(a: &str, b: &str) -> Finding {
        Finding::Isomorphism(Isomorphism {
            doc_a: a.into(), doc_b: b.into(), title_a: a.into(), title_b: b.into(),
            structural_similarity: 0.9, semantic_distance: 0.5, score: 0.45, matched: Vec::new(),
        })
    }

    #[test]
    fn weaver_mixes_types_and_adds_a_navel() {
        let results = ScanResults {
            displacements: vec![disp("a", "b", 0.5), disp("c", "d", 0.5), disp("e", "f", 0.5)],
            condensations: vec![cond(&["g", "h", "i"]), cond(&["j", "k", "l"])],
            wire_inferences: vec![wire("m", "n", "o"), wire("p", "q", "r")],
            isomorphisms: vec![iso("s", "t")],
        };
        let ranked = rank(results, &W, &|_| 0.0);
        assert!(ranked.windows(2).all(|w| w[0].charge.composite >= w[1].charge.composite));
        // Charges: isomorphism 0.555, displacement 0.525, condensation 0.5,
        // wire inference 0.45.
        assert_eq!(ranked[0].finding.kind(), "isomorphism");
        assert_eq!(ranked[7].finding.kind(), "wire_inference");
        // One dream: the pool is the top four (scene slots), and the navel
        // round reaches below it, to the lowest-charge finding.
        let one = weave(&ranked, 1, false);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].kinds_in_order(), vec!["isomorphism", "displacement", "displacement", "displacement", "wire_inference"]);
        // Two dreams: the pool holds all eight, round-robin prefers a kind
        // the dream lacks, and there is nothing left below for a navel.
        let two = weave(&ranked, 2, false);
        assert_eq!(two.len(), 2);
        for dream in &two {
            assert_eq!(dream.findings.len(), 4);
            assert!(dream.kinds().len() >= 3, "{:?}", dream.kinds_in_order());
        }
    }

    #[test]
    fn underfilled_dreams_are_dropped() {
        let ranked = rank(ScanResults { displacements: vec![disp("a", "b", 0.5)], ..Default::default() }, &W, &|_| 0.0);
        assert!(weave(&ranked, 3, false).is_empty());
    }

    #[test]
    fn each_dream_keeps_a_slot_for_the_days_residue() {
        let results = ScanResults {
            displacements: vec![disp("a", "b", 0.5), disp("c", "d", 0.5), disp("e", "f", 0.5)],
            condensations: vec![cond(&["g", "h", "i"]), cond(&["j", "k", "l"])],
            wire_inferences: vec![wire("m", "n", "o"), wire("p", "q", "r")],
            isomorphisms: vec![iso("s", "t")],
        };
        let mut ranked = rank(results, &W, &|_| 0.0);
        // The weakest finding is the only recent one: it still gets in, first.
        let last = ranked.len() - 1;
        ranked[last].residue = true;
        let one = weave(&ranked, 1, true);
        assert!(one[0].residue_slot);
        assert_eq!(one[0].findings[0].finding.doc_ids(), ranked[last].finding.doc_ids());
        assert_eq!(one[0].findings.len(), 5);
        // Nothing recent: the slot stays empty and the dream is one shorter.
        ranked[last].residue = false;
        let empty = weave(&ranked, 1, true);
        assert!(!empty[0].residue_slot);
        assert_eq!(empty[0].findings.len(), 4);
        // Two dreams, one residue finding: only the first dream gets it.
        ranked[last].residue = true;
        let two = weave(&ranked, 2, true);
        assert_eq!(two.iter().filter(|b| b.residue_slot).count(), 1);
    }
}
