use crate::loopback_scope_catalog::{
    loopback_scope_details, loopback_scope_keys, LoopbackScopeDetail,
};
use serde::Serialize;
use std::collections::BTreeSet;

/// The owner manifest carries the process's master credential. Connector
/// credentials are separate, persisted records with their own scopes.
pub(crate) const OWNER_TOKEN_SCOPE_MODE: &str = "owner-master";

/// MCP metadata must describe the credential a connector actually presents,
/// not the owner's compatibility credential in `loopback.json`.
pub(crate) const MCP_TOKEN_SCOPE_MODE: &str = "per-token";

/// The curated agent surface for a Garden user's own agents (Eschaton,
/// 2026-10-28): reading and writing documents, the workspace, wires, memory,
/// the Song and Diary, valuing, SPARQL reads, artifacts and the two job
/// reads. Transport tools the organism uses on its own node (rdf_load,
/// rdf_dump, sparql_update, the source mirror, crdt_operation, remember_batch,
/// the emporium faces, time travel) are in no agent profile but `developer`.
///
/// Left out on 2026-11-18 (Eschaton): list_documents (get_workspace lists
/// the same), create_document (write_document takes a folder, and
/// expectedRevision 0 makes it create-only), delete_blocks (delete does it),
/// check_document and the two artifact revision tools (too narrow for a
/// person's own agent). They stay in the catalog for `developer`.
pub(crate) const GARDEN_PROFILE_ID: &str = "garden";
pub(crate) const GARDEN_MCP_TOOLS: &[&str] = &[
    "attune",
    "get_user_location",
    "list_graphs",
    "create_graph",
    "duplicate_graph",
    "set_home_graph",
    "reindex_graph",
    "get_workspace",
    "create_folder",
    "move_folder",
    "move_documents",
    "read_document",
    "read_blocks",
    "read_document_at_snapshot",
    "get_document_history",
    "get_block",
    "query_blocks",
    "search_documents",
    "search_blocks",
    "document_digest",
    "write_document",
    "insert_blocks",
    "update_blocks",
    "edit_block_text",
    "delete",
    "rename",
    "make_document_editable",
    "edit_comment",
    "create_wires",
    "get_wires",
    "traverse_wires",
    "list_wire_predicates",
    "remember",
    "recall",
    "archive_memories",
    "music",
    "sing",
    "diary",
    "read_diary",
    "value",
    "get_block_values",
    "get_important_blocks",
    "sparql_query",
    "get_job_status",
    "get_job_result",
    "upload_artifact",
    "reference_artifact",
    "read_artifact",
    "ingest_artifact",
    "list_artifact_kinds",
];

/// The developer profile carries every connector-issuable scope and no tool
/// allowlist, so a token minted with it sees the whole registered catalog,
/// transport tools included. It is for the house and for anyone testing a
/// release build; it is never the default.
pub(crate) const DEVELOPER_PROFILE_ID: &str = "developer";

pub(crate) const LITE_MCP_TOOLS: &[&str] = &[
    "check_document",
    "document_digest",
    "get_block",
    "get_document_history",
    "get_important_blocks",
    "get_wires",
    "get_workspace",
    "music",
    "query_blocks",
    "read_blocks",
    "read_document",
    "read_document_at_snapshot",
    "recall",
    "search_blocks",
    "search_documents",
    "traverse_wires",
];

pub(crate) const ANGEL_MCP_TOOLS: &[&str] = &[
    "check_document",
    "create_wires",
    "document_digest",
    "edit_comment",
    "get_block",
    "get_block_values",
    "get_document_history",
    "get_important_blocks",
    "get_wires",
    "get_workspace",
    "insert_blocks",
    "list_wire_predicates",
    "music",
    "query_blocks",
    "read_blocks",
    "read_document",
    "read_document_at_snapshot",
    "recall",
    "search_blocks",
    "search_documents",
    "traverse_wires",
    "value",
    "write_document",
];

/// Sophia Mu (spec "Sophia Mu — spec (2026-09-30)"): reads everything an L1
/// reads; writes her Diary, comments and wires, and nothing else. No Song, no
/// shared memory queue, no valuations, no document text, no deletes.
///
/// The tool list is the grant at the MCP layer (enforced at tools/list and at
/// call). The scopes underneath are coarser (diary rides memory.write,
/// edit_comment rides documents.write.crdt), so Mu credentials are also
/// refused every non-read REST scope: see `loopback_http::profile_rest_scope_refused`.
pub(crate) const MU_PROFILE_ID: &str = "mu";

pub(crate) const MU_MCP_TOOLS: &[&str] = &[
    // Read: the lite surface plus what else an L1 reads.
    "attune",
    "check_document",
    "document_digest",
    "get_block",
    "get_block_values",
    "get_document_history",
    "get_important_blocks",
    "get_user_location",
    "get_wires",
    "get_workspace",
    "list_graphs",
    "list_wire_predicates",
    "music",
    "query_blocks",
    "read_blocks",
    "read_diary",
    "read_document",
    "read_document_at_snapshot",
    "recall",
    "search_blocks",
    "search_documents",
    "sparql_query",
    "traverse_wires",
    // Write: her Diary, comments, wires.
    "create_wires",
    "diary",
    "edit_comment",
];

pub(crate) const HIVEMIND_MCP_TOOLS: &[&str] = &[
    "attune",
    "check_document",
    "create_folder",
    "create_graph",
    "create_wires",
    "delete",
    "diary",
    "document_digest",
    "duplicate_graph",
    "edit_block_text",
    "edit_comment",
    "get_block",
    "get_block_values",
    "get_document_history",
    "get_important_blocks",
    "get_user_location",
    "get_wires",
    "get_workspace",
    "insert_blocks",
    "list_graphs",
    "make_document_editable",
    "move_documents",
    "move_folder",
    "music",
    "query_blocks",
    "read_blocks",
    "read_diary",
    "read_document",
    "read_document_at_snapshot",
    "recall",
    "remember",
    "rename",
    "search_blocks",
    "search_documents",
    "set_home_graph",
    "sing",
    "sparql_query",
    "sparql_update",
    "traverse_wires",
    "update_blocks",
    "value",
    "write_document",
];

/// Named Sophia grants follow the public profiles; Hivemind omits `care`
/// (and `care` itself was cut on 2026-10-28)
/// under the September 10 lightweight-memory decision.
/// Last audited against armistice-computing/mnemosyne-mcp@b1616f9,
/// `src/neem/mcp/server/standalone_server.py`.
/// Generic custom-scope and owner credentials remain scope-driven.
pub(crate) fn mcp_tool_names_for_grant_profile(
    profile_id: Option<&str>,
) -> Option<&'static [&'static str]> {
    match profile_id {
        Some("read-only" | "angel-code") => Some(LITE_MCP_TOOLS),
        Some("angel-graph") => Some(ANGEL_MCP_TOOLS),
        Some("hivemind") => Some(HIVEMIND_MCP_TOOLS),
        Some(MU_PROFILE_ID) => Some(MU_MCP_TOOLS),
        Some(GARDEN_PROFILE_ID) => Some(GARDEN_MCP_TOOLS),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LoopbackTokenGrant {
    pub(crate) scope_mode: &'static str,
    pub(crate) scopes: Vec<&'static str>,
    pub(crate) scope_details: Vec<LoopbackScopeDetail>,
    pub(crate) grant_profiles: Vec<LoopbackGrantProfile>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackGrantProfile {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) scopes: Vec<&'static str>,
    pub(crate) default_grant: bool,
    pub(crate) mutating: bool,
    /// Compatibility/master grants must never be minted as distributable
    /// connector credentials.
    pub(crate) client_issuable: bool,
}

pub(crate) fn session_all_token_grant() -> LoopbackTokenGrant {
    let scopes = loopback_scope_keys();
    let scope_details = loopback_scope_details();
    LoopbackTokenGrant {
        scope_mode: OWNER_TOKEN_SCOPE_MODE,
        grant_profiles: loopback_grant_profiles(&scopes),
        scopes,
        scope_details,
    }
}

fn loopback_grant_profiles(all_scopes: &[&'static str]) -> Vec<LoopbackGrantProfile> {
    let connector_read = connector_read_scopes(all_scopes);
    let authoring = extend_scopes(
        &connector_read,
        all_scopes,
        &[
            "documents.write.crdt",
            "documents.snapshots.write",
            "workspace.write.crdt",
            "artifacts.write",
            "artifacts.ingest",
            "images.write",
            "wires.write",
            "entities.write",
            "imports.web.write",
        ],
    );
    let hivemind = extend_scopes(
        &authoring,
        all_scopes,
        &[
            "graphs.write",
            "graphs.delete",
            "documents.delete.crdt",
            "workspace.delete.crdt",
            "wires.delete",
            "orientation.write",
            "rdf.update",
            "memory.write",
            "salience.write",
            "semantic.index.write",
            "time-travel.write",
        ],
    );
    let angel_graph = extend_scopes(
        &connector_read,
        all_scopes,
        &[
            "documents.write.crdt",
            "workspace.write.crdt",
            "wires.write",
            "memory.write",
            "salience.write",
        ],
    );
    let mu = extend_scopes(
        &connector_read,
        all_scopes,
        &["memory.write", "documents.write.crdt", "wires.write"],
    );
    let garden = garden_scopes(all_scopes);
    let developer = all_scopes
        .iter()
        .copied()
        .filter(|scope| !scope.starts_with("loopback."))
        .collect::<Vec<_>>();
    vec![
        LoopbackGrantProfile {
            id: GARDEN_PROFILE_ID,
            label: "Mycl agent",
            description: "The recommended profile for your own agents: read and write documents, the workspace, wires, memory, the Song and Diary, values, artifacts, and SPARQL reads. No transport or administration tools.",
            scopes: garden,
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "read-only",
            label: "Read-only",
            description: "Read graph content, history, search, wires, values, Geist, and semantic projections through MCP.",
            scopes: connector_read.clone(),
            default_grant: true,
            mutating: false,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "angel-code",
            label: "Angel code",
            description: "Read-only MCP access for code and verification agents.",
            scopes: connector_read.clone(),
            default_grant: false,
            mutating: false,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "angel-graph",
            label: "Angel graph",
            description: "Match the legacy Angel MCP surface: graph reads plus document, wire, comment, and valuation writes, without delete or administration tools.",
            scopes: angel_graph,
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: MU_PROFILE_ID,
            label: "Sophia Mu",
            description: "The headless explore agent: reads everything an L1 reads; writes only her Diary, comments and wires, through MCP. Non-read REST scopes are refused.",
            scopes: mu,
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "authoring",
            label: "Authoring",
            description: "Read and write documents, workspace structure, artifacts, inline images, wires, and imports without delete or administration scopes.",
            scopes: authoring,
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "hivemind",
            label: "Hivemind",
            description: "Match the legacy Hivemind MCP surface, including its unified delete tool, without owner, service, or Garden-extension tools.",
            scopes: hivemind,
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "rdf-admin",
            label: "RDF admin",
            description: "Read, load, update, and dump local RDF graphs.",
            scopes: extend_scopes(
                &connector_read,
                all_scopes,
                &["rdf.query", "rdf.dump", "rdf.load", "rdf.update"],
            ),
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: DEVELOPER_PROFILE_ID,
            label: "Developer",
            description: "Every connector scope and every registered tool, transport tools included. For the house and for testing a release build; never the default.",
            scopes: developer,
            default_grant: false,
            mutating: true,
            client_issuable: true,
        },
        LoopbackGrantProfile {
            id: "session-all",
            label: "Session all",
            description: "Owner compatibility grant containing every known local loopback and MCP scope. It cannot be issued to a connector.",
            scopes: all_scopes.to_vec(),
            default_grant: false,
            mutating: true,
            client_issuable: false,
        },
    ]
}

/// The garden profile's scopes are DERIVED from its tool list: the MCP entry
/// scopes plus the union of what each allowlisted tool declares, nothing
/// else. So the scope list can never reach past the tools (Eschaton,
/// 2026-10-30: the scopes opened REST routes no garden tool needed, among
/// them raw CRDT ingress and graph export). A garden token also writes only
/// through its tools over HTTP; see `loopback_http::profile_rest_scope_refused`.
/// The garden profile's scope set, computed once: the ceiling for a
/// scope-only connector key (see `effective_grant_profile_id`).
pub(crate) fn garden_profile_scopes() -> &'static [&'static str] {
    static SCOPES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    SCOPES.get_or_init(|| garden_scopes(&loopback_scope_keys()))
}

/// A connector key with no grant profile whose scopes all sit inside the
/// garden profile's set is a garden key in everything but name: it gets
/// garden's ceiling (tools = GARDEN_MCP_TOOLS ∩ scope-allowed, REST writes
/// refused). Without this a "read and write" scope key reached
/// crdt_operation, flush_crdt, remember_batch and revaluate through coarse
/// scopes, and wrote over REST, which no garden-profile key may (Delta,
/// 2026-10-30, on a real cell). A key asking for any scope outside garden's
/// set (developer-grade) keeps its own shape; so does any profiled key.
pub(crate) fn effective_grant_profile_id(
    stored_profile_id: Option<&str>,
    scopes: &[String],
) -> Option<String> {
    let stored = stored_profile_id.map(str::trim).filter(|id| !id.is_empty());
    if stored.is_some() {
        return stored.map(str::to_string);
    }
    if scopes.is_empty() {
        return None;
    }
    let garden = garden_profile_scopes();
    scopes
        .iter()
        .all(|scope| garden.contains(&scope.as_str()))
        .then(|| GARDEN_PROFILE_ID.to_string())
}

pub(crate) fn garden_scopes(all_scopes: &[&'static str]) -> Vec<&'static str> {
    let available = all_scopes.iter().copied().collect::<BTreeSet<_>>();
    let mut wanted = BTreeSet::from(["mcp.tools.read", "mcp.tools.call"]);
    for tool in GARDEN_MCP_TOOLS {
        wanted.extend(crate::loopback_scopes::mcp_tool_scopes(tool));
    }
    all_scopes
        .iter()
        .copied()
        .filter(|scope| wanted.contains(scope) && available.contains(scope))
        .collect()
}

fn connector_read_scopes(all_scopes: &[&'static str]) -> Vec<&'static str> {
    selected_scopes(
        all_scopes,
        &[
            "mcp.tools.read",
            "mcp.tools.call",
            "graphs.read",
            "documents.read",
            "documents.history.read",
            "workspace.read",
            "artifacts.read",
            "images.read",
            "rdf.query",
            "rdf.dump",
            "search.lexical.read",
            "search.semantic.read",
            "wires.read",
            "entities.read",
            "time-travel.read",
            "semantic.models.read",
            "semantic.index.read",
            "orientation.read",
            "memory.read",
            "salience.read",
            "jobs.read",
        ],
    )
}

fn extend_scopes(
    base: &[&'static str],
    all_scopes: &[&'static str],
    additions: &[&'static str],
) -> Vec<&'static str> {
    let available = all_scopes.iter().copied().collect::<BTreeSet<_>>();
    let mut scopes = base.to_vec();
    for scope in additions {
        if available.contains(scope) && !scopes.contains(scope) {
            scopes.push(scope);
        }
    }
    scopes
}

fn selected_scopes(all_scopes: &[&'static str], requested: &[&'static str]) -> Vec<&'static str> {
    let available = all_scopes.iter().copied().collect::<BTreeSet<_>>();
    requested
        .iter()
        .copied()
        .filter(|scope| available.contains(scope))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn session_all_grant_includes_public_contract_surfaces_once() {
        let grant = session_all_token_grant();
        let unique = grant.scopes.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(grant.scope_mode, OWNER_TOKEN_SCOPE_MODE);
        assert_eq!(unique.len(), grant.scopes.len());
        assert!(unique.contains("loopback.manifest.read"));
        assert!(unique.contains("documents.write.crdt"));
        assert!(unique.contains("documents.delete.crdt"));
        assert!(unique.contains("workspace.write.crdt"));
        assert!(unique.contains("workspace.delete.crdt"));
        assert!(unique.contains("rdf.query"));
        assert!(unique.contains("semantic.index.write"));
        assert!(unique.contains("mcp.tools.call"));
    }

    #[test]
    fn session_all_grant_scope_details_cover_granted_scopes() {
        let grant = session_all_token_grant();
        let detail_scopes = grant
            .scope_details
            .iter()
            .map(LoopbackScopeDetail::key)
            .collect::<BTreeSet<_>>();

        assert_eq!(
            detail_scopes,
            grant.scopes.iter().copied().collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn grant_profiles_are_unique_and_include_compatibility_profile() {
        let grant = session_all_token_grant();
        let ids = grant
            .grant_profiles
            .iter()
            .map(|profile| profile.id)
            .collect::<BTreeSet<_>>();

        assert_eq!(ids.len(), grant.grant_profiles.len());
        assert!(ids.contains("read-only"));
        assert!(ids.contains("angel-code"));
        assert!(ids.contains("angel-graph"));
        assert!(ids.contains("authoring"));
        assert!(ids.contains("hivemind"));
        assert!(ids.contains("rdf-admin"));
        assert!(ids.contains("session-all"));
        assert!(ids.contains(MU_PROFILE_ID));
    }

    #[test]
    fn mu_writes_only_diary_comments_and_wires() {
        let tools = mcp_tool_names_for_grant_profile(Some(MU_PROFILE_ID)).expect("mu tool list");
        for allowed in ["diary", "edit_comment", "create_wires", "read_diary", "read_document", "recall", "music"] {
            assert!(tools.contains(&allowed), "{allowed}");
        }
        for denied in [
            "remember", "store_memory", "sing", "value", "revaluate", "write_document",
            "insert_blocks", "update_blocks", "edit_block_text", "delete", "sparql_update",
            "create_folder", "move_documents", "rename", "set_home_graph", "make_document_editable",
        ] {
            assert!(!tools.contains(&denied), "{denied}");
        }
        // Every Mu tool is one the hivemind (L1) surface also has.
        for tool in tools {
            assert!(HIVEMIND_MCP_TOOLS.contains(tool) || *tool == "list_wire_predicates", "{tool}");
        }
    }

    #[test]
    fn mu_profile_scopes_cover_its_tools_and_nothing_administrative() {
        let grant = session_all_token_grant();
        let mu = grant.grant_profiles.iter().find(|p| p.id == MU_PROFILE_ID).expect("mu profile");
        assert!(mu.client_issuable && mu.mutating && !mu.default_grant);
        for scope in ["memory.read", "memory.write", "documents.write.crdt", "wires.write", "mcp.tools.call"] {
            assert!(mu.scopes.contains(&scope), "{scope}");
        }
        for scope in ["salience.write", "graphs.write", "documents.delete.crdt", "wires.delete", "rdf.update", "workspace.write.crdt"] {
            assert!(!mu.scopes.contains(&scope), "{scope}");
        }
        for tool in mcp_tool_names_for_grant_profile(Some(MU_PROFILE_ID)).unwrap() {
            for scope in crate::loopback_scopes::mcp_tool_scopes(tool) {
                assert!(mu.scopes.contains(&scope), "{tool} needs {scope}");
            }
        }
    }

    #[test]
    fn read_only_profile_is_mcp_usable_without_owner_scopes() {
        let grant = session_all_token_grant();
        let read_only_profile = grant
            .grant_profiles
            .iter()
            .find(|profile| profile.id == "read-only")
            .expect("read-only profile");

        assert!(read_only_profile.default_grant);
        assert!(!read_only_profile.mutating);
        assert!(read_only_profile.scopes.contains(&"mcp.tools.read"));
        assert!(read_only_profile.scopes.contains(&"mcp.tools.call"));
        assert!(read_only_profile.scopes.contains(&"documents.read"));
        assert!(!read_only_profile.scopes.contains(&"loopback.manifest.read"));
        assert!(!read_only_profile.scopes.contains(&"loopback.tokens.write"));
    }

    #[test]
    fn mutating_grant_profiles_are_not_default_granted() {
        let grant = session_all_token_grant();
        let known_scopes = grant.scopes.iter().copied().collect::<BTreeSet<_>>();

        for profile in grant
            .grant_profiles
            .iter()
            .filter(|profile| profile.mutating)
        {
            assert!(!profile.default_grant, "{} must need consent", profile.id);
            assert!(
                profile
                    .scopes
                    .iter()
                    .all(|scope| known_scopes.contains(scope)),
                "{} must reference only known scopes",
                profile.id
            );
        }
    }

    #[test]
    fn garden_profile_names_only_registered_tools_and_no_transport() {
        for tool in GARDEN_MCP_TOOLS {
            assert!(
                crate::mcp_tool_registry::is_known_mcp_tool_name(tool),
                "{tool} is in the garden profile but not the catalog"
            );
        }
        for transport in [
            "rdf_load", "rdf_dump", "sparql_update", "sparql_query_named", "manage_graph",
            "source_pull", "source_push", "source_rebuild", "crdt_operation", "remember_batch",
            "emporium_read", "emporium_write", "emporium_retract", "restore_to_restore_point",
            "get_values", "revaluate", "flush_crdt",
        ] {
            assert!(!GARDEN_MCP_TOOLS.contains(&transport), "{transport} must not reach a garden agent");
        }
        for redundant in [
            "list_documents", "check_document", "create_document", "delete_blocks",
            "list_artifact_revisions", "restore_artifact_revision",
        ] {
            assert!(!GARDEN_MCP_TOOLS.contains(&redundant), "{redundant} was left out of the garden profile");
        }
        assert_eq!(GARDEN_MCP_TOOLS.len(), 50);
        let unique = GARDEN_MCP_TOOLS.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), GARDEN_MCP_TOOLS.len());
        assert!(mcp_tool_names_for_grant_profile(Some(DEVELOPER_PROFILE_ID)).is_none());
        let grant = session_all_token_grant();
        let developer = grant.grant_profiles.iter().find(|p| p.id == DEVELOPER_PROFILE_ID).expect("developer profile");
        assert!(developer.client_issuable);
        assert!(developer.mutating);
        assert!(!developer.default_grant);
        assert!(developer.scopes.iter().all(|scope| !scope.starts_with("loopback.")));
        let garden = grant.grant_profiles.iter().find(|p| p.id == GARDEN_PROFILE_ID).expect("garden profile");
        assert!(!garden.scopes.contains(&"rdf.dump"), "a garden agent must not dump graphs over HTTP");
    }

    /// A scope-only key inside garden's scopes is capped like garden; a key
    /// with one scope outside, or with a profile of its own, is not touched.
    #[test]
    fn scope_only_keys_inside_garden_get_gardens_ceiling() {
        let inside: Vec<String> = ["mcp.tools.read", "mcp.tools.call", "documents.read", "documents.write.crdt", "memory.write"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(effective_grant_profile_id(None, &inside).as_deref(), Some(GARDEN_PROFILE_ID));
        assert_eq!(effective_grant_profile_id(Some(""), &inside).as_deref(), Some(GARDEN_PROFILE_ID));
        let mut outside = inside.clone();
        outside.push("rdf.update".to_string());
        assert_eq!(effective_grant_profile_id(None, &outside), None);
        assert_eq!(effective_grant_profile_id(Some("hivemind"), &inside).as_deref(), Some("hivemind"));
        assert_eq!(effective_grant_profile_id(None, &[]), None);
        // The ceiling hides the transport tools a coarse scope would reach.
        let tools = mcp_tool_names_for_grant_profile(Some(GARDEN_PROFILE_ID)).expect("garden tools");
        for hidden in ["crdt_operation", "flush_crdt", "remember_batch", "revaluate", "emporium_read", "get_values", "sparql_query_named"] {
            assert!(!tools.contains(&hidden), "{hidden} must be hidden under the garden ceiling");
        }
    }

    #[test]
    fn garden_scopes_are_exactly_what_its_tools_need() {
        let grant = session_all_token_grant();
        let garden = grant.grant_profiles.iter().find(|p| p.id == GARDEN_PROFILE_ID).expect("garden profile");
        let scopes = garden.scopes.iter().copied().collect::<BTreeSet<_>>();
        // Every tool is covered.
        for tool in GARDEN_MCP_TOOLS {
            for scope in crate::loopback_scopes::mcp_tool_scopes(tool) {
                assert!(scopes.contains(scope), "{tool} needs {scope}");
            }
        }
        // No scope without a tool behind it (the MCP entry scopes aside).
        let mut needed = BTreeSet::from(["mcp.tools.read", "mcp.tools.call"]);
        for tool in GARDEN_MCP_TOOLS {
            needed.extend(crate::loopback_scopes::mcp_tool_scopes(tool));
        }
        assert_eq!(scopes, needed);
        // The routes this closes: raw CRDT ingress and web imports, entity and
        // snapshot writes, image upload, graph dump and export.
        for closed in ["imports.web.write", "entities.write", "documents.snapshots.write", "images.write", "rdf.dump", "rdf.update", "rdf.load", "time-travel.restore", "sources.rebuild"] {
            assert!(!scopes.contains(closed), "{closed} must not reach a garden token");
        }
    }

    #[test]
    fn session_all_is_not_issuable_to_connectors() {
        let grant = session_all_token_grant();
        let session_all = grant
            .grant_profiles
            .iter()
            .find(|profile| profile.id == "session-all")
            .expect("session-all profile");

        assert!(!session_all.client_issuable);
    }

    #[test]
    fn client_profiles_are_mcp_usable_and_exclude_owner_scopes() {
        let grant = session_all_token_grant();
        for profile in grant
            .grant_profiles
            .iter()
            .filter(|profile| profile.client_issuable)
        {
            assert!(profile.scopes.contains(&"mcp.tools.read"), "{}", profile.id);
            assert!(profile.scopes.contains(&"mcp.tools.call"), "{}", profile.id);
            for owner_scope in [
                "loopback.manifest.read",
                "loopback.tokens.read",
                "loopback.tokens.write",
                "loopback.tokens.delete",
            ] {
                assert!(!profile.scopes.contains(&owner_scope), "{}", profile.id);
            }
        }
    }

    #[test]
    fn angel_graph_has_legacy_write_scopes_without_delete_authority() {
        let grant = session_all_token_grant();
        let angel = grant
            .grant_profiles
            .iter()
            .find(|profile| profile.id == "angel-graph")
            .expect("angel-graph profile");

        assert!(angel.scopes.contains(&"wires.write"));
        assert!(angel.scopes.contains(&"memory.write"));
        assert!(angel.scopes.contains(&"salience.write"));
        assert!(angel.scopes.contains(&"documents.write.crdt"));
        assert!(!angel.scopes.contains(&"documents.delete.crdt"));
        assert!(!angel.scopes.contains(&"wires.delete"));
    }

    #[test]
    fn named_sophia_profiles_have_exact_unique_tool_allowlists() {
        assert!(!HIVEMIND_MCP_TOOLS.contains(&"care"));
        assert!(!HIVEMIND_MCP_TOOLS.contains(&"archive_memories"));
        for (profile_id, tools) in [
            ("read-only", LITE_MCP_TOOLS),
            ("angel-code", LITE_MCP_TOOLS),
            ("angel-graph", ANGEL_MCP_TOOLS),
            ("hivemind", HIVEMIND_MCP_TOOLS),
        ] {
            let unique = tools.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(unique.len(), tools.len(), "duplicate tool in {profile_id}");
            assert!(!unique.contains("crdt_operation"), "{profile_id}");
            assert!(!unique.contains("rdf_load"), "{profile_id}");
            assert!(!unique.contains("delete_blocks"), "{profile_id}");
        }
    }
}
