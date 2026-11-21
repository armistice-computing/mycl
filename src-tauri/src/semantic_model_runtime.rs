use crate::{
    semantic_model_catalog::{SemanticModelBackend, SemanticModelSpec},
    storage::display_path,
};
use std::path::Path;

pub(crate) fn semantic_model_cache_path_hint() -> String {
    if let Ok(hf_home) = std::env::var("HF_HOME") {
        return display_path(Path::new(&hf_home));
    }
    if let Ok(home) = std::env::var("HOME") {
        return display_path(&Path::new(&home).join(".cache/huggingface/hub"));
    }
    "Hugging Face default cache".to_string()
}

pub(crate) fn semantic_model_runtime_status(
    spec: &SemanticModelSpec,
) -> (bool, Option<String>, Option<String>) {
    // Remote embeddings pool (headless cells): compute happens over HTTP,
    // so no local runtime (candle weights) is required for any model.
    // Reachability is verified at backend load.
    if let Some(endpoint) = crate::semantic_model_remote::remote_embeddings_endpoint() {
        return (
            true,
            Some(format!("remote embeddings pool at {endpoint}")),
            None,
        );
    }
    // Every local backend is candle-based (pure Rust, compiled in): there is
    // no external runtime to probe. The ONNX-era dylib checks retired with
    // the ONNX backends themselves (2026-08); model weights are fetched to
    // the HF cache during prepare, which reports its own failures.
    match &spec.backend {
        SemanticModelBackend::CandleBert(_) => (true, None, None),
        SemanticModelBackend::NomicV2Moe => (true, None, None),
        // Unreachable in practice: the remote check above always returns
        // first for a RemotePool spec. Kept so the match stays exhaustive.
        SemanticModelBackend::RemotePool => (true, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_model_catalog::{
        semantic_model_spec_by_id, SEMANTIC_MODEL_BGE_SMALL_CANDLE_ID,
        SEMANTIC_MODEL_NOMIC_V2_MOE_ID,
    };
    use crate::semantic_model_remote::test_support::remote_embeddings_test_serial;
    use crate::semantic_model_remote::REMOTE_EMBEDDINGS_ENV;

    #[test]
    fn nomic_runtime_is_available_without_onnx() {
        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let spec = semantic_model_spec_by_id(SEMANTIC_MODEL_NOMIC_V2_MOE_ID)
            .expect("nomic model spec should exist");
        let (available, reason, setup_hint) = semantic_model_runtime_status(&spec);

        assert!(available);
        assert!(reason.is_none());
        assert!(setup_hint.is_none());
    }

    #[test]
    fn candle_bert_runtime_is_always_available() {
        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(REMOTE_EMBEDDINGS_ENV);
        let spec = semantic_model_spec_by_id(SEMANTIC_MODEL_BGE_SMALL_CANDLE_ID)
            .expect("candle BGE model spec should exist");
        let (available, reason, setup_hint) = semantic_model_runtime_status(&spec);

        assert!(available);
        assert!(reason.is_none());
        assert!(setup_hint.is_none());
    }
}
