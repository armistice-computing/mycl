use serde::{Deserialize, Serialize};

/// MCP block-insert payload. `content`, `blocks`, `tiptapJson`, and the other
/// fields are intentionally pass-through `serde_json::Value` shapes: they are
/// forwarded into the CRDT operation, and the inner schemas are negotiated by
/// the block engine — not this surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpBlockInsertPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<serde_json::Value>,
    /// Opaque block content forwarded to the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    /// Free-form format hint accepted by the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<serde_json::Value>,
    /// Opaque block list forwarded to the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<serde_json::Value>,
    /// Opaque TipTap JSON tree forwarded to the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    tiptap_json: Option<serde_json::Value>,
    /// Block id (string in practice; kept as `Value` for null-tolerance).
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<serde_json::Value>,
    /// Numeric insert index (kept as `Value` because callers also pass strings).
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<serde_json::Value>,
    /// Position hint (`"before" | "after" | "child"`) kept opaque to the engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<serde_json::Value>,
    /// Chronicle Part 3 — staleness guard: the revision the caller last read.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_revision: Option<serde_json::Value>,
    /// Chronicle Part 3 — explicit guard bypass; recorded on the record.
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<serde_json::Value>,
}

/// MCP block-update payload. `edits` and `attrs` are pass-through opaque
/// payloads negotiated by the block engine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpBlockUpdatePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<serde_json::Value>,
    /// Opaque edit list (object/array) forwarded to the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    edits: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<serde_json::Value>,
    /// Opaque attribute bag forwarded to the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    attrs: Option<serde_json::Value>,
    /// Chronicle Part 3 — staleness guard: the revision the caller last read.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_revision: Option<serde_json::Value>,
    /// Chronicle Part 3 — explicit guard bypass; recorded on the record.
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<serde_json::Value>,
}

/// MCP block edit-text payload. `operations` is an engine-defined op list and
/// is forwarded opaque.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpBlockEditTextPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<serde_json::Value>,
    /// Opaque text-edit op list forwarded to the block engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    operations: Option<serde_json::Value>,
    /// Chronicle Part 3 — staleness guard: the revision the caller last read.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_revision: Option<serde_json::Value>,
    /// Chronicle Part 3 — explicit guard bypass; recorded on the record.
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<serde_json::Value>,
}

/// MCP block-delete payload. `block_ids` is intentionally an opaque value
/// because callers may pass either a single string or an array of strings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpBlockDeletePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    block_ids: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<serde_json::Value>,
    /// Chronicle Part 3 — staleness guard: the revision the caller last read.
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_revision: Option<serde_json::Value>,
    /// Chronicle Part 3 — explicit guard bypass; recorded on the record.
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<serde_json::Value>,
}

pub(super) fn mcp_block_insert_payload(arguments: &serde_json::Value) -> McpBlockInsertPayload {
    McpBlockInsertPayload {
        operation_id: arguments.get("operationId").cloned(),
        content: arguments.get("content").cloned(),
        format: arguments.get("format").cloned(),
        blocks: arguments.get("blocks").cloned(),
        tiptap_json: arguments
            .get("tiptapJson")
            .or_else(|| arguments.get("tiptap_json"))
            .cloned(),
        block_id: arguments
            .get("blockId")
            .or_else(|| arguments.get("block_id"))
            .cloned(),
        index: arguments.get("index").cloned(),
        position: arguments.get("position").cloned(),
        expected_revision: chronicle_expected_revision(arguments),
        force: chronicle_force(arguments),
    }
}

pub(super) fn mcp_block_update_payload(arguments: &serde_json::Value) -> McpBlockUpdatePayload {
    McpBlockUpdatePayload {
        operation_id: arguments.get("operationId").cloned(),
        edits: arguments
            .get("edits")
            .or_else(|| arguments.get("updates"))
            .cloned(),
        block_id: arguments
            .get("blockId")
            .or_else(|| arguments.get("block_id"))
            .cloned(),
        attrs: arguments
            .get("attrs")
            .or_else(|| arguments.get("attributes"))
            .cloned(),
        expected_revision: chronicle_expected_revision(arguments),
        force: chronicle_force(arguments),
    }
}

pub(super) fn mcp_block_edit_text_payload(
    arguments: &serde_json::Value,
) -> McpBlockEditTextPayload {
    McpBlockEditTextPayload {
        block_id: arguments
            .get("blockId")
            .or_else(|| arguments.get("block_id"))
            .cloned(),
        operations: arguments
            .get("operations")
            .or_else(|| arguments.get("ops"))
            .cloned(),
        expected_revision: chronicle_expected_revision(arguments),
        force: chronicle_force(arguments),
    }
}

pub(super) fn mcp_block_delete_payload(arguments: &serde_json::Value) -> McpBlockDeletePayload {
    McpBlockDeletePayload {
        block_ids: arguments
            .get("blockIds")
            .or_else(|| arguments.get("block_ids"))
            .cloned(),
        block_id: arguments
            .get("blockId")
            .or_else(|| arguments.get("block_id"))
            .cloned(),
        expected_revision: chronicle_expected_revision(arguments),
        force: chronicle_force(arguments),
    }
}

/// `expectedRevision` / `expected_revision`, forwarded verbatim (the engine
/// validates the shape) but null-collapsed so an explicit `null` reads as
/// "no guard".
fn chronicle_expected_revision(arguments: &serde_json::Value) -> Option<serde_json::Value> {
    arguments
        .get("expectedRevision")
        .or_else(|| arguments.get("expected_revision"))
        .filter(|value| !value.is_null())
        .cloned()
}

fn chronicle_force(arguments: &serde_json::Value) -> Option<serde_json::Value> {
    crate::document_chronicle::payload_force(arguments).then_some(serde_json::Value::Bool(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_payload_builders_normalize_mcp_aliases() {
        let insert = serde_json::to_value(mcp_block_insert_payload(&serde_json::json!({
            "content": "Hello",
            "format": "markdown",
            "tiptap_json": { "type": "doc" },
            "block_id": "block-a",
            "position": "after"
        })))
        .expect("McpBlockInsertPayload serializes");
        assert_eq!(insert["content"], "Hello");
        assert_eq!(insert["tiptapJson"]["type"], "doc");
        assert_eq!(insert["blockId"], "block-a");

        let update = serde_json::to_value(mcp_block_update_payload(&serde_json::json!({
            "updates": [{ "blockId": "block-a" }],
            "attributes": { "checked": true },
            "block_id": "block-a"
        })))
        .expect("McpBlockUpdatePayload serializes");
        assert!(update["edits"].is_array());
        assert_eq!(update["attrs"]["checked"], true);
        assert_eq!(update["blockId"], "block-a");

        let edit = serde_json::to_value(mcp_block_edit_text_payload(&serde_json::json!({
            "block_id": "block-a",
            "ops": [{ "type": "insert" }]
        })))
        .expect("McpBlockEditTextPayload serializes");
        assert_eq!(edit["blockId"], "block-a");
        assert!(edit["operations"].is_array());

        let delete = serde_json::to_value(mcp_block_delete_payload(&serde_json::json!({
            "block_ids": ["a", "b"]
        })))
        .expect("McpBlockDeletePayload serializes");
        assert_eq!(delete["blockIds"][0], "a");
    }

    /// Chronicle Part 3: the staleness guard and its explicit bypass ride the
    /// block payloads under their canonical camelCase names.
    #[test]
    fn block_payloads_carry_expected_revision_and_force() {
        let edit = serde_json::to_value(mcp_block_edit_text_payload(&serde_json::json!({
            "block_id": "block-a",
            "ops": [{ "type": "insert" }],
            "expected_revision": 4,
        })))
        .expect("serializes");
        assert_eq!(edit["expectedRevision"], 4);
        assert!(edit.get("force").is_none());

        let forced = serde_json::to_value(mcp_block_delete_payload(&serde_json::json!({
            "block_ids": ["a"],
            "expectedRevision": null,
            "force": true,
        })))
        .expect("serializes");
        assert!(forced.get("expectedRevision").is_none(), "null collapses to no guard");
        assert_eq!(forced["force"], true);

        let plain = serde_json::to_value(mcp_block_insert_payload(&serde_json::json!({
            "content": "x"
        })))
        .expect("serializes");
        assert!(plain.get("expectedRevision").is_none());
        assert!(plain.get("force").is_none());
    }
}
