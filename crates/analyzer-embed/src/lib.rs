//! Local embedding generation for agent-analyzer.
//!
//! This crate produces per-file or per-function vector embeddings for use
//! by downstream analyzer queries (semantic find, slop targeting,
//! stylometry-based AI authorship, semantic dup detection).
//!
//! ## Architecture
//!
//! - [`chunk`] — splits source files into embedding units (per-file or
//!   per-function via tree-sitter).
//! - [`sidecar`] — packed binary on-disk format for embedding vectors.
//!   Lives next to `repo-intel.json` as `repo-intel.embeddings.bin`.
//! - [`schema`] — JSON output schema piped to `agent-analyzer set-embeddings`.
//! - [`embedder`] — trait for an embedding model. Concrete implementations
//!   land in a follow-up PR (ONNX Runtime + EmbeddingGemma / BGE-small).
//!
//! ## Workflow
//!
//! 1. `agent-analyzer-embed scan` walks the repo, chunks files, calls the
//!    [`embedder::Embedder`] for each chunk, writes a JSON document.
//! 2. `agent-analyzer-embed update` reads the existing sidecar, hashes
//!    each file, and only re-embeds changed/added files.
//! 3. JSON is piped to `agent-analyzer set-embeddings` which merges into
//!    the sidecar binary file.

pub mod chunk;
pub mod embedder;
pub mod schema;
pub mod sidecar;

pub use chunk::{Chunk, ChunkKind, Granularity, chunk_file};
pub use embedder::{Embedder, ModelVariant};
pub use schema::{EmbeddingsDocument, FileEmbeddings, ScanMeta};
pub use sidecar::{Sidecar, SidecarHeader};
