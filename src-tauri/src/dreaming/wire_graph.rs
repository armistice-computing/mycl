//! The wire graph as the dream scanners see it, ported from cloud-1's
//! `wire_graph.py`: typed directed edges, two-hop paths with predicate
//! composition rules, ego networks and their predicate signatures.
//!
//! Wires are interpretive acts, not logical connectives. A composition rule
//! proposes a candidate for the Dreamer; it never asserts a wire.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompositionRule {
    pub(crate) inferred_predicate: &'static str,
    pub(crate) confidence: f64,
    pub(crate) requires_llm_judgment: bool,
    pub(crate) note: &'static str,
}

const fn rule(inferred: &'static str, confidence: f64, judgment: bool, note: &'static str) -> CompositionRule {
    CompositionRule { inferred_predicate: inferred, confidence, requires_llm_judgment: judgment, note }
}

/// cloud-1's `PREDICATE_COMPOSITIONS`, verbatim.
const COMPOSITIONS: &[((&str, &str), CompositionRule)] = &[
    (("causeOf", "causeOf"), rule("causeOf", 0.7, false,
        "Causal chains compose. If A causes B and B causes C, A is a distal cause of C.")),
    (("flowsInto", "flowsInto"), rule("flowsInto", 0.7, false,
        "Flows compose. If A flows into B and B flows into C, there is a current from A toward C.")),
    (("precedes", "precedes"), rule("precedes", 0.8, false,
        "Temporal succession is reliably transitive.")),
    (("partOf", "partOf"), rule("partOf", 0.7, false,
        "Mereological transitivity: parts of parts are parts of the whole.")),
    (("contains", "contains"), rule("contains", 0.7, false, "Containment is transitive.")),
    (("supports", "supports"), rule("supports", 0.3, true,
        "Transitive support attenuates. Supporting a premise that supports a conclusion doesn't necessarily mean you support the conclusion.")),
    (("enables", "enables"), rule("enables", 0.5, true,
        "Enabling chains: A enables B which enables C. Plausible but the path from A to C may involve assumptions about how B is used.")),
    (("requires", "requires"), rule("requires", 0.5, true,
        "Dependency chains. A requires B, B requires C \u{2014} does A require C? Only if B's dependence on C is intrinsic, not contextual.")),
    (("supports", "contradicts"), rule("contradicts", 0.5, true,
        "If A supports B and B contradicts C, A is in tension with C.")),
    (("contradicts", "supports"), rule("contradicts", 0.5, true,
        "If A contradicts B and B supports C, A may undermine C.")),
    (("contradicts", "contradicts"), rule("co_belligerent", 0.2, true,
        "CAUTION: Two critics of the same thing are NOT necessarily allies. This flags a potential alliance, not an assertion of support.")),
    (("exemplifies", "supports"), rule("supports", 0.4, true,
        "If A exemplifies B and B supports C, A may serve as instance-level evidence for C.")),
    (("enables", "requires"), rule("conditionally_requires", 0.3, true,
        "A enables B, B requires C \u{2192} 'if you use A to get B, you'll need C.'")),
    (("causeOf", "consequenceOf"), rule("relatedTo", 0.3, true,
        "A causes B, and C is a consequence of B. A and C are related through B.")),
];

pub(crate) fn composition(first: &str, second: &str) -> Option<&'static CompositionRule> {
    COMPOSITIONS
        .iter()
        .find(|((a, b), _)| *a == first && *b == second)
        .map(|(_, rule)| rule)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WireEdge {
    pub(crate) wire_id: String,
    pub(crate) source_doc: String,
    pub(crate) target_doc: String,
    pub(crate) predicate: String,
    pub(crate) source_block: Option<String>,
    pub(crate) target_block: Option<String>,
    pub(crate) bidirectional: bool,
}

pub(crate) fn short_predicate(predicate: &str) -> String {
    match predicate.rsplit_once('#') {
        Some((_, short)) => short.to_string(),
        None => predicate.rsplit('/').next().unwrap_or(predicate).to_string(),
    }
}

fn text(wire: &Value, key: &str) -> Option<String> {
    wire.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Active workspace wires as directed edges. Inverse mirrors (`-inv`) and
/// tombstoned wires are skipped, as in cloud-1's `load_wire_graph`. Only
/// judgment wires are the graph's structure: record wires are provenance,
/// and a dream must never find its own dreamtOf wires (a "dream now" by
/// day runs while last night's are still live).
pub(crate) fn edges_from_workspace_wires(wires: &[Value]) -> Vec<WireEdge> {
    wires
        .iter()
        .filter(|wire| crate::wire_state::wire_is_active(wire))
        .filter(|wire| crate::wire_predicates::WireFamily::of_wire(wire).conceptual())
        .filter_map(|wire| {
            let wire_id = text(wire, "id")?;
            if wire_id.ends_with("-inv") {
                return None;
            }
            Some(WireEdge {
                source_doc: text(wire, "sourceDocumentId")?,
                target_doc: text(wire, "targetDocumentId")?,
                predicate: short_predicate(&text(wire, "predicate")?),
                source_block: text(wire, "sourceBlockId"),
                target_block: text(wire, "targetBlockId"),
                bidirectional: wire.get("bidirectional").and_then(Value::as_bool).unwrap_or(false),
                wire_id,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct TwoHopPath {
    pub(crate) ab: WireEdge,
    pub(crate) bc: WireEdge,
    pub(crate) composition: Option<&'static CompositionRule>,
}

/// Adjacency in edge order (a `BTreeMap` keeps iteration stable, where
/// cloud-1 relied on dict insertion order). A bidirectional edge also runs
/// backwards.
fn adjacency(edges: &[WireEdge]) -> Vec<(String, Vec<WireEdge>)> {
    let mut order: Vec<String> = Vec::new();
    let mut adj: HashMap<String, Vec<WireEdge>> = HashMap::new();
    let mut push = |doc: &str, edge: WireEdge| {
        if !adj.contains_key(doc) {
            order.push(doc.to_string());
        }
        adj.entry(doc.to_string()).or_default().push(edge);
    };
    for edge in edges {
        push(&edge.source_doc, edge.clone());
        if edge.bidirectional {
            let reverse = WireEdge {
                wire_id: format!("{}-rev", edge.wire_id),
                source_doc: edge.target_doc.clone(),
                target_doc: edge.source_doc.clone(),
                predicate: edge.predicate.clone(),
                source_block: edge.target_block.clone(),
                target_block: edge.source_block.clone(),
                bidirectional: true,
            };
            push(&edge.target_doc, reverse);
        }
    }
    order.into_iter().map(|doc| { let edges = adj.remove(&doc).unwrap_or_default(); (doc, edges) }).collect()
}

/// Two-hop paths A→B→C with no direct wire between A and C, in cloud-1's
/// traversal order, stopping at `max_paths`.
pub(crate) fn two_hop_paths(edges: &[WireEdge], max_paths: usize) -> Vec<TwoHopPath> {
    let adj = adjacency(edges);
    let index: HashMap<&str, &Vec<WireEdge>> = adj.iter().map(|(d, e)| (d.as_str(), e)).collect();
    let direct: BTreeSet<(String, String)> = edges
        .iter()
        .map(|e| ordered(&e.source_doc, &e.target_doc))
        .collect();
    let mut paths = Vec::new();
    for (doc_a, outgoing) in &adj {
        for ab in outgoing {
            let doc_b = &ab.target_doc;
            let Some(next) = index.get(doc_b.as_str()) else { continue };
            for bc in next.iter() {
                let doc_c = &bc.target_doc;
                if doc_c == doc_a || doc_c == doc_b || direct.contains(&ordered(doc_a, doc_c)) {
                    continue;
                }
                paths.push(TwoHopPath {
                    ab: ab.clone(),
                    bc: bc.clone(),
                    composition: composition(&ab.predicate, &bc.predicate),
                });
                if paths.len() >= max_paths {
                    return paths;
                }
            }
        }
    }
    paths
}

fn ordered(a: &str, b: &str) -> (String, String) {
    if a <= b { (a.to_string(), b.to_string()) } else { (b.to_string(), a.to_string()) }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EgoNetwork {
    pub(crate) outgoing: Vec<(String, String)>,
    pub(crate) incoming: Vec<(String, String)>,
}

impl EgoNetwork {
    pub(crate) fn size(&self) -> usize {
        self.outgoing.len() + self.incoming.len()
    }

    fn signature(&self) -> BTreeMap<(String, &'static str), f64> {
        let mut sig = BTreeMap::new();
        for (pred, _) in &self.outgoing {
            *sig.entry((pred.clone(), "out")).or_insert(0.0) += 1.0;
        }
        for (pred, _) in &self.incoming {
            *sig.entry((pred.clone(), "in")).or_insert(0.0) += 1.0;
        }
        sig
    }

    fn neighbors(&self) -> BTreeSet<&str> {
        self.outgoing.iter().chain(&self.incoming).map(|(_, n)| n.as_str()).collect()
    }
}

pub(crate) fn ego_networks(edges: &[WireEdge]) -> BTreeMap<String, EgoNetwork> {
    let mut nets: BTreeMap<String, EgoNetwork> = BTreeMap::new();
    for e in edges {
        nets.entry(e.source_doc.clone()).or_default().outgoing.push((e.predicate.clone(), e.target_doc.clone()));
        nets.entry(e.target_doc.clone()).or_default().incoming.push((e.predicate.clone(), e.source_doc.clone()));
        if e.bidirectional {
            nets.entry(e.target_doc.clone()).or_default().outgoing.push((e.predicate.clone(), e.source_doc.clone()));
            nets.entry(e.source_doc.clone()).or_default().incoming.push((e.predicate.clone(), e.target_doc.clone()));
        }
    }
    nets
}

fn signature_similarity(a: &BTreeMap<(String, &'static str), f64>, b: &BTreeMap<(String, &'static str), f64>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().map(|(k, v)| v * b.get(k).copied().unwrap_or(0.0)).sum();
    let norm = |m: &BTreeMap<_, f64>| m.values().map(|v| v * v).sum::<f64>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Document pairs whose ego networks have similar predicate signatures and
/// do not share more than half of the first one's neighbors, best first.
pub(crate) fn candidate_isomorphisms(
    nets: &BTreeMap<String, EgoNetwork>,
    min_similarity: f64,
    min_ego_size: usize,
) -> Vec<(String, String, f64)> {
    let eligible: Vec<(&String, &EgoNetwork, BTreeMap<(String, &'static str), f64>)> = nets
        .iter()
        .filter(|(_, net)| net.size() >= min_ego_size)
        .map(|(doc, net)| (doc, net, net.signature()))
        .collect();
    let mut matches = Vec::new();
    for (i, (doc_a, net_a, sig_a)) in eligible.iter().enumerate() {
        let neighbors_a = net_a.neighbors();
        for (doc_b, net_b, sig_b) in &eligible[i + 1..] {
            let shared = neighbors_a.intersection(&net_b.neighbors()).count();
            if shared as f64 > neighbors_a.len() as f64 / 2.0 {
                continue;
            }
            let sim = signature_similarity(sig_a, sig_b);
            if sim >= min_similarity {
                matches.push(((*doc_a).clone(), (*doc_b).clone(), sim));
            }
        }
    }
    // Stable sort, like Python's: equal scores keep pair order.
    matches.sort_by(|x, y| y.2.total_cmp(&x.2));
    matches
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MatchedPair {
    pub(crate) predicate: String,
    pub(crate) direction: &'static str,
    pub(crate) neighbor_a: String,
    pub(crate) neighbor_b: String,
}

/// Greedy neighbor matching; the excess shows where the analogy breaks.
pub(crate) fn neighbor_correspondence(
    a: &EgoNetwork,
    b: &EgoNetwork,
) -> (Vec<MatchedPair>, Vec<(String, String)>, Vec<(String, String)>) {
    let entries = |n: &EgoNetwork| {
        n.outgoing.iter().map(|(p, d)| (p.clone(), "out", d.clone()))
            .chain(n.incoming.iter().map(|(p, d)| (p.clone(), "in", d.clone())))
            .collect::<Vec<_>>()
    };
    let (entries_a, entries_b) = (entries(a), entries(b));
    let mut used = vec![false; entries_b.len()];
    let mut matched = Vec::new();
    let mut excess_a = Vec::new();
    for (pred, dir, nbr) in entries_a {
        match entries_b.iter().enumerate().position(|(j, (p, d, _))| !used[j] && *p == pred && *d == dir) {
            Some(j) => {
                used[j] = true;
                matched.push(MatchedPair { predicate: pred, direction: dir, neighbor_a: nbr, neighbor_b: entries_b[j].2.clone() });
            }
            None => excess_a.push((pred, nbr)),
        }
    }
    let excess_b = entries_b.into_iter().enumerate().filter(|(j, _)| !used[*j]).map(|(_, (p, _, n))| (p, n)).collect();
    (matched, excess_a, excess_b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edge(id: &str, s: &str, t: &str, p: &str) -> WireEdge {
        WireEdge { wire_id: id.into(), source_doc: s.into(), target_doc: t.into(), predicate: p.into(),
                   source_block: None, target_block: None, bidirectional: false }
    }

    #[test]
    fn workspace_wires_become_edges() {
        let wires = vec![
            json!({"id": "w1", "sourceDocumentId": "a", "targetDocumentId": "b",
                   "predicate": "http://mnemosyne.ai/vocab#supports", "sourceBlockId": "b1"}),
            json!({"id": "w1-inv", "sourceDocumentId": "b", "targetDocumentId": "a", "predicate": "supports"}),
            json!({"id": "w2", "sourceDocumentId": "b", "targetDocumentId": "c", "predicate": "flowsInto",
                   "deletedAt": "2026-01-01"}),
            json!({"id": "w3", "sourceDocumentId": "b", "targetDocumentId": "c", "predicate": "grounds",
                   "bidirectional": true}),
            // Provenance and last night's dream are not the graph's structure.
            json!({"id": "w4", "sourceDocumentId": "a", "targetDocumentId": "c", "predicate": "cites"}),
            json!({"id": "w5", "sourceDocumentId": "j", "targetDocumentId": "c", "predicate": "dreamtOf",
                   "extra": {"expiresAt": "2999-01-01T00:00:00.000Z"}}),
        ];
        let edges = edges_from_workspace_wires(&wires);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].predicate, "supports");
        assert_eq!(edges[0].source_block.as_deref(), Some("b1"));
        assert!(edges[1].bidirectional);
    }

    #[test]
    fn two_hop_skips_closed_triangles_and_applies_rules() {
        let edges = vec![
            edge("1", "a", "b", "causeOf"),
            edge("2", "b", "c", "causeOf"),
            edge("3", "b", "d", "supports"),
            edge("4", "a", "d", "relatedTo"),
        ];
        let paths = two_hop_paths(&edges, 10);
        assert_eq!(paths.len(), 1, "a→b→d is closed by a→d");
        assert_eq!(paths[0].bc.target_doc, "c");
        assert_eq!(paths[0].composition.unwrap().confidence, 0.7);
        assert!(composition("contradicts", "contradicts").unwrap().requires_llm_judgment);
    }

    #[test]
    fn isomorphism_candidates_need_size_and_distinct_neighborhoods() {
        let edges = vec![
            edge("1", "x", "p", "supports"), edge("2", "x", "q", "supports"), edge("3", "r", "x", "grounds"),
            edge("4", "y", "s", "supports"), edge("5", "y", "t", "supports"), edge("6", "u", "y", "grounds"),
        ];
        let nets = ego_networks(&edges);
        let found = candidate_isomorphisms(&nets, 0.6, 3);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0.as_str(), found[0].1.as_str()), ("x", "y"));
        assert!((found[0].2 - 1.0).abs() < 1e-9);
        let (matched, ea, eb) = neighbor_correspondence(&nets["x"], &nets["y"]);
        assert_eq!(matched.len(), 3);
        assert!(ea.is_empty() && eb.is_empty());
    }
}
