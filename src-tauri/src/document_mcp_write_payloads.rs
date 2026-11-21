use crate::json_utils::json_string;

/// How a write's durable-survival claim was actually paid.
///
/// Until 2026-08-24 the response's `durabilityChecked` field was computed
/// from the caller's own `awaitDurable` request flag — intent echoed back as
/// a receipt (the 2026-08-21 incident shape: 16.5h of writes acknowledged as
/// durable were later discarded by a lease fence with no revocation, because
/// nothing ever tied the ack to an actual publication). The claim is now
/// bound to real events only; see [`write_durability_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteDurability {
    /// The caller declined the check (`awaitDurable=false`).
    Unchecked,
    /// Durable-plane semantics: every epoch the awaited write's persists marked is covered by
    /// an actual flush completion — a snapshot that survived Gate C
    /// `publish_commit`, or a confirmed-clean pass over dirty state (see
    /// `cell_durability::durably_resolved_epoch`).
    Published,
    /// Durable-plane semantics: no completed flush covers this write yet.
    /// The write succeeded and will be captured by the dirty-driven flusher
    /// (debounce/max-RPO bounded), but its durable survival has NOT been
    /// checked — saying otherwise is exactly the defect this enum retired.
    Pending,
    /// No durable plane in this process (desktop build): the local profile
    /// dir on the user's disk is the durable ground, and the awaited CRDT
    /// write + projection flush already landed there (either failing would
    /// have failed the call before a response was built).
    LocalDisk,
}

impl WriteDurability {
    /// The boolean the wire has always carried. `true` now means "the
    /// requested durable-survival check ran and passed", never "the caller
    /// asked for it".
    pub(crate) fn checked(self) -> bool {
        matches!(self, Self::Published | Self::LocalDisk)
    }

    /// Additive wire field naming how the claim was (or was not) paid.
    pub(crate) fn state(self) -> &'static str {
        match self {
            Self::Unchecked => "unchecked",
            Self::Published => "published",
            Self::Pending => "pending",
            Self::LocalDisk => "local",
        }
    }
}

/// Pure classifier binding the durability claim to actual events.
///
/// * `requested` — the caller's `awaitDurable` flag: it selects whether the
///   check is *reported*, and can never manufacture a positive result.
/// * `durable_plane` — `cell_durability::durable_plane_semantics()`: whether
///   this process must be held to durable-plane (published snapshot) truth.
/// * `resolved_epoch` — `cell_durability::durably_resolved_epoch()`, the
///   commit-backed watermark advanced only by completed flushes.
/// * `write_epoch` — a `current_write_epoch()` value read AFTER the write's
///   CRDT operation and projection flush were awaited, so it is `>=` every
///   completion mark the write produced (reading it late over-counts
///   concurrent unrelated writes — conservative, never a false claim).
pub(crate) fn write_durability_verdict(
    requested: bool,
    durable_plane: bool,
    resolved_epoch: u64,
    write_epoch: u64,
) -> WriteDurability {
    if !requested {
        return WriteDurability::Unchecked;
    }
    if !durable_plane {
        return WriteDurability::LocalDisk;
    }
    if resolved_epoch >= write_epoch {
        WriteDurability::Published
    } else {
        WriteDurability::Pending
    }
}

pub(super) fn mcp_write_document_payload(
    arguments: &serde_json::Value,
    document_id: &str,
) -> Result<serde_json::Value, String> {
    let content = arguments.get("content");
    let tiptap_json = arguments
        .get("tiptapJson")
        .or_else(|| arguments.get("tiptap_json"));
    if content.is_some() && tiptap_json.is_some() {
        return Err("write_document accepts exactly one of content or tiptapJson".to_string());
    }

    let mut payload = serde_json::json!({
        "documentId": document_id,
        "comments": arguments.get("comments").cloned().unwrap_or_else(|| serde_json::json!({})),
        // Carried through to the CRDT operation payload for shape parity; the
        // executor does not consult it. The flag's real effect is on the
        // RESPONSE: it selects whether the durable-survival check is reported
        // (see `write_durability_verdict`) — it never manufactures one.
        "awaitDurable": arguments.get("awaitDurable").or_else(|| arguments.get("await_durable")).cloned().unwrap_or(serde_json::Value::Bool(true)),
    });
    for key in ["operationId", "_inlineParsedContent", "_inlineValuationWrite", "_inlineSourceFormat"] {
        if let Some(value) = arguments.get(key) { payload[key] = value.clone(); }
    }
    if let Some(title) = arguments.get("title") {
        if !title.is_string() {
            return Err("title must be a string".to_string());
        }
        payload["title"] = title.clone();
    }
    // Placement. Named: the document is filed there, and an existing one is
    // moved there; an explicit null is the root. Left out: a new document
    // goes to the root and an existing one stays where it is.
    if let Some(parent) = arguments
        .get("parentId")
        .or_else(|| arguments.get("parent_id"))
    {
        match parent.as_str() {
            Some(folder_id) if !folder_id.trim().is_empty() => {}
            None if parent.is_null() => {}
            _ => return Err("parentId must be a folder id, or null for the root".to_string()),
        }
        payload["parentId"] = parent.clone();
    }
    if let Some(expected_revision) = arguments
        .get("expectedRevision")
        .or_else(|| arguments.get("expected_revision"))
    {
        if !expected_revision.is_null() && expected_revision.as_u64().is_none() {
            return Err("expectedRevision must be a non-negative integer".to_string());
        }
        payload["expectedRevision"] = expected_revision.clone();
    }
    // Chronicle (design Part 3): `force=true` bypasses the staleness guard
    // explicitly. The guard is dropped from the payload and the override is
    // carried so persistence records it — never silent.
    if crate::document_chronicle::payload_force(arguments) {
        payload["force"] = serde_json::Value::Bool(true);
        if let Some(object) = payload.as_object_mut() {
            object.remove("expectedRevision");
        }
    }
    match (content, tiptap_json) {
        (Some(content), None) => {
            let Some(content) = content.as_str() else {
                return Err("content must be a string".to_string());
            };
            payload["content"] = serde_json::json!(content);
            payload["format"] = arguments
                .get("format")
                .cloned()
                .unwrap_or_else(|| serde_json::json!("auto"));
        }
        (None, Some(tiptap_json)) => {
            if !tiptap_json.is_object() {
                return Err("tiptapJson must be an object".to_string());
            }
            payload["tiptapJson"] = tiptap_json.clone();
        }
        (None, None) => return Err("content or tiptapJson is required".to_string()),
        (Some(_), Some(_)) => unreachable!("checked above"),
    }
    Ok(payload)
}

/// How many characters of a block's stored text ride in a write ack.
pub(crate) const BLOCK_PREVIEW_CHARS: usize = 10;
/// Past this many blocks a write ack carries ids only; the previews would
/// outweigh the document.
pub(crate) const BLOCK_PREVIEW_MAX_BLOCKS: usize = 400;

/// The first `BLOCK_PREVIEW_CHARS` characters of a text with whitespace runs
/// collapsed to one space, built incrementally: the iterator is consumed only
/// until the head is full, so a block of any length costs the same. Both the
/// write ack (stored record) and the block ops (fragment leaves) use this.
pub(crate) fn preview_head(chars: impl Iterator<Item = char>) -> String {
    let mut head = String::new();
    let mut pending_space = false;
    for ch in chars {
        if ch.is_whitespace() {
            pending_space = !head.is_empty();
            continue;
        }
        if pending_space {
            if head.chars().count() + 1 >= BLOCK_PREVIEW_CHARS {
                break;
            }
            head.push(' ');
            pending_space = false;
        }
        head.push(ch);
        if head.chars().count() >= BLOCK_PREVIEW_CHARS {
            break;
        }
    }
    head
}

/// One line per written block, in document order: the block's FULL id (what a
/// caller copies into its next call; the engine matches ids exactly) and the
/// first characters of the text as STORED — after the content conversion,
/// which is where block boundaries move. This is the answer to "which id is
/// which sentence" without a second read: the write handler already holds the
/// post-write record these come from.
pub(crate) fn block_previews(blocks: &[serde_json::Value]) -> serde_json::Value {
    if blocks.len() > BLOCK_PREVIEW_MAX_BLOCKS {
        return serde_json::json!({
            "omitted": blocks.len(),
            "reason": format!("more than {BLOCK_PREVIEW_MAX_BLOCKS} blocks; use blockIds"),
        });
    }
    let lines = blocks
        .iter()
        .filter_map(|block| {
            let id = json_string(block.get("id"))?;
            let text = block
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let head = preview_head(text.chars());
            Some(serde_json::Value::String(if head.is_empty() {
                id
            } else {
                format!("{id} {head}")
            }))
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(lines)
}

pub(super) fn mcp_write_document_response(
    arguments: &serde_json::Value,
    graph_id: &str,
    document_id: &str,
    document: &serde_json::Value,
    outcome_value: &serde_json::Value,
    durability: WriteDurability,
) -> serde_json::Value {
    let blocks = document
        .get("blocks")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let block_ids = blocks
        .iter()
        .filter_map(|block| json_string(block.get("id")))
        .collect::<Vec<_>>();
    let previews = block_previews(&blocks);
    let warnings = outcome_value
        .get("warnings")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let source_format = outcome_value
        .get("sourceFormat")
        .cloned()
        .unwrap_or_else(|| {
            if arguments
                .get("tiptapJson")
                .or_else(|| arguments.get("tiptap_json"))
                .is_some_and(serde_json::Value::is_object)
            {
                serde_json::json!("tiptap-json")
            } else {
                serde_json::Value::Null
            }
        });

    serde_json::json!({
        "success": true,
        "graph_id": graph_id,
        "graphId": graph_id,
        "document_id": document_id,
        "documentId": document_id,
        "title": document.get("title").cloned().unwrap_or_else(|| serde_json::json!(document_id)),
        "revision": document.get("revision").cloned().unwrap_or(serde_json::Value::Null),
        "block_ids": block_ids.clone(),
        "blockIds": block_ids.clone(),
        "blocks": previews,
        "durability_checked": durability.checked(),
        "durabilityChecked": durability.checked(),
        "durability": durability.state(),
        "source_format": source_format.clone(),
        "sourceFormat": source_format.clone(),
        "warnings": warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_document_payload_preserves_defaults_and_aliases() {
        assert_eq!(
            mcp_write_document_payload(&serde_json::json!({ "content": "hello" }), "doc-a")
                .unwrap(),
            serde_json::json!({
                "documentId": "doc-a",
                "content": "hello",
                "format": "auto",
                "comments": {},
                "awaitDurable": true,
            })
        );

        assert_eq!(
            mcp_write_document_payload(
                &serde_json::json!({
                    "content": "hello",
                    "format": "markdown",
                    "comments": { "c1": "note" },
                    "await_durable": false,
                }),
                "doc-a"
            )
            .unwrap(),
            serde_json::json!({
                "documentId": "doc-a",
                "content": "hello",
                "format": "markdown",
                "comments": { "c1": "note" },
                "awaitDurable": false,
            })
        );
    }

    #[test]
    fn write_document_payload_preserves_tiptap_json_without_string_roundtrip() {
        let tiptap_json = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": { "data-block-id": "block-a" },
                "content": [{ "type": "text", "text": "hello" }]
            }]
        });
        let payload = mcp_write_document_payload(
            &serde_json::json!({
                "title": "A Document",
                "tiptapJson": tiptap_json,
            }),
            "doc-a",
        )
        .unwrap();

        assert_eq!(payload["title"], "A Document");
        assert_eq!(payload["tiptapJson"], tiptap_json);
        assert!(payload.get("content").is_none());
        assert!(payload.get("format").is_none());
    }

    #[test]
    fn write_document_payload_preserves_expected_revision() {
        let payload = mcp_write_document_payload(
            &serde_json::json!({
                "content": "hello",
                "expected_revision": 7,
            }),
            "doc-a",
        )
        .unwrap();
        assert_eq!(payload.get("expectedRevision"), Some(&serde_json::json!(7)));

        assert!(mcp_write_document_payload(
            &serde_json::json!({
                "content": "hello",
                "expectedRevision": -1,
            }),
            "doc-a",
        )
        .is_err());
    }

    /// Chronicle Part 3: `force` drops the guard from the payload and rides
    /// through so the persistence layer records the override.
    #[test]
    fn write_document_payload_force_bypasses_expected_revision_and_is_carried() {
        let payload = mcp_write_document_payload(
            &serde_json::json!({
                "content": "hello",
                "expected_revision": 7,
                "force": true,
            }),
            "doc-a",
        )
        .unwrap();
        assert!(payload.get("expectedRevision").is_none());
        assert_eq!(payload.get("force"), Some(&serde_json::json!(true)));

        let unforced = mcp_write_document_payload(
            &serde_json::json!({ "content": "hello", "expected_revision": 7 }),
            "doc-a",
        )
        .unwrap();
        assert!(unforced.get("force").is_none());
        assert_eq!(unforced.get("expectedRevision"), Some(&serde_json::json!(7)));
    }

    #[test]
    fn write_document_payload_rejects_missing_ambiguous_or_malformed_content() {
        assert!(mcp_write_document_payload(&serde_json::json!({}), "doc-a").is_err());
        assert!(mcp_write_document_payload(
            &serde_json::json!({ "content": "hello", "tiptapJson": { "type": "doc" } }),
            "doc-a"
        )
        .is_err());
        assert!(mcp_write_document_payload(
            &serde_json::json!({ "tiptapJson": "not an object" }),
            "doc-a"
        )
        .is_err());
    }

    #[test]
    fn block_previews_name_each_block_by_its_stored_text() {
        let blocks = serde_json::json!([
            { "id": "block-e1998a25", "content": "Marker probe" },
            { "id": "block-e0998892", "content": "  This   line carries\nan inline marker." },
            { "id": "block-df9986ff", "content": "" },
            { "id": "block-ünï", "content": "héllo wörld and more" },
        ]);
        let previews = block_previews(blocks.as_array().unwrap());
        assert_eq!(
            previews,
            serde_json::json!([
                "block-e1998a25 Marker pro",
                "block-e0998892 This line",
                "block-df9986ff",
                "block-ünï héllo wörl",
            ])
        );
    }

    #[test]
    fn preview_head_is_bounded_and_collapses_whitespace_incrementally() {
        assert_eq!(preview_head("  This   line carries\nan inline marker.".chars()), "This line");
        assert_eq!(preview_head("Marker probe".chars()), "Marker pro");
        assert_eq!(preview_head("   ".chars()), "");
        assert_eq!(preview_head("héllo wörld and more".chars()), "héllo wörl");
        // A long single run of text: the iterator is not exhausted. A
        // side-effecting iterator proves how far it was read.
        let mut consumed = 0usize;
        let long = std::iter::repeat('x').take(1_000_000).inspect(|_| consumed += 1);
        assert_eq!(preview_head(long), "xxxxxxxxxx");
        assert!(consumed <= BLOCK_PREVIEW_CHARS + 1, "read {consumed} chars for a 10-char head");
    }

    #[test]
    fn block_previews_are_capped_by_block_count() {
        let many = (0..=BLOCK_PREVIEW_MAX_BLOCKS)
            .map(|i| serde_json::json!({ "id": format!("block-{i}"), "content": "x" }))
            .collect::<Vec<_>>();
        let previews = block_previews(&many);
        assert_eq!(previews["omitted"], BLOCK_PREVIEW_MAX_BLOCKS + 1);
        assert!(previews["reason"].as_str().unwrap().contains("use blockIds"));
    }

    #[test]
    fn write_document_response_reports_block_ids_and_outcome_metadata() {
        let response = mcp_write_document_response(
            &serde_json::json!({ "await_durable": false }),
            "graph-a",
            "doc-a",
            &serde_json::json!({
                "title": "Doc",
                "revision": 4,
                "blocks": [
                    { "id": "block-a" },
                    { "id": 12 },
                    { "missing": true }
                ],
            }),
            &serde_json::json!({
                "sourceFormat": "markdown",
                "warnings": ["converted"],
            }),
            WriteDurability::Unchecked,
        );

        assert_eq!(response.get("success"), Some(&serde_json::json!(true)));
        assert_eq!(response.get("revision"), Some(&serde_json::json!(4)));
        assert_eq!(
            response.get("block_ids"),
            Some(&serde_json::json!(["block-a", "12"]))
        );
        assert_eq!(
            response.get("blockIds"),
            Some(&serde_json::json!(["block-a", "12"]))
        );
        assert_eq!(
            response.get("blocks"),
            Some(&serde_json::json!(["block-a", "12"])),
            "blocks without text preview as bare full ids"
        );
        assert_eq!(
            response.get("durability_checked"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            response.get("durability"),
            Some(&serde_json::json!("unchecked"))
        );
        assert_eq!(
            response.get("source_format"),
            Some(&serde_json::json!("markdown"))
        );
        assert_eq!(
            response.get("warnings"),
            Some(&serde_json::json!(["converted"]))
        );
    }

    /// The verdict is bound to actual events. The request flag can suppress
    /// the check but can NEVER manufacture a positive; the durable-plane
    /// claim is paid only by a resolved watermark at or past the write's
    /// epoch; a plane-less (desktop) process pays from the local ground.
    #[test]
    fn write_durability_verdict_pays_only_actual_publication() {
        // Caller declined the check — reported as unchecked regardless of
        // how good or bad the plane's state is.
        assert_eq!(
            write_durability_verdict(false, true, 100, 1),
            WriteDurability::Unchecked
        );
        assert_eq!(
            write_durability_verdict(false, false, 0, 1),
            WriteDurability::Unchecked
        );

        // Durable-plane semantics: the claim is paid exactly when the
        // commit-backed watermark covers the write's epoch.
        assert_eq!(
            write_durability_verdict(true, true, 7, 7),
            WriteDurability::Published
        );
        assert_eq!(
            write_durability_verdict(true, true, 8, 7),
            WriteDurability::Published
        );
        assert_eq!(
            write_durability_verdict(true, true, 6, 7),
            WriteDurability::Pending
        );
        // Freshly-booted plane (watermark 0): nothing is claimable.
        assert_eq!(
            write_durability_verdict(true, true, 0, 1),
            WriteDurability::Pending
        );

        // No durable plane (desktop): local profile dir is the ground.
        assert_eq!(
            write_durability_verdict(true, false, 0, 7),
            WriteDurability::LocalDisk
        );
    }

    /// Regression for the 2026-08-21 incident (and the W2 sitting's FIX NOW
    /// ruling): a write requested with `awaitDurable=true` whose durable
    /// flush has NOT completed must not receive a success response claiming
    /// durability. Before this fix the response computed `durabilityChecked`
    /// from the request flag itself, so this exact assertion — success ack
    /// present, durability claim absent — was unsatisfiable: 16.5h of writes
    /// were acked as durable and later discarded by a lease fence with
    /// nothing revoked.
    #[test]
    fn pending_durable_write_never_claims_durability_2026_08_21() {
        // The caller asked for the check; the plane has resolved nothing
        // (flush never completed — the incident shape).
        let verdict = write_durability_verdict(true, true, 0, 42);
        assert_eq!(verdict, WriteDurability::Pending);

        let response = mcp_write_document_response(
            &serde_json::json!({ "awaitDurable": true, "content": "x" }),
            "graph-a",
            "doc-a",
            &serde_json::json!({ "title": "Doc", "revision": 1, "blocks": [] }),
            &serde_json::json!({}),
            verdict,
        );

        // The write itself succeeded...
        assert_eq!(response.get("success"), Some(&serde_json::json!(true)));
        // ...but the durability claim is honestly declined, in both casings,
        // with the additive state naming why.
        assert_eq!(
            response.get("durability_checked"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            response.get("durabilityChecked"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            response.get("durability"),
            Some(&serde_json::json!("pending"))
        );
    }

    /// Each verdict maps to the boolean the wire has always carried plus the
    /// additive `durability` state, and the mapping never consults the
    /// request arguments.
    #[test]
    fn response_maps_each_durability_verdict_honestly() {
        for (verdict, checked, state) in [
            (WriteDurability::Unchecked, false, "unchecked"),
            (WriteDurability::Published, true, "published"),
            (WriteDurability::Pending, false, "pending"),
            (WriteDurability::LocalDisk, true, "local"),
        ] {
            // Adversarial arguments: the request flag disagrees with the
            // verdict in whichever direction would have fooled the old
            // intent-echo path.
            let arguments = serde_json::json!({ "awaitDurable": !checked });
            let response = mcp_write_document_response(
                &arguments,
                "graph-a",
                "doc-a",
                &serde_json::json!({ "title": "Doc", "revision": 1, "blocks": [] }),
                &serde_json::json!({}),
                verdict,
            );
            assert_eq!(
                response.get("durability_checked"),
                Some(&serde_json::json!(checked)),
                "verdict {verdict:?}"
            );
            assert_eq!(
                response.get("durabilityChecked"),
                Some(&serde_json::json!(checked)),
                "verdict {verdict:?}"
            );
            assert_eq!(
                response.get("durability"),
                Some(&serde_json::json!(state)),
                "verdict {verdict:?}"
            );
        }
    }
}
