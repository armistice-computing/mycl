use std::sync::OnceLock;

pub(crate) const PROFILE_ID: &str = "default";
pub(crate) const RUNTIME_PROFILE: &str = "local_only";
pub(crate) const LOCAL_GRAPH_ORIGIN: &str = "local";
pub(crate) const LOCAL_PROVIDER_ID: &str = "local-profile";
pub(crate) const MNEMO_NS: &str = "https://mnemosyne.local/ns#";
pub(crate) const MDOC_NS: &str = "http://mnemosyne.dev/doc#";
pub(crate) const WIRE_NS: &str = "http://mnemosyne.ai/vocab#";
pub(crate) const NFO_NS: &str = "http://www.semanticdesktop.org/ontologies/2007/03/22/nfo#";
pub(crate) const NIE_NS: &str = "http://www.semanticdesktop.org/ontologies/2007/01/19/nie#";
pub(crate) const DCTERMS_NS: &str = "http://purl.org/dc/terms/";
pub(crate) const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub(crate) const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";
pub(crate) const DOCUMENT_SCHEMA_VERSION: u32 = 1;
pub(crate) const YDOC_STATE_FILE: &str = "update-v1.bin";
pub(crate) const WORKSPACE_SNAPSHOT_FILE: &str = "workspace.json";
pub(crate) const SEMANTIC_INDEX_FILE: &str = "blocks.json";
pub(crate) const SEMANTIC_INDEX_REFRESH_JOB_TYPE: &str = "semantic_index_refresh";
pub(crate) const SEMANTIC_MODEL_PREPARE_JOB_TYPE: &str = "semantic_model_prepare";
pub(crate) const SEMANTIC_MODEL_CONFIG_FILE: &str = "selection.json";
pub(crate) const DOCLING_SETUP_FILE: &str = "runtime.json";
pub(crate) const PDF_PIPELINE_CONFIG_FILE: &str = "pipeline.json";
pub(crate) const LOOPBACK_MANIFEST_FILE: &str = "loopback.json";
pub(crate) const LOOPBACK_BIND_HOST: &str = "127.0.0.1";
pub(crate) const LOCAL_JOB_INLINE_RESULT_MAX_BYTES: usize = 256 * 1024;
pub(crate) const LOCAL_UPLOAD_MAX_BYTES: usize = 500 * 1024 * 1024;
pub(crate) const LOCAL_UPLOAD_REQUEST_MAX_BYTES: usize = LOCAL_UPLOAD_MAX_BYTES + 16 * 1024 * 1024;
pub(crate) const LOCAL_IMAGE_UPLOAD_MAX_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const LOCAL_WEB_CLIP_MAX_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const LOCAL_OPENAPI_JSON: &str = include_str!("../../parity/local-openapi.json");
pub(crate) const GRAPH_STATUS_ACTIVE: &str = "active";
pub(crate) const GRAPH_STATUS_DELETED: &str = "deleted";

/// The per-path memory-validation policy (the founder's EA-6 decision). A
/// Greenhouse knob, stored per-graph on [`crate::graph_record_store::GraphRecord`]
/// and (later) mapped to entitlement/tier defaults. Three modes:
///
/// - [`ValidationPolicy::Halt`] — REJECT a malformed write (loud-halt), returning
///   the structured SHACL violations to the caller (the agent) AS FEEDBACK so it
///   can repair and retry. This is the SAFE default: a legacy graph (no field in
///   `graph.json`) defaults here, so a broken write is user-visible, not silently
///   accepted.
/// - [`ValidationPolicy::FlagAndAccept`] — WRITE the memory anyway, but RECORD the
///   violation to the violation ledger (a Meaningful Object) for periodic review.
///   The flexible mode: the write lands, the violation is durable + queryable.
/// - [`ValidationPolicy::Off`] — legacy: NO validation runs (byte-identical to the
///   pre-EA-6 memory path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ValidationPolicy {
    Halt,
    FlagAndAccept,
    Off,
}

impl ValidationPolicy {
    /// The stable string token used when this policy is materialized to RDF
    /// (the Greenhouse-readable face of the knob) and in human logs. Kept as the
    /// policy's canonical token API ahead of the Greenhouse projection that will
    /// surface it; not yet wired to a caller.
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ValidationPolicy::Halt => "halt",
            ValidationPolicy::FlagAndAccept => "flagAndAccept",
            ValidationPolicy::Off => "off",
        }
    }
}

impl Default for ValidationPolicy {
    fn default() -> Self {
        ValidationPolicy::Halt
    }
}

/// The serde default for a missing `validationPolicy` field on a legacy
/// `graph.json` — loud-halt (safe + agent-visible).
pub(crate) fn default_validation_policy() -> ValidationPolicy {
    ValidationPolicy::Halt
}

/// True when a policy equals the default — drives `skip_serializing_if` so a
/// legacy `graph.json` round-trips byte-for-byte (the field is only written when
/// it diverges from the default).
pub(crate) fn is_default_validation_policy(p: &ValidationPolicy) -> bool {
    *p == ValidationPolicy::default()
}

static SELF_HEAL_GRAPHS: OnceLock<bool> = OnceLock::new();

/// Read `GARDEN_SELF_HEAL_GRAPHS` once at startup (see
/// `loopback_server::start_loopback_server`) and cache it for the process's
/// lifetime — the "register once at startup" idiom already used by
/// `cell_durability::set_durable_dirs`. Gates F4c self-heal
/// (`graph_paths::self_heal_missing_graph`); default off. The gateway sets
/// this to `"1"` only in the env block it builds for cells it dynamically
/// spawns behind its own ACL (`cell_env` in `platform-next/gateway/src/cell.rs`)
/// — a bad `graph_id` there materializes a graph rather than 404ing. Bare
/// headless spawns (e.g. `sophia-mcp` local backend mode) never set it and
/// keep today's not-found behavior.
pub(crate) fn init_self_heal_graphs_from_env() {
    let _ = SELF_HEAL_GRAPHS.set(std::env::var("GARDEN_SELF_HEAL_GRAPHS").as_deref() == Ok("1"));
}

pub(crate) fn self_heal_graphs_enabled() -> bool {
    *SELF_HEAL_GRAPHS.get_or_init(|| false)
}
