//! Embedder trait and model variant definitions.
//!
//! Concrete implementations (ONNX Runtime + tokenizers + downloaded model
//! files) land in a follow-up PR. This module fixes the API surface so the
//! rest of the crate (chunking, sidecar, CLI) can compile and be tested
//! independently of the model layer.

use serde::{Deserialize, Serialize};

/// Which embedding model is installed.
///
/// Picked by the user via the repo-intel skill prompt and persisted in
/// `preference.json` as `embedder: "small" | "big"`. `"none"` means no
/// embedder is installed and this crate is not invoked at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelVariant {
    /// BAAI/bge-small-en-v1.5 Q8 ONNX (~30 MB). English-only, weaker on
    /// code, older architecture. The "good for laptops" option.
    Small,
    /// google/embeddinggemma-300m Q4 ONNX (~195 MB). SOTA at <500M params,
    /// code-aware, multilingual, 2048 ctx, Matryoshka. The recommended
    /// option for users with the disk.
    Big,
}

impl ModelVariant {
    /// Stable identifier embedded in the sidecar so the loader can detect
    /// model swaps and trigger a full rebuild instead of mixing vector
    /// spaces.
    pub fn id(self) -> &'static str {
        match self {
            ModelVariant::Small => "bge-small-en-v1.5-q8",
            ModelVariant::Big => "embeddinggemma-300m-q4",
        }
    }

    /// Native output dimensionality of the model. Vectors may be truncated
    /// (Matryoshka) below this when stored.
    pub fn native_dim(self) -> usize {
        match self {
            ModelVariant::Small => 384,
            ModelVariant::Big => 768,
        }
    }
}

/// Stateless embedder. Takes a batch of texts, returns a vector per text
/// at the model's native dimensionality.
///
/// Implementations are expected to handle their own batching, tokenization,
/// and model state. Callers pass slices of any reasonable size.
pub trait Embedder: Send + Sync {
    /// The model variant this embedder was constructed with.
    fn variant(&self) -> ModelVariant;

    /// Embed a batch of texts. Returns one vector per input text, each of
    /// length `self.variant().native_dim()`.
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}
