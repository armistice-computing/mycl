//! Cloud-1 document authorities become native shared Geist, never agent-attributed.
//! Source Y.Docs are retained independently of the mutable native projections.
use crate::{
    crdt_engine::{import_archive_ops::ParsedGraphArchive, projection::ydoc_to_tiptap_json},
    geist_memory_store::LocalMemoryStore,
    geist_song_store::LocalSongStore,
    salience_value_config::{default_value_config, LocalValueConfig},
    storage::{create_dir_all, write_json},
};
use base64::Engine;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};
use yrs::updates::decoder::Decode;
use yrs::{Doc, Transact, Update};

static GEIST_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(crate) fn mutation_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    GEIST_MUTATION_LOCK
        .lock()
        .map_err(|_| "Geist mutation lock poisoned".into())
}

/// Imported graph documents are evidence, not disposable native projections.
pub(crate) fn projection_document_id(graph_dir: &Path, id: &str) -> Result<String, String> {
    let path = graph_dir.join("memory/legacy-geist-source.json");
    if path.is_file() {
        let source: Value = crate::storage::read_json(&path).map_err(|e| e.to_string())?;
        if source["documentsBase64"].get(id).is_some() {
            return Ok(format!("{id}-native"));
        }
    }
    Ok(id.into())
}

pub(crate) struct LegacyGeistPlan {
    pub(crate) song: Option<LocalSongStore>,
    pub(crate) memory: Option<LocalMemoryStore>,
    pub(crate) config: Option<LocalValueConfig>,
    source: Value,
}

pub(crate) fn plan(
    archive: &ParsedGraphArchive,
    graph_id: &str,
) -> Result<LegacyGeistPlan, String> {
    let mut documents = BTreeMap::new();
    let mut source = BTreeMap::new();
    for (id, bytes) in &archive.documents {
        if !id.starts_with("geist-") {
            continue;
        }
        let doc = Doc::new();
        let update = Update::decode_v1(bytes).map_err(|e| format!("Geist {id}: {e}"))?;
        doc.transact_mut()
            .apply_update(update)
            .map_err(|e| format!("Geist {id}: {e}"))?;
        documents.insert(id.clone(), ydoc_to_tiptap_json(&doc));
        source.insert(
            id.clone(),
            base64::engine::general_purpose::STANDARD.encode(bytes),
        );
    }
    // Native exports contain both authority files and their document projections.
    // Do not reinterpret the projections as a second legacy authority.
    let native = |path: &str| archive.originals.iter().any(|(name, _)| name == path);
    let song = if native("narrative/song.json") {
        None
    } else {
        crate::legacy_geist_song::parse_song(&documents, graph_id)?
    };
    let memory = if native("memory/memory-queue.json") {
        None
    } else {
        crate::legacy_geist_memory::parse_memory(graph_id, &documents)?
    };
    let config = if native("values/block-values.json") {
        None
    } else {
        crate::legacy_geist_config::parse_config(&documents)?
    };
    if song.is_some() {
        for id in ["geist-song-native", "geist-past-songs-native"] {
            if documents.contains_key(id) {
                return Err(format!(
                    "native Song projection destination already occupied in source: {id}"
                ));
            }
        }
    }
    Ok(LegacyGeistPlan {
        song,
        memory,
        config,
        source: json!({
            "schemaVersion": 1, "sourceGraphId": archive.manifest.source_graph_id,
            "sourceUserId": archive.manifest.source_user_id,
            "documentsBase64": source,
        }),
    })
}

impl LegacyGeistPlan {
    /// Rebuild derived state only after imported RDF has finished loading.
    pub(crate) fn reconcile(&self, graph_dir: &Path, graph_id: &str) -> Result<(), String> {
        let _value_guard = crate::salience_value_store::value_mutation_guard()?;
        let _guard = mutation_guard()?;
        if self.song.is_some() {
            let store = crate::geist_song_store::read_song_store(graph_dir, graph_id)?;
            crate::geist_song_rdf::reconcile_song_store(graph_dir, &store)?;
        }
        if self.memory.is_some() {
            let store = crate::geist_memory_store::read_memory_store(graph_dir, graph_id)?;
            crate::geist_memory_rdf::materialize_memory_store(graph_dir, &store)?;
        }
        if self.config.is_some() {
            let store = crate::salience_value_store::read_value_store(graph_dir, graph_id)?;
            crate::salience_rdf_materializer::reconcile_value_store(graph_dir, &store)?;
        }
        Ok(())
    }
    pub(crate) fn authority_evidence(&self) -> Result<BTreeMap<String, Value>, String> {
        let mut result = BTreeMap::new();
        if let Some(song) = &self.song {
            result.insert(
                "narrative/song.json".into(),
                serde_json::to_value(song).map_err(|e| e.to_string())?,
            );
        }
        if let Some(memory) = &self.memory {
            result.insert(
                "memory/memory-queue.json".into(),
                serde_json::to_value(memory).map_err(|e| e.to_string())?,
            );
        }
        if let Some(config) = &self.config {
            result.insert(
                "values/block-values.json#config".into(),
                serde_json::to_value(config).map_err(|e| e.to_string())?,
            );
        }
        if !result.is_empty() {
            result.insert(
                "memory/legacy-geist-source.json".into(),
                self.source.clone(),
            );
        }
        Ok(result)
    }
    pub(crate) fn summary(&self) -> Value {
        json!({"song":self.song.is_some(),"memory":self.memory.is_some(),"config":self.config.is_some()})
    }

    /// Caller holds the value mutation guard; only a new/partially imported graph is valid.
    /// Validate all existing files before any writes; exact retry is permitted, drift is not.
    pub(crate) fn install(&self, graph_dir: &Path, graph_id: &str) -> Result<(), String> {
        let _guard = mutation_guard()?;
        if self.song.is_none() && self.memory.is_none() && self.config.is_none() {
            return Ok(());
        }
        let mut writes = BTreeMap::new();
        writes.insert("memory/legacy-geist-source.json", self.source.clone());
        if let Some(song) = &self.song {
            writes.insert(
                "narrative/song.json",
                serde_json::to_value(song).map_err(|e| e.to_string())?,
            );
        }
        if let Some(memory) = &self.memory {
            writes.insert(
                "memory/memory-queue.json",
                serde_json::to_value(memory).map_err(|e| e.to_string())?,
            );
        }
        for (path, expected) in &writes {
            let path = graph_dir.join(path);
            if path.exists() {
                let actual: Value = crate::storage::read_json(&path).map_err(|e| e.to_string())?;
                if &actual != expected {
                    return Err(format!(
                        "Geist import conflicts with {}; refusing overwrite",
                        path.display()
                    ));
                }
            }
        }
        if let Some(config) = &self.config {
            let path = graph_dir.join("values/block-values.json");
            let mut store: Value = if path.exists() {
                crate::storage::read_json(&path).map_err(|e| e.to_string())?
            } else {
                serde_json::to_value(crate::salience_value_store::read_value_store(
                    graph_dir, graph_id,
                )?)
                .map_err(|e| e.to_string())?
            };
            let incoming = serde_json::to_value(config).map_err(|e| e.to_string())?;
            let default =
                serde_json::to_value(default_value_config()).map_err(|e| e.to_string())?;
            if store["graphId"] != graph_id || store["observer"].as_str().unwrap_or("") != "" {
                return Err("Geist config destination is not this graph's shared store".into());
            }
            if store["config"] != incoming
                && (store["config"] != default
                    || store["configHistory"]
                        .as_array()
                        .is_some_and(|a| !a.is_empty()))
            {
                return Err(
                    "Geist import conflicts with existing scoring config; refusing overwrite"
                        .into(),
                );
            }
            store["config"] = incoming;
            writes.insert("values/block-values.json", store);
        }
        for (relative, value) in writes {
            let path = graph_dir.join(relative);
            create_dir_all(path.parent().ok_or("missing Geist parent")?)?;
            write_json(&path, &value).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
