//! Concrete [`Embedder`] implementation backed by fastembed-rs.
//!
//! `fastembed` wraps ONNX Runtime + HuggingFace tokenizers + model
//! download / cache. We pick from its `EmbeddingModel` enum based on
//! [`ModelVariant`] and pass through the embed call.
//!
//! Model files land in fastembed's default cache (`~/.cache/fastembed/`
//! or platform equivalent). The skill prompts the user once at install
//! time and persists the choice; this binary just consumes that decision.

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::embedder::{Embedder, ModelVariant};

/// Embedder that delegates to a `fastembed::TextEmbedding`.
pub struct FastEmbedder {
    variant: ModelVariant,
    model: TextEmbedding,
}

impl FastEmbedder {
    /// Construct an embedder for the given variant. On first use this
    /// downloads the model files; subsequent constructions read from the
    /// fastembed cache.
    pub fn new(variant: ModelVariant) -> Result<Self> {
        let opts = InitOptions::new(map_variant(variant)).with_show_download_progress(true);
        let model = TextEmbedding::try_new(opts)
            .with_context(|| format!("initialize fastembed model for variant {variant:?}"))?;
        Ok(Self { variant, model })
    }
}

impl Embedder for FastEmbedder {
    fn variant(&self) -> ModelVariant {
        self.variant
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<&str> = texts.iter().map(String::as_str).collect();
        // fastembed handles batching internally; passing None uses its
        // default batch size, which is tuned per model.
        self.model
            .embed(docs, None)
            .with_context(|| format!("embed {} texts with {:?}", texts.len(), self.variant))
    }
}

fn map_variant(v: ModelVariant) -> EmbeddingModel {
    match v {
        ModelVariant::Small => EmbeddingModel::BGESmallENV15Q,
        ModelVariant::Big => EmbeddingModel::EmbeddingGemma300M,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_maps_to_expected_fastembed_model() {
        // We map small -> BGESmallENV15Q (quantized, ~30 MB) and
        // big -> EmbeddingGemma300M (~195 MB). This test just locks in
        // the mapping so a future fastembed enum change is caught.
        assert!(matches!(
            map_variant(ModelVariant::Small),
            EmbeddingModel::BGESmallENV15Q
        ));
        assert!(matches!(
            map_variant(ModelVariant::Big),
            EmbeddingModel::EmbeddingGemma300M
        ));
    }

    // Integration tests that actually download + run the model live in
    // tests/integration_embed.rs, gated behind --ignored so CI doesn't
    // pull ~225 MB of model files on every PR. Run locally with:
    //
    //   cargo test -p analyzer-embed -- --ignored
}
