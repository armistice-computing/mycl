//! The unified head reader over the LIVE memory projection: which record is
//! the current head of each `mem:lineage`, now ([`current_heads_by_lineage`])
//! or as of a point in time ([`heads_as_of`]). `query_engine`'s `currentHeads`
//! and `as_of` reads route through these two functions, so there is ONE
//! "current head" semantics. A forked supersession (two `mem:isCurrent=true`
//! heads on one lineage) is CONTESTED, never resolved here: the reader returns
//! every head, flagged, and resolution stays an explicit producer act.
//!
//! The post-hoc conformance sweep that filed contested lineages into the
//! violation ledger (`sweep_memory_conformance`, the `emporium_sweep` tool and
//! the `/emporium/sweep` route) was cut on 2026-10-27 for the Mycl release;
//! `current_heads_flagged` remains as the write path's test oracle.

use std::collections::BTreeMap;

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

#[cfg(test)]
use crate::app_runtime::AppHandle;
#[cfg(test)]
use crate::emporium::contract::memory_core_vocabulary;
#[cfg(test)]
use crate::emporium::contract::VocabularyContract;
#[cfg(test)]
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::query_emit::union_over_graphs;
use crate::emporium::terms::RETRACTED_AT_PRED;
#[cfg(test)]
use crate::rdf_authority::memory_projection_graph_iri_for;

/// One lineage with ALL its current heads and whether it is contested (>1 head).
/// The read-back shape for the ratified `mem:contested` / "return all" policy —
/// storage never picks a winner, so a reader gets every head, flagged.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg(test)]
pub(crate) struct LineageHeads {
    pub(crate) lineage: String,
    pub(crate) heads: Vec<String>,
    pub(crate) contested: bool,
}

/// The ratified conflict strategy a memory graph runs under. `contested` (the
/// declared default, `plans/mo-reconciliation-policy-20260704.md`): a fork is
/// FLAGGED and all heads are returned; the engine never resolves. Any other
/// declared value is carried verbatim as a v2 resolver hook — v1 still only
/// flags. Read from the contract's `conflict_policies` (the ratified override
/// surface), so behavior is declaration-driven, not hard-coded.
#[cfg(test)]
pub(crate) fn declared_conflict_strategy(contract: &VocabularyContract) -> String {
    contract
        .conflict_policies
        .iter()
        .find_map(|p| {
            p.get("conflictStrategy")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "contested".to_string())
}

/// The read-back of every current head, grouped by lineage and contested-flagged
/// — the "return all heads flagged" primitive a recall reader consumes to honor
/// the ratified policy (surface all testimony, never let storage rank).
#[derive(Debug, serde::Serialize)]
#[cfg(test)]
pub(crate) struct HeadsReport {
    pub(crate) graph_id: String,
    pub(crate) observer_graph: String,
    pub(crate) strategy: String,
    pub(crate) lineages: Vec<LineageHeads>,
}

/// Run a `SELECT ?lineage ?s WHERE { … }` query and group the solutions into
/// `lineage → [head, …]` (heads sorted; lineages sorted via the `BTreeMap`).
/// The shared execution tail for [`current_heads_by_lineage`] and
/// [`heads_as_of`] — the two differ only in the WHERE-clause body they build,
/// never in how the rows are gathered.
fn run_lineage_head_query(
    store: &Store,
    query: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|e| format!("parse heads query: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute heads query: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("heads query expected SELECT solutions".to_string()),
    };
    let strip = |t: &oxigraph::model::Term| {
        let s = t.to_string();
        s.strip_prefix('<')
            .and_then(|x| x.strip_suffix('>'))
            .map(str::to_string)
            .unwrap_or(s)
    };
    let mut heads_by_lineage: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("heads row: {e}"))?;
        let (Some(lineage), Some(head)) = (sol.get("lineage"), sol.get("s")) else {
            continue;
        };
        heads_by_lineage
            .entry(strip(lineage))
            .or_default()
            .push(strip(head));
    }
    for heads in heads_by_lineage.values_mut() {
        heads.sort();
    }
    Ok(heads_by_lineage)
}

/// Gather every `(lineage → [current head])` across one or more graphs — THE
/// server-authoritative "current head" semantics (ratified, `plans/mo-
/// reconciliation-policy-20260704.md`: "Head — a record with
/// `mem:isCurrent=true`"). The shared read behind the sweep (contested
/// subset), the return-all reader ([`current_heads_flagged`]), AND the T2
/// `query_emit` `currentHeads` plan ([`crate::emporium::query_engine`]) — ONE
/// function, so "current head" cannot mean two different things depending on
/// which caller asks.
///
/// `graphs` is queried as a UNION (one branch per graph) so a membrane-ed
/// class's commons-∪-observer scoping is the SAME shape as a single-graph
/// read — a 1-element slice degenerates to exactly the pre-generalization
/// query (an inert extra `{ }` grouping; the BGP and its results are
/// unchanged). `lineage_pred`/`is_current_pred` are the class's OWN expanded
/// predicate URIs (not hardcoded to `mem:`), so this generalizes to any class
/// whose shape declares the same lineage convention.
///
/// T-W Law 4 (retraction excludes a head): every branch ALSO excludes a
/// subject carrying [`RETRACTED_AT_PRED`] in that same graph — a retracted
/// record is never a current head again, in EVERY caller of this ONE
/// function (the sweep, `emporium_heads`, `sparql_query_named`'s
/// `currentHeads`, and `emporium_query`'s contested-class routing).
pub(crate) fn current_heads_by_lineage(
    store: &Store,
    graphs: &[String],
    lineage_pred: &str,
    is_current_pred: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    if graphs.is_empty() {
        return Err("current_heads_by_lineage: no graphs to scope the read to".to_string());
    }
    let branches: Vec<String> = graphs
        .iter()
        .map(|g| {
            format!(
                "{{ GRAPH <{g}> {{ ?s <{lineage_pred}> ?lineage ; <{is_current_pred}> true . \
                 FILTER NOT EXISTS {{ ?s <{RETRACTED_AT_PRED}> ?emporiumRetractedAt }} }} }}"
            )
        })
        .collect();
    let query = format!(
        "SELECT ?lineage ?s WHERE {{ {} }}",
        branches.join(" UNION ")
    );
    run_lineage_head_query(store, &query)
}

/// Reconstruct the heads AS OF a point in time — the historical counterpart to
/// [`current_heads_by_lineage`]. `mem:isCurrent` is a LIVE flag with no history
/// of its own (it only ever describes "now"), so "what was current at time T"
/// cannot be read off it; instead a record `r` is a head-as-of-`asOf` in its
/// lineage iff `r.createdAt <= asOf` AND no OTHER record supersedes it with ITS
/// OWN `createdAt <= asOf` too. This walks the append-only supersession log
/// (`supersedes`/`createdAt`), so it is well-defined for any past timestamp —
/// the §8.1 "return all heads flagged" policy generalized across time, not just
/// the live instant. `as_of_literal` is a ready-to-splice SPARQL term (a typed
/// `"…"^^xsd:dateTime` literal); the caller owns formatting it.
///
/// The supersession check is scoped to the SAME union of `graphs` as the
/// candidate search, NOT re-scoped per candidate's own graph: a membrane-ed
/// class's `graphs` is commons ∪ an observer's membrane, and a record filed
/// in commons can be superseded by a later record filed in the observer's own
/// membrane (recall's ordinary shape). Nesting `FILTER NOT EXISTS` inside each
/// `GRAPH <g>` branch would make that supersession invisible whenever the
/// superseding record lives in a DIFFERENT graph than the one being checked —
/// the commons branch can't see into the membrane branch — silently
/// resurrecting an already-superseded head as "current as of". Both the
/// candidate BGP and the NOT-EXISTS BGP are therefore UNIONed across the same
/// `graphs` (a 1-graph slice degenerates to the pre-existing per-graph shape).
pub(crate) fn heads_as_of(
    store: &Store,
    graphs: &[String],
    lineage_pred: &str,
    created_at_pred: &str,
    supersedes_pred: &str,
    as_of_literal: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    if graphs.is_empty() {
        return Err("heads_as_of: no graphs to scope the read to".to_string());
    }
    // T-W Law 4: a retracted candidate is never a historical head either —
    // but ONLY once the retraction had actually happened as of `as_of`. A
    // retraction is itself a timestamped event; a query for a moment BEFORE
    // that retraction must still see the record as it stood then, or an
    // as-of read would smuggle "currently retracted" into the past (the
    // review finding this guards). Compare, don't just check existence —
    // mirrors the `?createdAt <= as_of_literal` treatment below.
    let candidate_body = format!(
        "?s <{lineage_pred}> ?lineage ; <{created_at_pred}> ?createdAt . \
         FILTER NOT EXISTS {{ \
           ?s <{RETRACTED_AT_PRED}> ?emporiumRetractedAtCandidate . \
           FILTER(?emporiumRetractedAtCandidate <= {as_of_literal}) \
         }}"
    );
    let supersession_body = format!("?s2 <{supersedes_pred}> ?s ; <{created_at_pred}> ?createdAt2");
    let query = format!(
        "SELECT ?lineage ?s WHERE {{ \
           {candidates} \
           FILTER(?createdAt <= {as_of_literal}) \
           FILTER NOT EXISTS {{ \
             {supersessions} \
             FILTER(?createdAt2 <= {as_of_literal}) \
           }} \
         }}",
        candidates = union_over_graphs(graphs, &candidate_body),
        supersessions = union_over_graphs(graphs, &supersession_body),
    );
    run_lineage_head_query(store, &query)
}

/// Read every current head, grouped + contested-flagged (the "return all"
/// policy). Pure read — never writes, never resolves.
#[cfg(test)]
pub(crate) fn current_heads_flagged(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
) -> Result<HeadsReport, String> {
    let store = open_memory_store(app, graph_id)?;
    let mem_graph = memory_projection_graph_iri_for(graph_id, observer);
    let contract = memory_core_vocabulary();
    let mem = contract.primary_namespace();
    let heads_by_lineage = current_heads_by_lineage(
        &store,
        std::slice::from_ref(&mem_graph),
        &format!("{mem}lineage"),
        &format!("{mem}isCurrent"),
    )?;
    let lineages = heads_by_lineage
        .into_iter()
        .map(|(lineage, heads)| LineageHeads {
            contested: heads.len() > 1,
            lineage,
            heads,
        })
        .collect();
    Ok(HeadsReport {
        graph_id: graph_id.to_string(),
        observer_graph: mem_graph,
        strategy: declared_conflict_strategy(contract),
        lineages,
    })
}

