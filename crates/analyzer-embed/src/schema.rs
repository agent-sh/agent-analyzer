//! JSON schema for the document piped from `agent-analyzer-embed` to
//! `agent-analyzer set-embeddings`.
//!
//! The wire format intentionally mirrors the existing
//! `set-descriptors`/`set-summary` pattern: the embed binary writes a
//! self-contained JSON document to stdout; the main binary reads it,
//! validates, and merges into the on-disk artifact.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::chunk::{ChunkKind, Granularity};
use crate::embedder::ModelVariant;

/// The complete document piped between binaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingsDocument {
    pub meta: ScanMeta,
    /// Keyed by repo-relative file path.
    pub files: HashMap<String, FileEmbeddings>,
}

/// Metadata describing how this scan was produced. The main binary uses
/// this to decide between merge and full-rebuild on subsequent runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanMeta {
    pub model: ModelVariant,
    pub model_id: String,
    pub granularity: Granularity,
    /// Stored vector dimensionality. May be smaller than the model's
    /// native dim when truncation (Matryoshka) is applied.
    pub dim: usize,
    pub generated_at: DateTime<Utc>,
}

/// Embeddings for one file. The file's content hash lets `update` skip
/// re-embedding when the file is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEmbeddings {
    /// `sha256:<hex>` of the file content used for this embedding.
    pub content_hash: String,
    pub vectors: Vec<EmbeddingVector>,
}

/// One embedding vector + its source span.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingVector {
    pub kind: ChunkKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Raw f32 vector at `ScanMeta::dim` length. JSON-friendly format;
    /// the on-disk sidecar packs more efficiently (see [`crate::sidecar`]).
    pub values: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let doc = EmbeddingsDocument {
            meta: ScanMeta {
                model: ModelVariant::Small,
                model_id: "bge-small-en-v1.5-q8".into(),
                granularity: Granularity::PerFile,
                dim: 128,
                generated_at: Utc::now(),
            },
            files: {
                let mut m = HashMap::new();
                m.insert(
                    "src/foo.rs".into(),
                    FileEmbeddings {
                        content_hash: "sha256:abc".into(),
                        vectors: vec![EmbeddingVector {
                            kind: ChunkKind::File,
                            name: None,
                            start_line: 1,
                            end_line: 10,
                            values: vec![0.1, 0.2, 0.3],
                        }],
                    },
                );
                m
            },
        };
        let json = serde_json::to_string(&doc).expect("serialize");
        let back: EmbeddingsDocument = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.meta.dim, 128);
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files["src/foo.rs"].vectors[0].values.len(), 3);
    }

    #[test]
    fn variant_serializes_lowercase() {
        let json = serde_json::to_string(&ModelVariant::Big).unwrap();
        assert_eq!(json, "\"big\"");
    }
}
