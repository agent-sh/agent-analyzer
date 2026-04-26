//! Repo walk → chunk → embed → emit JSON.
//!
//! Two entry points:
//!
//! - [`run_scan`] — full embed of every eligible file
//! - [`run_update`] — delta-only: read existing sidecar, hash files,
//!   re-embed only changed/added, drop removed
//!
//! Both produce an [`EmbeddingsDocument`] which the caller serializes to
//! stdout for the main binary's `set-embeddings` to consume.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use analyzer_core::limits::MAX_WALK_FILE_SIZE;
use analyzer_core::secrets::is_secret_like;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};

use crate::chunk::{Chunk, Granularity, chunk_file};
use crate::embedder::Embedder;
use crate::schema::{EmbeddingVector, EmbeddingsDocument, FileEmbeddings, ScanMeta};
use crate::sidecar::Sidecar;

/// Inputs shared by [`run_scan`] and [`run_update`].
pub struct ScanOptions {
    pub repo: PathBuf,
    pub granularity: Granularity,
    /// Stored vector dimensionality. May be smaller than the embedder's
    /// native dim — vectors are truncated then L2-renormalized
    /// (Matryoshka).
    pub dim: usize,
    /// Cap on total files visited. Matches the existing `top-500`
    /// pattern used by the descriptor and summary agents.
    pub max_files: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            repo: PathBuf::from("."),
            granularity: Granularity::PerFunction,
            dim: 256,
            max_files: 500,
        }
    }
}

/// Walk the repo, embed every eligible file, return the document.
pub fn run_scan(embedder: &mut dyn Embedder, opts: &ScanOptions) -> Result<EmbeddingsDocument> {
    let files = walk_repo(&opts.repo, opts.max_files)?;
    let mut doc = empty_document(embedder, opts);
    embed_files(embedder, opts, &files, &mut doc)?;
    Ok(doc)
}

/// Read the existing sidecar (if any), hash files, re-embed only
/// changed/added, drop removed. Returns the updated document.
///
/// `sidecar_path` is where the existing on-disk sidecar lives; it does
/// not need to exist (a missing sidecar triggers a full scan).
pub fn run_update(
    embedder: &mut dyn Embedder,
    opts: &ScanOptions,
    sidecar_path: &Path,
) -> Result<EmbeddingsDocument> {
    let existing_hashes = load_existing_hashes(sidecar_path, embedder.variant().id())?;
    let files = walk_repo(&opts.repo, opts.max_files)?;

    let mut to_embed: Vec<PathBuf> = Vec::new();
    let mut unchanged: HashMap<String, FileEmbeddings> = HashMap::new();
    for (path, content_hash) in &files {
        let rel = relative_path(&opts.repo, path)?;
        if let Some((old_hash, vectors)) = existing_hashes.get(&rel)
            && old_hash == content_hash
        {
            unchanged.insert(rel, vectors.clone());
        } else {
            to_embed.push(path.clone());
        }
    }

    let mut doc = empty_document(embedder, opts);
    doc.files.extend(unchanged);
    let to_embed_subset: Vec<(PathBuf, String)> = to_embed
        .into_iter()
        .filter_map(|p| {
            files
                .iter()
                .find(|(fp, _)| fp == &p)
                .map(|(fp, h)| (fp.clone(), h.clone()))
        })
        .collect();
    embed_files(embedder, opts, &to_embed_subset, &mut doc)?;
    Ok(doc)
}

fn empty_document(embedder: &mut dyn Embedder, opts: &ScanOptions) -> EmbeddingsDocument {
    EmbeddingsDocument {
        meta: ScanMeta {
            model: embedder.variant(),
            model_id: embedder.variant().id().to_string(),
            granularity: opts.granularity,
            dim: opts.dim,
            generated_at: Utc::now(),
        },
        files: HashMap::new(),
    }
}

fn embed_files(
    embedder: &mut dyn Embedder,
    opts: &ScanOptions,
    files: &[(PathBuf, String)],
    doc: &mut EmbeddingsDocument,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let native_dim = embedder.variant().native_dim();
    if opts.dim > native_dim {
        bail!(
            "requested dim {} exceeds model native dim {} for {:?}",
            opts.dim,
            native_dim,
            embedder.variant()
        );
    }

    // Build a flat list of (file_path, chunk) pairs across all files,
    // then submit one batched embed call per file (fastembed batches
    // internally so we benefit from contiguous calls). We could also
    // batch across files, but keeping per-file batching avoids holding
    // every text in memory at once on huge repos.
    for (path, content_hash) in files {
        let rel = relative_path(&opts.repo, path)?;
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary or unreadable; skip silently
        };
        let chunks = chunk_file(path, &content, opts.granularity);
        if chunks.is_empty() {
            continue;
        }
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let raw_vectors = embedder.embed(&texts)?;
        if raw_vectors.len() != chunks.len() {
            bail!(
                "embedder returned {} vectors for {} chunks in {}",
                raw_vectors.len(),
                chunks.len(),
                rel
            );
        }
        let truncated: Vec<EmbeddingVector> = raw_vectors
            .iter()
            .zip(chunks.iter())
            .map(|(raw, chunk)| build_vector(raw, chunk, opts.dim, native_dim))
            .collect();

        doc.files.insert(
            rel,
            FileEmbeddings {
                content_hash: content_hash.clone(),
                vectors: truncated,
            },
        );
    }
    Ok(())
}

fn build_vector(raw: &[f32], chunk: &Chunk, dim: usize, native_dim: usize) -> EmbeddingVector {
    let truncated = if dim < native_dim {
        l2_normalize(&raw[..dim])
    } else {
        raw.to_vec()
    };
    EmbeddingVector {
        kind: chunk.kind,
        name: chunk.name.clone(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        values: truncated,
    }
}

fn l2_normalize(values: &[f32]) -> Vec<f32> {
    let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return values.to_vec();
    }
    values.iter().map(|v| v / norm).collect()
}

fn load_existing_hashes(
    sidecar_path: &Path,
    expected_model_id: &str,
) -> Result<HashMap<String, (String, FileEmbeddings)>> {
    if !sidecar_path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = fs::read(sidecar_path).context("read sidecar")?;
    let sidecar = Sidecar::from_bytes(&bytes[..])?;
    if sidecar.header.model_id != expected_model_id {
        // Model changed — drop everything, force full rebuild.
        return Ok(HashMap::new());
    }
    // The sidecar stores vectors but not content hashes. Hashes live in
    // the JSON artifact's `embeddingsMeta`. For now we conservatively
    // re-embed everything when only the binary sidecar is available.
    // Once the JSON merge lands (set-embeddings), hashes flow back in.
    // TODO(next-pr): read companion hash file or pull from artifact.
    let _ = sidecar;
    Ok(HashMap::new())
}

/// Maximum size of any single file the embed/slop walkers will read.
fn walk_repo(repo: &Path, max_files: usize) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    for entry in WalkBuilder::new(repo)
        .standard_filters(true)
        .hidden(true)
        .max_filesize(Some(MAX_WALK_FILE_SIZE))
        .build()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().to_path_buf();
        if !is_eligible(&path) {
            continue;
        }
        if is_secret_like(&path) {
            continue;
        }
        let content = match fs::read(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let hash = format!("sha256:{:x}", Sha256::digest(&content));
        out.push((path, hash));
        if out.len() >= max_files {
            break;
        }
    }
    Ok(out)
}

fn is_eligible(path: &Path) -> bool {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "java"
            | "md"
            | "markdown"
    )
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let stripped = path.strip_prefix(root).unwrap_or(path);
    Ok(stripped.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    /// Test embedder that returns deterministic vectors so we can verify
    /// the scan/update plumbing without downloading a real model.
    struct StubEmbedder {
        variant: crate::embedder::ModelVariant,
        seed: f32,
    }

    impl Embedder for StubEmbedder {
        fn variant(&self) -> crate::embedder::ModelVariant {
            self.variant
        }
        fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let dim = self.variant.native_dim();
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    (0..dim)
                        .map(|j| {
                            self.seed
                                + (i as f32) * 0.01
                                + (j as f32) * 0.001
                                + (t.len() as f32) * 0.0001
                        })
                        .collect()
                })
                .collect())
        }
    }

    fn make_repo(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut f = File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }
        dir
    }

    #[test]
    fn run_scan_embeds_eligible_files_and_skips_others() {
        let dir = make_repo(&[
            ("a.rs", "fn alpha() { 1 }\n"),
            ("b.py", "def bar():\n    return 1\n"),
            ("c.bin", "binary garbage"),
            ("README.md", "# Title\n\nbody\n"),
        ]);
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 0.1,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 32,
            max_files: 100,
        };
        let doc = run_scan(&mut embedder, &opts).unwrap();

        assert_eq!(doc.meta.dim, 32);
        assert_eq!(doc.meta.model_id, "bge-small-en-v1.5-q8");
        assert_eq!(doc.files.len(), 3);
        assert!(doc.files.contains_key("a.rs"));
        assert!(doc.files.contains_key("b.py"));
        assert!(doc.files.contains_key("README.md"));
        assert!(!doc.files.contains_key("c.bin"));
    }

    #[test]
    fn truncated_vectors_are_l2_normalized() {
        let dir = make_repo(&[("a.rs", "fn alpha() { 1 }\n")]);
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 1.0,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 16,
            max_files: 10,
        };
        let doc = run_scan(&mut embedder, &opts).unwrap();
        let vec = &doc.files["a.rs"].vectors[0].values;
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[test]
    fn dim_exceeding_native_errors() {
        let dir = make_repo(&[("a.rs", "fn alpha() { 1 }\n")]);
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 0.1,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 99999,
            max_files: 10,
        };
        let err = run_scan(&mut embedder, &opts).unwrap_err();
        assert!(err.to_string().contains("exceeds model native dim"));
    }

    #[test]
    fn empty_repo_yields_empty_document() {
        let dir = TempDir::new().unwrap();
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 0.1,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 32,
            max_files: 10,
        };
        let doc = run_scan(&mut embedder, &opts).unwrap();
        assert!(doc.files.is_empty());
        assert_eq!(doc.meta.dim, 32);
    }

    #[test]
    fn max_files_caps_walk() {
        let dir = make_repo(&[
            ("a.rs", "fn a() {}\n"),
            ("b.rs", "fn b() {}\n"),
            ("c.rs", "fn c() {}\n"),
        ]);
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 0.1,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 32,
            max_files: 2,
        };
        let doc = run_scan(&mut embedder, &opts).unwrap();
        assert_eq!(doc.files.len(), 2);
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let dir = make_repo(&[("nested/inner/a.rs", "fn a() {}\n")]);
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 0.1,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 32,
            max_files: 10,
        };
        let doc = run_scan(&mut embedder, &opts).unwrap();
        assert!(doc.files.contains_key("nested/inner/a.rs"));
    }

    #[test]
    fn walk_skips_files_larger_than_cap() {
        let dir = TempDir::new().unwrap();
        // Small eligible file - must be kept.
        {
            let p = dir.path().join("small.rs");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"fn a() {}\n").unwrap();
        }
        // 10 MiB file with an eligible extension - must be skipped.
        {
            let p = dir.path().join("big.rs");
            let mut f = File::create(&p).unwrap();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..10 {
                f.write_all(&chunk).unwrap();
            }
        }
        let files = walk_repo(dir.path(), 100).unwrap();
        let rels: Vec<String> = files
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            rels.iter().any(|p| p == "small.rs"),
            "small.rs missing: {rels:?}"
        );
        assert!(
            !rels.iter().any(|p| p == "big.rs"),
            "10 MiB big.rs should have been skipped: {rels:?}"
        );
    }

    #[test]
    fn walk_excludes_dotfiles_and_secret_patterns() {
        let dir = make_repo(&[
            ("src/a.rs", "fn a() {}\n"),
            (".env", "SECRET=hunter2\n"),
            (".git/config", "[core]\n"),
            (".ssh/id_rsa", "-----BEGIN RSA PRIVATE KEY-----\n"),
            ("keys/server.pem", "-----BEGIN CERTIFICATE-----\n"),
            ("keys/server.key", "-----BEGIN PRIVATE KEY-----\n"),
        ]);
        let files = walk_repo(dir.path(), 100).unwrap();
        let rels: Vec<String> = files
            .iter()
            .map(|(p, _)| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(
            rels.iter().any(|p| p == "src/a.rs"),
            "src/a.rs missing: {rels:?}"
        );
        for forbidden in [
            ".env",
            ".git/config",
            ".ssh/id_rsa",
            "keys/server.pem",
            "keys/server.key",
        ] {
            assert!(
                !rels.iter().any(|p| p == forbidden),
                "secret-like path {forbidden} should have been excluded: {rels:?}"
            );
        }
    }

    #[test]
    fn run_update_with_no_existing_sidecar_full_embeds() {
        let dir = make_repo(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let mut embedder = StubEmbedder {
            variant: crate::embedder::ModelVariant::Small,
            seed: 0.1,
        };
        let opts = ScanOptions {
            repo: dir.path().to_path_buf(),
            granularity: Granularity::PerFile,
            dim: 32,
            max_files: 10,
        };
        let nonexistent = dir.path().join("missing.embeddings.bin");
        let doc = run_update(&mut embedder, &opts, &nonexistent).unwrap();
        assert_eq!(doc.files.len(), 2);
    }
}
