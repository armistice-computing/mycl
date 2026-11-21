//! Candle-native BERT sentence embedder — the zero-dependency fast tier.
//!
//! Runs classic BERT-family retrieval models (BGE-small, all-MiniLM) through
//! `candle-transformers`' reference BERT implementation: Metal on macOS when
//! available, otherwise CPU,
//! no ONNX runtime, no dylib probing. This is the same dependency story as
//! fastembed's Nomic V2 MoE candle path (`SemanticModelBackend::NomicV2Moe`),
//! extended down to the small models so every local tier is bundleable.
//!
//! Weights, tokenizer, and config are fetched once from the Hugging Face hub
//! via `hf-hub` and cached under the standard HF cache directory (the same
//! cache `semantic_model_cache_path_hint` reports). The `hf_repo` in the
//! model spec must therefore point at a repo that ships `model.safetensors` +
//! `tokenizer.json` (the canonical upstream repos, not the `-onnx` mirrors).
//!
//! Pooling differs per model family and is part of the spec contract:
//! BGE pools the CLS token; MiniLM mean-pools over the attention mask. Both
//! L2-normalize, matching their sentence-transformers reference outputs —
//! the golden-vector test in this module is the guard on that parity.

use candle_core::{DType, IndexOp, Tensor, D};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE as BERT_DTYPE};
use hf_hub::api::sync::ApiBuilder;
use std::path::PathBuf;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CandleBertPooling {
    /// First-token (CLS) pooling — the BGE family's contract.
    Cls,
    /// Attention-mask-weighted mean pooling — the MiniLM family's contract.
    Mean,
}

pub(crate) struct CandleBertEmbedding {
    model: BertModel,
    device: candle_core::Device,
    tokenizer: Tokenizer,
    pooling: CandleBertPooling,
}

impl CandleBertEmbedding {
    pub(crate) fn from_hf(
        repo_id: &str,
        pooling: CandleBertPooling,
        max_length: usize,
    ) -> Result<Self, String> {
        let api = ApiBuilder::new()
            .with_progress(false)
            .build()
            .map_err(|error| format!("initialize Hugging Face hub client: {error}"))?;
        let repo = api.model(repo_id.to_string());

        let cfg_path: PathBuf = repo
            .get("config.json")
            .map_err(|error| format!("fetch {repo_id} config.json: {error}"))?;
        let cfg: BertConfig = serde_json::from_slice(
            &std::fs::read(&cfg_path)
                .map_err(|error| format!("read {}: {error}", cfg_path.display()))?,
        )
        .map_err(|error| format!("parse {repo_id} config.json as BERT config: {error}"))?;

        let weights_path: PathBuf = repo
            .get("model.safetensors")
            .map_err(|error| format!("fetch {repo_id} model.safetensors: {error}"))?;
        let mut device = embedding_device();
        let load = |device: &candle_core::Device| -> Result<BertModel, String> {
            let vb = unsafe {
                VarBuilder::from_mmaped_safetensors(std::slice::from_ref(&weights_path), BERT_DTYPE, device)
                    .map_err(|error| format!("map {repo_id} safetensors: {error}"))?
            };
            BertModel::load(vb, &cfg).map_err(|error| format!("load {repo_id} BERT weights: {error}"))
        };
        let model = match load(&device) {
            Ok(model) => model,
            Err(error) if device.is_metal() => {
                log::warn!("Metal BERT initialization failed; falling back to CPU: {error}");
                device = candle_core::Device::Cpu;
                load(&device)?
            }
            Err(error) => return Err(error),
        };
        log::info!("Candle BERT {repo_id} embedding device: {:?}", device.location());

        let tok_path: PathBuf = repo
            .get("tokenizer.json")
            .map_err(|error| format!("fetch {repo_id} tokenizer.json: {error}"))?;
        let mut tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|error| format!("load {repo_id} tokenizer: {error}"))?;
        let _ = tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_id: cfg.pad_token_id as u32,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));
        let _ = tokenizer.with_truncation(Some(TruncationParams {
            max_length,
            ..Default::default()
        }));

        Ok(Self {
            model,
            device,
            tokenizer,
            pooling,
        })
    }

    pub(crate) fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.iter().map(String::as_str).collect::<Vec<_>>(), true)
            .map_err(|error| format!("tokenize batch: {error}"))?;

        let batch_size = encodings.len();
        let seq_len = encodings[0].len();

        let mut input_ids_vec: Vec<u32> = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask_vec: Vec<f32> = Vec::with_capacity(batch_size * seq_len);
        for encoding in &encodings {
            input_ids_vec.extend(encoding.get_ids().iter().copied());
            attention_mask_vec.extend(encoding.get_attention_mask().iter().map(|&m| m as f32));
        }

        let device = &self.device;
        let input_ids = Tensor::from_vec(input_ids_vec, (batch_size, seq_len), &device)
            .map_err(|error| format!("build input tensor: {error}"))?;
        let attention_mask = Tensor::from_vec(attention_mask_vec, (batch_size, seq_len), &device)
            .map_err(|error| format!("build attention mask tensor: {error}"))?;
        let token_type_ids = Tensor::zeros((batch_size, seq_len), DType::U32, &device)
            .map_err(|error| format!("build token type tensor: {error}"))?;

        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|error| format!("BERT forward pass: {error}"))?;

        let pooled = match self.pooling {
            CandleBertPooling::Cls => hidden
                .i((.., 0))
                .map_err(|error| format!("CLS pooling: {error}"))?,
            CandleBertPooling::Mean => {
                mean_pool(&hidden, &attention_mask).map_err(|error| format!("mean pooling: {error}"))?
            }
        };
        let normalized =
            l2_normalize(&pooled).map_err(|error| format!("L2 normalization: {error}"))?;
        normalized
            .to_vec2::<f32>()
            .map_err(|error| format!("read embedding vectors: {error}"))
    }
}

fn embedding_device() -> candle_core::Device {
    #[cfg(target_os = "macos")]
    match candle_core::Device::new_metal(0) {
        Ok(device) => return device,
        Err(error) => log::warn!("Metal unavailable; using CPU embeddings: {error}"),
    }
    candle_core::Device::Cpu
}

fn mean_pool(hidden: &Tensor, attention_mask: &Tensor) -> candle_core::Result<Tensor> {
    let mask = attention_mask.to_dtype(hidden.dtype())?.unsqueeze(2)?;
    let masked = hidden.broadcast_mul(&mask)?;
    let summed = masked.sum(1)?;
    let count = mask.sum(1)?.clamp(1e-9, f64::MAX)?;
    summed.broadcast_div(&count)
}

fn l2_normalize(xs: &Tensor) -> candle_core::Result<Tensor> {
    let sum_sq = xs.sqr()?.sum_keepdim(D::Minus1)?;
    let eps = Tensor::new(&[1e-12f32], xs.device())?.broadcast_as(sum_sq.shape())?;
    let norm = sum_sq.to_dtype(DType::F32)?.add(&eps)?.sqrt()?;
    xs.to_dtype(DType::F32)?.broadcast_div(&norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden-vector parity against the sentence-transformers reference
    // implementation (v6.0.0, torch CPU, normalize_embeddings=True),
    // captured 2026-08-26. If these drift, the embedder is producing a
    // DIFFERENT vector space than the canonical model — that is an index-
    // corrupting bug, not a tolerance nit. Ignored by default: first run
    // downloads model weights from the Hugging Face hub.

    const SENTENCES: [&str; 3] = [
        "The garden grows toward the light.",
        "Plants in the garden reach for sunlight.",
        "The quarterly invoice is due on Tuesday.",
    ];

    const COMPONENT_TOLERANCE: f32 = 2e-3;
    const COSINE_TOLERANCE: f32 = 5e-3;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    fn assert_first8_close(actual: &[f32], expected: &[f32; 8], label: &str) {
        for (index, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (a - e).abs() < COMPONENT_TOLERANCE,
                "{label} component {index}: got {a}, reference {e}"
            );
        }
    }

    fn assert_unit_norm(vector: &[f32], label: &str) {
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "{label} should be L2-normalized, got norm {norm}"
        );
    }

    #[test]
    #[ignore = "downloads model weights from the Hugging Face hub"]
    fn bge_small_matches_sentence_transformers_reference() {
        let model = CandleBertEmbedding::from_hf("BAAI/bge-small-en-v1.5", CandleBertPooling::Cls, 512)
            .expect("load BGE small");
        let texts: Vec<String> = SENTENCES.iter().map(|s| s.to_string()).collect();
        let vectors = model.embed(&texts).expect("embed batch");

        assert_eq!(vectors.len(), 3);
        for (vector, label) in vectors.iter().zip(["s0", "s1", "s2"]) {
            assert_eq!(vector.len(), 384);
            assert_unit_norm(vector, label);
        }

        assert_first8_close(
            &vectors[0],
            &[0.019014, -0.022009, 0.010611, -0.005733, 0.077213, 0.033736, 0.017756, -0.034633],
            "bge s0",
        );
        assert_first8_close(
            &vectors[1],
            &[0.004468, -0.028671, 0.025497, -0.005215, 0.094656, 0.034778, 0.029410, -0.059267],
            "bge s1",
        );
        assert_first8_close(
            &vectors[2],
            &[-0.046052, -0.029978, -0.002398, -0.053392, -0.002156, -0.039483, 0.015610, 0.011344],
            "bge s2",
        );

        let similar = cosine(&vectors[0], &vectors[1]);
        let dissimilar = cosine(&vectors[0], &vectors[2]);
        assert!((similar - 0.908574).abs() < COSINE_TOLERANCE, "got {similar}");
        assert!((dissimilar - 0.458356).abs() < COSINE_TOLERANCE, "got {dissimilar}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires Metal hardware and cached BGE weights"]
    fn bge_small_runs_on_metal() {
        candle_core::Device::new_metal(0).expect("Metal hardware required for this acceptance test");
        let model = CandleBertEmbedding::from_hf("BAAI/bge-small-en-v1.5", CandleBertPooling::Cls, 512)
            .expect("load BGE on Metal");
        assert!(model.device.is_metal(), "must not silently pass on CPU");
        let vectors = model.embed(&["A short note.".into(), "The garden grows toward the light.".into()]).unwrap();
        assert_eq!(vectors.len(), 2);
        for vector in &vectors {
            assert_eq!(vector.len(), 384);
            assert!(vector.iter().all(|v| v.is_finite()));
            assert_unit_norm(vector, "Metal BGE");
        }
    }

    #[test]
    #[ignore = "downloads model weights from the Hugging Face hub"]
    fn minilm_matches_sentence_transformers_reference() {
        let model = CandleBertEmbedding::from_hf(
            "sentence-transformers/all-MiniLM-L6-v2",
            CandleBertPooling::Mean,
            256,
        )
        .expect("load MiniLM");
        let texts: Vec<String> = SENTENCES.iter().map(|s| s.to_string()).collect();
        let vectors = model.embed(&texts).expect("embed batch");

        assert_eq!(vectors.len(), 3);
        for (vector, label) in vectors.iter().zip(["s0", "s1", "s2"]) {
            assert_eq!(vector.len(), 384);
            assert_unit_norm(vector, label);
        }

        assert_first8_close(
            &vectors[0],
            &[0.009764, 0.016782, -0.044181, 0.021654, 0.066700, 0.015724, 0.019343, -0.064003],
            "minilm s0",
        );
        assert_first8_close(
            &vectors[1],
            &[-0.012280, 0.044874, -0.025716, 0.063268, 0.038495, 0.021527, 0.025422, -0.053716],
            "minilm s1",
        );
        assert_first8_close(
            &vectors[2],
            &[-0.081998, 0.004352, 0.040108, -0.008311, -0.081164, -0.012249, -0.142110, 0.015086],
            "minilm s2",
        );

        let similar = cosine(&vectors[0], &vectors[1]);
        let dissimilar = cosine(&vectors[0], &vectors[2]);
        assert!((similar - 0.784030).abs() < COSINE_TOLERANCE, "got {similar}");
        assert!((dissimilar - (-0.066952)).abs() < COSINE_TOLERANCE, "got {dissimilar}");
    }
}
