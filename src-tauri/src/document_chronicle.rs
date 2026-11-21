//! The Chronicle — per-document authorship and provenance, kept by the
//! owner process. Design of record: graph doc
//! `settlement-chronicle-design-2026-08-26`, Part 2 (chronicle) and Part 3
//! (staleness guard + force flag).
//!
//! gardend already assigned `created_at` / `updated_at` / `revision` on every
//! `DocumentRecord` before this module existed. What was missing was WHO:
//! nothing recorded which principal created a document or touched it since,
//! the import path stamped `created_at` with the import instant (every one of
//! the 1,075 documents restored on 2026-09-05 was "born" that afternoon), and
//! an agent overriding the staleness guard left no trace.
//!
//! Three pieces live here:
//!
//! 1. **Actors and the record fields** — [`ChronicleActor`] (`human` /
//!    `agent` / `system`, plus how the request was authenticated) and
//!    [`DocumentChronicle`], the flattened block of authorship fields on the
//!    record: `createdBy`, `updatedBy`, a bounded `recentAuthors` list, the
//!    `createdAtSource` honesty flag, and the forced-write trail.
//! 2. **Principal plumbing** — a task-local actor set once per loopback
//!    request from the presented credential (master token = the local human
//!    owner; a named client token = an agent named by its label), refined by
//!    the MCP dispatcher from an explicit `agent` argument, and injected into
//!    every CRDT operation payload at the enqueue choke point. Editor sync
//!    flushes carry no actor and fall back to the human owner.
//! 3. **The archive shape** — [`ChronicleEntry`] rides in the graph export
//!    under `chronicle/documents/{id}.json`, and [`ChronicleRestore`] tells
//!    `save_document` to restore the original clock instead of stamping the
//!    import instant.
//!
//! The Feb-2026 decision against human collaboration attribution stands: the
//! chronicle stamps agents and system integrity; humans are recorded as the
//! session principal and surfaced minimally.

use crate::{clock::timestamp, document_types::DocumentRecord};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;

/// Bounded length of the per-document recent-authors list.
pub(crate) const RECENT_AUTHORS_CAP: usize = 8;

pub(crate) const ACTOR_KIND_HUMAN: &str = "human";
pub(crate) const ACTOR_KIND_AGENT: &str = "agent";
pub(crate) const ACTOR_KIND_SYSTEM: &str = "system";

/// Who performed a write. `kind` is one of `human`, `agent`, `system`;
/// `name` is the principal within that kind; `via` names how the principal
/// was established (`master-token`, `client-token:<id>`, or the credential an
/// explicit `agent` argument rode in on).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChronicleActor {
    pub(crate) kind: String,
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) via: Option<String>,
}

impl ChronicleActor {
    /// The one human on a local node: the profile owner.
    pub(crate) fn human_owner() -> Self {
        Self {
            kind: ACTOR_KIND_HUMAN.to_string(),
            name: "owner".to_string(),
            via: None,
        }
    }

    pub(crate) fn human_owner_via(via: impl Into<String>) -> Self {
        Self {
            via: Some(via.into()),
            ..Self::human_owner()
        }
    }

    pub(crate) fn agent(name: impl Into<String>, via: Option<String>) -> Self {
        Self {
            kind: ACTOR_KIND_AGENT.to_string(),
            name: name.into(),
            via,
        }
    }

    pub(crate) fn system(name: impl Into<String>) -> Self {
        Self {
            kind: ACTOR_KIND_SYSTEM.to_string(),
            name: name.into(),
            via: None,
        }
    }

    /// `kind:name` — the compact form used in RDF literals and MCP responses.
    pub(crate) fn label(&self) -> String {
        format!("{}:{}", self.kind, self.name)
    }

    pub(crate) fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Accepts the object form or a `kind:name` string; anything else is
    /// treated as absent rather than an error (an actor is provenance, never a
    /// reason to fail a write).
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Object(_) => serde_json::from_value::<ChronicleActor>(value.clone())
                .ok()
                .filter(|actor| !actor.kind.trim().is_empty() && !actor.name.trim().is_empty()),
            Value::String(text) => {
                let (kind, name) = text.split_once(':')?;
                let kind = kind.trim();
                let name = name.trim();
                if kind.is_empty() || name.is_empty() {
                    return None;
                }
                Some(Self {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    via: None,
                })
            }
            _ => None,
        }
    }
}

/// One line of the bounded recent-authors list: an actor and the span of its
/// attention on this document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChronicleAuthor {
    pub(crate) actor: ChronicleActor,
    pub(crate) first_at: String,
    pub(crate) last_at: String,
    pub(crate) last_revision: u64,
    pub(crate) writes: u64,
}

fn u64_is_zero(value: &u64) -> bool {
    *value == 0
}

/// Agentic time as witnessed by the Cantor at the moment of a write: the
/// house's day and stanza (integer tokens underneath; the MToks are a display
/// skin) and, for an agent the Cantor has seen, the writer's watch and
/// measure. `evidence` is the stamp's own provenance grade — `inferred`, the
/// clock as last witnessed, never an exact reading. Read-only here; produced
/// by `crate::agentic_clock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgenticTimeStamp {
    pub(crate) day: String,
    pub(crate) stanza_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) watch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) measure: Option<u64>,
    pub(crate) witnessed_at: String,
    pub(crate) evidence: String,
}

impl AgenticTimeStamp {
    pub(crate) const STANZA_TOKENS: u64 = 1_000_000;

    /// The reading with its display skins: stanza in MTok to three places,
    /// the watch in hex with the `0x` prefix (so it never reads as a count).
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "day": self.day,
            "stanza": (self.stanza_tokens as f64 / Self::STANZA_TOKENS as f64 * 1000.0).round() / 1000.0,
            "stanzaTokens": self.stanza_tokens,
            "watch": self.watch,
            "watchHex": self.watch.map(|w| format!("{w:#x}")),
            "measure": self.measure,
            "witnessedAt": self.witnessed_at,
            "evidence": self.evidence,
        })
    }
}

/// The authorship block flattened into `DocumentRecord`. Every field defaults,
/// so records written before the chronicle existed read back unchanged (their
/// `createdBy` stays honestly unknown — it is never guessed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentChronicle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_by: Option<ChronicleActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_by: Option<ChronicleActor>,
    /// Most recent first; one entry per distinct actor; capped at
    /// [`RECENT_AUTHORS_CAP`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) recent_authors: Vec<ChronicleAuthor>,
    /// Where `created_at` came from when it was not assigned by this
    /// authority at creation: `archive-chronicle` (restored from an exported
    /// chronicle entry), `archive-catalog` (best evidence: the exported
    /// workspace catalog's `createdAt`), or `import` (no evidence — the import
    /// instant, flagged so nobody mistakes it for a birthday).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_at_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_forced_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_forced_by: Option<ChronicleActor>,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub(crate) forced_writes: u64,
    /// Agentic time at the first authored write and at the latest one, as
    /// the Cantor last witnessed it. Absent when the Cantor was not running,
    /// and always absent on imported documents: agentic time did not exist
    /// when they were written, and the cell never guesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_clock: Option<AgenticTimeStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_clock: Option<AgenticTimeStamp>,
}

impl DocumentChronicle {
    pub(crate) fn created_by(actor: ChronicleActor) -> Self {
        Self {
            created_by: Some(actor),
            ..Self::default()
        }
    }

    /// Stamp one write. `forced` records an explicit staleness-guard bypass —
    /// override is possible but never silent.
    pub(crate) fn record_write(
        &mut self,
        actor: &ChronicleActor,
        at: &str,
        revision: u64,
        forced: bool,
    ) {
        self.updated_by = Some(actor.clone());
        if let Some(index) = self
            .recent_authors
            .iter()
            .position(|author| author.actor == *actor)
        {
            let mut author = self.recent_authors.remove(index);
            author.last_at = at.to_string();
            author.last_revision = revision;
            author.writes = author.writes.saturating_add(1);
            self.recent_authors.insert(0, author);
        } else {
            self.recent_authors.insert(
                0,
                ChronicleAuthor {
                    actor: actor.clone(),
                    first_at: at.to_string(),
                    last_at: at.to_string(),
                    last_revision: revision,
                    writes: 1,
                },
            );
            self.recent_authors.truncate(RECENT_AUTHORS_CAP);
        }
        if forced {
            self.last_forced_at = Some(at.to_string());
            self.last_forced_by = Some(actor.clone());
            self.forced_writes = self.forced_writes.saturating_add(1);
        }
    }

    /// Stamp agentic time on one write. The creation stamp is set once, by
    /// the first authored write, and never moves; the update stamp follows
    /// every write. `None` (no witness) leaves both untouched — a stamp is
    /// never cleared by a later write the Cantor missed.
    pub(crate) fn record_clock(&mut self, first_authored_write: bool, clock: Option<AgenticTimeStamp>) {
        if let Some(clock) = clock {
            if first_authored_write && self.created_clock.is_none() {
                self.created_clock = Some(clock.clone());
            }
            self.updated_clock = Some(clock);
        }
    }

    /// The MCP-facing shape: labels alongside the structured actors so a
    /// reader can scan `human:owner` / `agent:beta` without unpacking objects.
    pub(crate) fn to_json(&self) -> Value {
        let actor_json = |actor: &Option<ChronicleActor>| match actor {
            Some(actor) => json!({
                "kind": actor.kind,
                "name": actor.name,
                "via": actor.via,
                "label": actor.label(),
            }),
            None => Value::Null,
        };
        json!({
            "createdBy": actor_json(&self.created_by),
            "updatedBy": actor_json(&self.updated_by),
            "recentAuthors": self
                .recent_authors
                .iter()
                .map(|author| json!({
                    "actor": actor_json(&Some(author.actor.clone())),
                    "label": author.actor.label(),
                    "firstAt": author.first_at,
                    "lastAt": author.last_at,
                    "lastRevision": author.last_revision,
                    "writes": author.writes,
                }))
                .collect::<Vec<_>>(),
            "createdAtSource": self.created_at_source,
            "lastForcedAt": self.last_forced_at,
            "lastForcedBy": actor_json(&self.last_forced_by),
            "forcedWrites": self.forced_writes,
            "createdClock": self.created_clock.as_ref().map(AgenticTimeStamp::to_json),
            "updatedClock": self.updated_clock.as_ref().map(AgenticTimeStamp::to_json),
        })
    }
}

/// One document's chronicle as it travels in a graph archive
/// (`chronicle/documents/{id}.json`). Additive format extension: absent in
/// older archives, ignored by older importers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChronicleEntry {
    pub(crate) document_id: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) revision: u64,
    #[serde(flatten)]
    pub(crate) chronicle: DocumentChronicle,
}

impl ChronicleEntry {
    pub(crate) fn from_record(record: &DocumentRecord) -> Self {
        Self {
            document_id: record.document_id.clone(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            revision: record.revision,
            chronicle: record.chronicle.clone(),
        }
    }
}

/// What an import asks `save_document` to restore instead of stamping the
/// import instant. `source` lands in `createdAtSource` so the provenance of
/// the clock is itself recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChronicleRestore {
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) revision: u64,
    pub(crate) source: String,
    #[serde(flatten)]
    pub(crate) chronicle: DocumentChronicle,
}

impl ChronicleRestore {
    pub(crate) fn from_entry(entry: &ChronicleEntry) -> Self {
        Self {
            created_at: entry.created_at.clone(),
            updated_at: entry.updated_at.clone(),
            revision: entry.revision,
            source: if entry.chronicle.created_at_source.as_deref() == Some("import") {
                "import".to_string()
            } else {
                "archive-chronicle".to_string()
            },
            chronicle: entry.chronicle.clone(),
        }
    }

    /// Best available evidence when an archive predates the chronicle: the
    /// exported workspace catalog's epoch-ms `createdAt` / `updatedAt`.
    pub(crate) fn from_catalog_entry(entry: &Value) -> Option<Self> {
        let created_ms = entry.get("createdAt").and_then(Value::as_f64)?;
        if !created_ms.is_finite() || created_ms <= 0.0 {
            return None;
        }
        // Some cloud-1 catalogs carry createdAt 1000 times too large (a
        // year near 25000). A creation time after the import is not
        // evidence; fall back to the import instant, flagged as such.
        let now_ms = crate::clock::epoch_millis() as f64;
        if created_ms > now_ms + 24.0 * 60.0 * 60.0 * 1000.0 {
            return None;
        }
        let updated_ms = entry
            .get("updatedAt")
            .and_then(Value::as_f64)
            .filter(|value| {
                value.is_finite()
                    && *value >= created_ms
                    && *value <= now_ms + 24.0 * 60.0 * 60.0 * 1000.0
            })
            .unwrap_or(created_ms);
        Some(Self {
            created_at: format!("{}", created_ms as u128),
            updated_at: format!("{}", updated_ms as u128),
            revision: 0,
            source: "archive-catalog".to_string(),
            chronicle: DocumentChronicle::default(),
        })
    }

    /// No evidence at all: the import instant, flagged as such.
    pub(crate) fn import_instant() -> Self {
        let now = timestamp();
        Self {
            created_at: now.clone(),
            updated_at: now,
            revision: 0,
            source: "import".to_string(),
            chronicle: DocumentChronicle::default(),
        }
    }

    pub(crate) fn apply(self, record: &mut DocumentRecord) {
        if !self.created_at.trim().is_empty() {
            record.created_at = self.created_at;
        }
        if !self.updated_at.trim().is_empty() {
            record.updated_at = self.updated_at;
        }
        record.revision = record.revision.max(self.revision);
        record.chronicle = self.chronicle;
        record.chronicle.created_at_source = Some(self.source);
    }
}

/// Per-write hints that ride the CRDT operation payload into persistence:
/// the actor, the explicit `force` override, and (import only) a restore.
#[derive(Debug, Clone, Default)]
pub(crate) struct WriteHints {
    pub(crate) actor: Option<Value>,
    pub(crate) force: bool,
    pub(crate) restore: Option<Value>,
}

impl WriteHints {
    pub(crate) fn from_payload(payload: &Value) -> Self {
        Self {
            actor: payload
                .get("actor")
                .filter(|value| ChronicleActor::from_value(value).is_some())
                .cloned(),
            force: payload_force(payload),
            restore: payload
                .get("chronicleRestore")
                .filter(|value| value.is_object())
                .cloned(),
        }
    }

    /// Stamp the hints onto a `save_document` candidate payload.
    pub(crate) fn apply_to_candidate(&self, candidate: &mut Value) {
        let Some(object) = candidate.as_object_mut() else {
            return;
        };
        if let Some(actor) = &self.actor {
            object.insert("actor".to_string(), actor.clone());
        }
        if self.force {
            object.insert("force".to_string(), Value::Bool(true));
        }
        if let Some(restore) = &self.restore {
            object.insert("chronicleRestore".to_string(), restore.clone());
        }
    }
}

/// `force` / `force_write` on a payload or MCP argument bag, JS-truthy.
pub(crate) fn payload_force(value: &Value) -> bool {
    ["force", "force_write", "forceWrite"]
        .iter()
        .filter_map(|key| value.get(*key))
        .any(|flag| match flag {
            Value::Bool(flag) => *flag,
            Value::String(text) => matches!(text.trim(), "true" | "1" | "yes"),
            Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
            _ => false,
        })
}

/// `expectedRevision` / `expected_revision` on a payload, or `None` when
/// absent or null. A malformed value is an error: the caller asked for a
/// guard and got the syntax wrong.
pub(crate) fn payload_expected_revision(value: &Value) -> Result<Option<u64>, String> {
    let Some(raw) = value
        .get("expectedRevision")
        .or_else(|| value.get("expected_revision"))
        .filter(|raw| !raw.is_null())
    else {
        return Ok(None);
    };
    raw.as_u64()
        .map(Some)
        .ok_or_else(|| "expectedRevision must be a non-negative integer".to_string())
}

/// The staleness guard, as one shared verdict: `expected` is what the caller
/// last read, `current` is the record now. Equality at `expected + 1` is
/// the same-operation replay `next_revision` tolerates.
pub(crate) fn revision_guard(expected: Option<u64>, current: u64, force: bool) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if force || current == expected || current == expected.saturating_add(1) {
        return Ok(());
    }
    Err(format!(
        "revision conflict: expected {expected}, actual {current}; the document changed since your read — re-read it, or pass force=true to override (the override is recorded)"
    ))
}

// ---------------------------------------------------------------------------
// Principal plumbing
// ---------------------------------------------------------------------------

tokio::task_local! {
    static CURRENT_ACTOR: ChronicleActor;
}

/// The actor for the current request, if one was established.
pub(crate) fn current_actor() -> Option<ChronicleActor> {
    CURRENT_ACTOR.try_with(|actor| actor.clone()).ok()
}

/// Run `future` with `actor` as the current request principal.
pub(crate) async fn with_actor<F: Future>(actor: ChronicleActor, future: F) -> F::Output {
    CURRENT_ACTOR.scope(actor, future).await
}

/// Called at the CRDT enqueue choke point: the server-resolved actor always
/// wins over anything a client wrote into the payload. Without a request
/// principal (internal callers, tests, recovery replay) the payload is left
/// alone and persistence falls back to the human owner.
pub(crate) fn inject_actor(payload: &mut Value) {
    let Some(actor) = current_actor() else {
        return;
    };
    if let Some(object) = payload.as_object_mut() {
        object.insert("actor".to_string(), actor.to_value());
    }
}

/// The actor an MCP call writes as: an explicit `agent` argument names the
/// author (the credential it rode in on is kept as `via`); otherwise the
/// request principal; otherwise the human owner.
pub(crate) fn resolve_mcp_actor(arguments: &Value) -> ChronicleActor {
    let principal = current_actor().unwrap_or_else(ChronicleActor::human_owner);
    let explicit = ["agent", "agent_name", "agentName", "actor_name", "actorName"]
        .iter()
        .filter_map(|key| arguments.get(*key))
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|name| !name.is_empty());
    match explicit {
        Some(name) if principal.kind != ACTOR_KIND_AGENT || principal.name != name => {
            ChronicleActor::agent(name, Some(principal.label()))
        }
        _ => principal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_archive_transport_keeps_guessed_creation_time_flagged() {
        let entry = ChronicleEntry {
            document_id: "doc".into(), created_at: "1000".into(), updated_at: "2000".into(), revision: 4,
            chronicle: DocumentChronicle { created_at_source: Some("import".into()), ..Default::default() },
        };
        let restored = ChronicleRestore::from_entry(&entry);
        assert_eq!(restored.source, "import");
        assert_eq!(restored.created_at, "1000");
        assert_eq!(restored.updated_at, "2000");
        let native = ChronicleEntry { chronicle: DocumentChronicle::default(), ..entry };
        assert_eq!(ChronicleRestore::from_entry(&native).source, "archive-chronicle");
    }

    #[test]
    fn actor_labels_and_value_round_trip() {
        let beta = ChronicleActor::agent("beta", Some("client-token:tok-1".to_string()));
        assert_eq!(beta.label(), "agent:beta");
        assert_eq!(
            ChronicleActor::from_value(&beta.to_value()),
            Some(beta.clone())
        );
        assert_eq!(
            ChronicleActor::from_value(&json!("human:owner")),
            Some(ChronicleActor::human_owner())
        );
        assert_eq!(ChronicleActor::from_value(&json!("nonsense")), None);
        assert_eq!(ChronicleActor::from_value(&json!({ "kind": "", "name": "x" })), None);
        assert_eq!(ChronicleActor::from_value(&json!(42)), None);
    }

    #[test]
    fn record_write_keeps_a_bounded_most_recent_first_author_list() {
        let mut chronicle = DocumentChronicle::default();
        let owner = ChronicleActor::human_owner();
        chronicle.record_write(&owner, "10", 1, false);
        for index in 0..RECENT_AUTHORS_CAP + 3 {
            let agent = ChronicleActor::agent(format!("agent-{index}"), None);
            chronicle.record_write(&agent, &format!("{}", 20 + index), 2 + index as u64, false);
        }
        assert_eq!(chronicle.recent_authors.len(), RECENT_AUTHORS_CAP);
        assert_eq!(
            chronicle.recent_authors[0].actor.name,
            format!("agent-{}", RECENT_AUTHORS_CAP + 2)
        );
        assert!(
            chronicle
                .recent_authors
                .iter()
                .all(|author| author.actor != owner),
            "the oldest author was ejected"
        );

        // A returning author moves to the front and accumulates, never
        // duplicating.
        let returning = chronicle.recent_authors[3].actor.clone();
        chronicle.record_write(&returning, "99", 50, true);
        assert_eq!(chronicle.recent_authors[0].actor, returning);
        assert_eq!(chronicle.recent_authors[0].writes, 2);
        assert_eq!(chronicle.recent_authors[0].last_revision, 50);
        assert_eq!(
            chronicle
                .recent_authors
                .iter()
                .filter(|author| author.actor == returning)
                .count(),
            1
        );
        assert_eq!(chronicle.updated_by, Some(returning.clone()));
        assert_eq!(chronicle.forced_writes, 1);
        assert_eq!(chronicle.last_forced_by, Some(returning));
        assert_eq!(chronicle.last_forced_at.as_deref(), Some("99"));
    }

    #[test]
    fn chronicle_serde_defaults_keep_legacy_records_readable() {
        let legacy: DocumentChronicle = serde_json::from_value(json!({})).expect("empty block");
        assert_eq!(legacy, DocumentChronicle::default());
        assert_eq!(serde_json::to_value(&legacy).expect("serialize"), json!({}));

        let mut stamped = DocumentChronicle::default();
        stamped.record_write(&ChronicleActor::human_owner(), "5", 1, false);
        let round: DocumentChronicle =
            serde_json::from_value(serde_json::to_value(&stamped).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round, stamped);
    }

    #[test]
    fn revision_guard_matches_next_revision_semantics() {
        assert!(revision_guard(None, 7, false).is_ok());
        assert!(revision_guard(Some(7), 7, false).is_ok());
        assert!(revision_guard(Some(6), 7, false).is_ok(), "replay tolerance");
        let conflict = revision_guard(Some(3), 7, false).unwrap_err();
        assert!(conflict.starts_with("revision conflict: expected 3, actual 7"));
        assert!(conflict.contains("force=true"));
        assert!(revision_guard(Some(3), 7, true).is_ok(), "force bypasses");
    }

    #[test]
    fn payload_helpers_read_aliases_and_reject_malformed_revisions() {
        assert!(payload_force(&json!({ "force": true })));
        assert!(payload_force(&json!({ "force_write": "true" })));
        assert!(!payload_force(&json!({ "force": "no" })));
        assert!(!payload_force(&json!({})));
        assert_eq!(payload_expected_revision(&json!({})).unwrap(), None);
        assert_eq!(
            payload_expected_revision(&json!({ "expected_revision": 4 })).unwrap(),
            Some(4)
        );
        assert_eq!(
            payload_expected_revision(&json!({ "expectedRevision": null })).unwrap(),
            None
        );
        assert!(payload_expected_revision(&json!({ "expectedRevision": -1 })).is_err());
    }

    #[test]
    fn write_hints_round_trip_through_a_candidate_payload() {
        let hints = WriteHints::from_payload(&json!({
            "actor": { "kind": "agent", "name": "beta" },
            "force": true,
            "chronicleRestore": { "createdAt": "1", "updatedAt": "2", "source": "archive-chronicle" },
            "documentId": "doc-a",
        }));
        assert!(hints.force);
        let mut candidate = json!({ "documentId": "doc-a" });
        hints.apply_to_candidate(&mut candidate);
        assert_eq!(candidate["actor"]["name"], "beta");
        assert_eq!(candidate["force"], true);
        assert_eq!(candidate["chronicleRestore"]["source"], "archive-chronicle");

        // Junk actors are dropped, not carried.
        let junk = WriteHints::from_payload(&json!({ "actor": 12 }));
        assert!(junk.actor.is_none());
        let mut untouched = json!({ "documentId": "doc-a" });
        WriteHints::default().apply_to_candidate(&mut untouched);
        assert_eq!(untouched, json!({ "documentId": "doc-a" }));
    }

    #[test]
    fn restore_from_catalog_uses_epoch_millis_and_flags_the_source() {
        let restore = ChronicleRestore::from_catalog_entry(&json!({
            "createdAt": 1700000000000.0,
            "updatedAt": 1700000005000.0,
        }))
        .expect("catalog dates");
        assert_eq!(restore.created_at, "1700000000000");
        assert_eq!(restore.updated_at, "1700000005000");
        assert_eq!(restore.source, "archive-catalog");
        assert!(ChronicleRestore::from_catalog_entry(&json!({ "createdAt": null })).is_none());
        // An updatedAt before createdAt is not trusted over createdAt.
        let clamped = ChronicleRestore::from_catalog_entry(&json!({
            "createdAt": 50.0,
            "updatedAt": 10.0,
        }))
        .expect("catalog dates");
        assert_eq!(clamped.updated_at, "50");
        // A creation time after the import (cloud-1's 1000x values) is not
        // evidence: the caller falls back to the import instant.
        assert!(ChronicleRestore::from_catalog_entry(&json!({
            "createdAt": 946713600000000.0,
        }))
        .is_none());
        // A sane createdAt with an impossible updatedAt keeps createdAt.
        let bounded = ChronicleRestore::from_catalog_entry(&json!({
            "createdAt": 1700000000000.0,
            "updatedAt": 946713600000000.0,
        }))
        .expect("catalog dates");
        assert_eq!(bounded.updated_at, "1700000000000");
    }

    #[tokio::test]
    async fn task_local_actor_scopes_and_injects() {
        assert!(current_actor().is_none());
        let mut payload = json!({ "documentId": "doc-a", "actor": "spoofed:client" });
        inject_actor(&mut payload);
        assert_eq!(payload["actor"], "spoofed:client", "no principal: payload untouched");

        let beta = ChronicleActor::agent("beta", Some("master-token".to_string()));
        with_actor(beta.clone(), async {
            assert_eq!(current_actor(), Some(beta.clone()));
            let mut payload = json!({ "documentId": "doc-a", "actor": "spoofed:client" });
            inject_actor(&mut payload);
            assert_eq!(
                ChronicleActor::from_value(&payload["actor"]),
                Some(beta.clone()),
                "the server-resolved actor overwrites a client-supplied one"
            );
        })
        .await;
        assert!(current_actor().is_none());
    }

    #[tokio::test]
    async fn mcp_actor_resolution_prefers_an_explicit_agent_name() {
        // No principal, no argument: the human owner.
        assert_eq!(resolve_mcp_actor(&json!({})), ChronicleActor::human_owner());
        // No principal, explicit agent: an agent riding the owner's session.
        let named = resolve_mcp_actor(&json!({ "agent": "beta" }));
        assert_eq!(named.label(), "agent:beta");
        assert_eq!(named.via.as_deref(), Some("human:owner"));

        with_actor(ChronicleActor::human_owner_via("master-token"), async {
            assert_eq!(
                resolve_mcp_actor(&json!({ "content": "x" })),
                ChronicleActor::human_owner_via("master-token")
            );
            let refined = resolve_mcp_actor(&json!({ "agent_name": " gamma " }));
            assert_eq!(refined.label(), "agent:gamma");
        })
        .await;

        with_actor(
            ChronicleActor::agent("hivemind", Some("client-token:tok".to_string())),
            async {
                let same = resolve_mcp_actor(&json!({ "agent": "hivemind" }));
                assert_eq!(same.via.as_deref(), Some("client-token:tok"), "same name keeps the credential");
                let other = resolve_mcp_actor(&json!({ "agent": "delta" }));
                assert_eq!(other.label(), "agent:delta");
                assert_eq!(other.via.as_deref(), Some("agent:hivemind"));
            },
        )
        .await;
    }
}
