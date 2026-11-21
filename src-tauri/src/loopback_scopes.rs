pub(crate) use crate::loopback_scope_catalog::LoopbackScopeDetail;
pub(crate) use crate::loopback_token_grants::MCP_TOKEN_SCOPE_MODE;
use serde::Deserialize;
use std::sync::OnceLock;

pub(crate) const STATIC_SCOPE_SELECTION_MODE: &str = "all";
const LOCAL_LOOPBACK_SURFACE_JSON: &str = include_str!("../../parity/local-loopback-surface.json");

static LOCAL_MCP_TOOL_SCOPE_ENTRIES: OnceLock<Vec<LocalMcpToolScopeEntry>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalLoopbackSurface {
    mcp_tools: Vec<LocalMcpToolScopeEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalMcpToolScopeEntry {
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    required_scope_mode: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CrdtOperationScopeRule {
    pub(crate) kind: &'static str,
    pub(crate) scopes: &'static [&'static str],
}

const CRDT_OPERATION_SCOPE_RULES: &[CrdtOperationScopeRule] = &[
    crdt_rule(
        "crdt.flush",
        &["documents.write.crdt", "workspace.write.crdt"],
    ),
    crdt_rule("workspace.createDocument", &["workspace.write.crdt"]),
    crdt_rule("workspace.updateDocument", &["workspace.write.crdt"]),
    crdt_rule("workspace.createFolder", &["workspace.write.crdt"]),
    crdt_rule("workspace.updateFolder", &["workspace.write.crdt"]),
    crdt_rule("workspace.moveFolder", &["workspace.write.crdt"]),
    crdt_rule("workspace.moveDocuments", &["workspace.write.crdt"]),
    crdt_rule(
        "workspace.moveDocumentsAcrossGraphs",
        &[
            "documents.read",
            "documents.write.crdt",
            "documents.delete.crdt",
            "workspace.write.crdt",
            "workspace.delete.crdt",
            "wires.delete",
            "salience.write",
        ],
    ),
    crdt_rule(
        "workspace.deleteDocument",
        &["documents.delete.crdt", "workspace.delete.crdt"],
    ),
    crdt_rule("workspace.deleteFolder", &["workspace.delete.crdt"]),
    crdt_rule(
        "workspace.putArtifact",
        &["workspace.write.crdt", "artifacts.write"],
    ),
    crdt_rule(
        "workspace.deleteArtifact",
        &["workspace.delete.crdt", "artifacts.delete"],
    ),
    crdt_rule("workspace.createWire", &["wires.write"]),
    crdt_rule("workspace.refreshWire", &["wires.write"]),
    crdt_rule("workspace.deleteWire", &["wires.delete"]),
    crdt_rule("document.write", &["documents.write.crdt"]),
    crdt_rule("document.editComment", &["documents.write.crdt"]),
    crdt_rule("document.liveProjection", &["documents.write.crdt"]),
    crdt_rule("block.insert", &["documents.write.crdt"]),
    crdt_rule("block.update", &["documents.write.crdt"]),
    crdt_rule("block.editText", &["documents.write.crdt"]),
    crdt_rule("block.delete", &["documents.delete.crdt"]),
    crdt_rule(
        "document.batchPrepare",
        &["documents.write.crdt", "workspace.write.crdt"],
    ),
    crdt_rule(
        "document.batchRegister",
        &["documents.write.crdt", "workspace.write.crdt"],
    ),
    crdt_rule(
        "document.ingestMarkdownOriginal",
        &[
            "artifacts.ingest",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ),
    crdt_rule(
        "document.uploadIngest",
        &[
            "artifacts.ingest",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ),
    crdt_rule("graph.importArchive", &["graphs.import"]),
    crdt_rule("import.vault", &["graphs.import"]),
    crdt_rule(
        "import.webClip",
        &[
            "imports.web.write",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ),
];

const fn crdt_rule(kind: &'static str, scopes: &'static [&'static str]) -> CrdtOperationScopeRule {
    CrdtOperationScopeRule { kind, scopes }
}

pub(crate) fn crdt_operation_scope_rules() -> &'static [CrdtOperationScopeRule] {
    CRDT_OPERATION_SCOPE_RULES
}

pub(crate) fn crdt_operation_scopes(kind: &str) -> Option<Vec<&'static str>> {
    CRDT_OPERATION_SCOPE_RULES
        .iter()
        .find(|rule| rule.kind == kind)
        .map(|rule| rule.scopes.to_vec())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DeleteScopeRule {
    pub(crate) kind: &'static str,
    pub(crate) scopes: &'static [&'static str],
}

const DELETE_SCOPE_RULES: &[DeleteScopeRule] = &[
    delete_rule(
        "documents",
        &["documents.delete.crdt", "workspace.delete.crdt"],
    ),
    delete_rule("blocks", &["documents.delete.crdt"]),
    delete_rule("folder", &["workspace.delete.crdt"]),
    delete_rule("wires", &["wires.delete"]),
    delete_rule("graph", &["graphs.delete"]),
];

const fn delete_rule(kind: &'static str, scopes: &'static [&'static str]) -> DeleteScopeRule {
    DeleteScopeRule { kind, scopes }
}

pub(crate) fn delete_scope_rules() -> &'static [DeleteScopeRule] {
    DELETE_SCOPE_RULES
}

pub(crate) fn delete_scopes(delete_type: &str) -> Option<Vec<&'static str>> {
    let canonical = match delete_type {
        "document" | "documents" => "documents",
        "block" | "blocks" => "blocks",
        "folder" | "folders" => "folder",
        "wire" | "wires" => "wires",
        "graph" | "graphs" => "graph",
        _ => return None,
    };
    DELETE_SCOPE_RULES
        .iter()
        .find(|rule| rule.kind == canonical)
        .map(|rule| rule.scopes.to_vec())
}

pub(crate) fn mcp_tool_required_scope_mode(name: &str) -> &'static str {
    local_mcp_tool_scope_entry(name)
        .and_then(|entry| entry.required_scope_mode.as_deref())
        .unwrap_or(STATIC_SCOPE_SELECTION_MODE)
}

pub(crate) fn mcp_tool_scopes(name: &str) -> Vec<&'static str> {
    local_mcp_tool_scope_entry(name)
        .map(|entry| entry.scopes.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

fn local_mcp_tool_scope_entry(name: &str) -> Option<&'static LocalMcpToolScopeEntry> {
    local_mcp_tool_scope_entries()
        .iter()
        .find(|entry| entry.name == name)
}

fn local_mcp_tool_scope_entries() -> &'static [LocalMcpToolScopeEntry] {
    LOCAL_MCP_TOOL_SCOPE_ENTRIES
        .get_or_init(|| {
            let surface: LocalLoopbackSurface = serde_json::from_str(LOCAL_LOOPBACK_SURFACE_JSON)
                .expect("embedded local loopback surface registry must be valid JSON");
            surface.mcp_tools
        })
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_scope_map_covers_mutating_representatives() {
        assert_eq!(mcp_tool_scopes("list_graphs"), vec!["graphs.read"]);
        assert!(mcp_tool_scopes("write_document").contains(&"documents.write.crdt"));
        assert!(mcp_tool_scopes("delete").contains(&"workspace.delete.crdt"));
        assert!(mcp_tool_scopes("rdf_load").contains(&"rdf.load"));
        assert!(mcp_tool_scopes("crdt_operation").contains(&"graphs.import"));
        assert_eq!(
            mcp_tool_required_scope_mode("crdt_operation"),
            "operation-kind"
        );
        assert_eq!(
            mcp_tool_required_scope_mode("write_document"),
            STATIC_SCOPE_SELECTION_MODE
        );
    }

    #[test]
    fn crdt_operation_scope_map_covers_known_operation_families() {
        assert!(crdt_operation_scope_rules()
            .iter()
            .any(|rule| rule.kind == "document.write"));
        assert_eq!(
            crdt_operation_scopes("document.write"),
            Some(vec!["documents.write.crdt"])
        );
        assert_eq!(
            crdt_operation_scopes("workspace.deleteDocument"),
            Some(vec!["documents.delete.crdt", "workspace.delete.crdt"])
        );
        assert_eq!(
            crdt_operation_scopes("workspace.putArtifact"),
            Some(vec!["workspace.write.crdt", "artifacts.write"])
        );
        assert_eq!(
            crdt_operation_scopes("import.webClip"),
            Some(vec![
                "imports.web.write",
                "documents.write.crdt",
                "workspace.write.crdt"
            ])
        );
        assert_eq!(crdt_operation_scopes("unknown.operation"), None);
    }

    #[test]
    fn crdt_operation_tool_scope_union_matches_operation_rules() {
        let mut union = Vec::new();
        for rule in crdt_operation_scope_rules() {
            for scope in rule.scopes {
                if !union.contains(scope) {
                    union.push(*scope);
                }
            }
        }
        assert_eq!(mcp_tool_scopes("crdt_operation"), union);
    }

    #[test]
    fn delete_scopes_are_selected_from_target_type() {
        assert_eq!(delete_scopes("block"), Some(vec!["documents.delete.crdt"]));
        assert_eq!(
            delete_scopes("documents"),
            Some(vec!["documents.delete.crdt", "workspace.delete.crdt"])
        );
        assert_eq!(
            delete_scopes("folders"),
            Some(vec!["workspace.delete.crdt"])
        );
        assert_eq!(delete_scopes("wire"), Some(vec!["wires.delete"]));
        assert_eq!(delete_scopes("graph"), Some(vec!["graphs.delete"]));
        assert_eq!(delete_scopes("unknown"), None);
    }
}
