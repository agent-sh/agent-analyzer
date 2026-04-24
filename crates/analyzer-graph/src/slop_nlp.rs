//! NLP-enabled slop detectors that consume the embedding sidecar.
//!
//! These patterns extend [`crate::slop_targets`] when the user has
//! opted into the embedder. They degrade silently to empty results
//! when no sidecar is available, so the unified `slop-targets` query
//! returns the same shape regardless.
//!
//! Detectors landed:
//!
//! * **stylistic outliers** — per-file embedding centroid versus the
//!   repo-wide centroid; files with the highest cosine distance are
//!   stylistically unusual (often a sign of pasted-in AI code or
//!   abandoned third-party blobs). Honest framing: this is *outlier
//!   detection*, not "AI authorship" — same lesson as the deleted
//!   `aiAttribution` field, just applied to embeddings instead of git
//!   metadata.
//! * **semantic duplicates** — pairs of per-function embeddings whose
//!   cosine similarity exceeds a threshold across different files,
//!   indicating duplicate logic that AST-shape match would miss.
//!
//! Detectors deferred (require additional scan modes in
//! `agent-analyzer-embed`):
//!
//! * **comment-restates-code** — needs separate embedding of comment
//!   text vs adjacent code, which requires extending the embedder's
//!   scan to emit two parallel vectors per chunk.
//! * **doc-drift v2** — needs embedded markdown sections plus per-
//!   function code embeddings, then cross-modal comparison.

use analyzer_embed::sidecar::{Sidecar, StoredVector};

use crate::slop_targets::{SlopSuspect, SlopTarget, SlopTier};

/// Add NLP-derived [`SlopTarget`] rows. Returns an empty vec when the
/// sidecar is absent or cannot be interpreted.
pub fn nlp_targets(sidecar: &Sidecar, top_per_kind: usize) -> Vec<SlopTarget> {
    let mut out = Vec::new();
    out.extend(stylistic_outliers(sidecar, top_per_kind));
    out.extend(semantic_duplicates(sidecar, top_per_kind));
    out
}

// ── Stylistic outliers ───────────────────────────────────────────

fn stylistic_outliers(sidecar: &Sidecar, top: usize) -> Vec<SlopTarget> {
    if sidecar.vectors.len() < 5 {
        // Too few files to define a meaningful baseline.
        return Vec::new();
    }
    let dim = sidecar.header.dim;
    if dim == 0 {
        return Vec::new();
    }

    // Per-file centroid: mean of all per-chunk vectors in that file.
    let mut file_centroids: Vec<(String, Vec<f32>)> = Vec::new();
    for (path, vectors) in &sidecar.vectors {
        if vectors.is_empty() {
            continue;
        }
        let centroid = mean_vectors(vectors, dim);
        file_centroids.push((path.clone(), centroid));
    }
    if file_centroids.len() < 5 {
        return Vec::new();
    }

    // Repo centroid: mean of file centroids (so each file weighted
    // equally regardless of chunk count).
    let mut repo_mean = vec![0.0_f32; dim];
    for (_, c) in &file_centroids {
        for (i, v) in c.iter().enumerate() {
            repo_mean[i] += v;
        }
    }
    let n = file_centroids.len() as f32;
    for v in repo_mean.iter_mut() {
        *v /= n;
    }
    let repo_norm = repo_mean.clone();

    let mut distances: Vec<(String, f32)> = file_centroids
        .iter()
        .map(|(path, centroid)| (path.clone(), cosine_distance(centroid, &repo_norm)))
        .collect();
    distances.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let dmax = distances.first().map(|(_, d)| *d).unwrap_or(0.0);
    distances
        .into_iter()
        .take(top)
        .filter(|(_, d)| *d > 0.15)
        .map(|(path, dist)| SlopTarget::File {
            path,
            tier: SlopTier::Sonnet,
            score: 5.0 + (dist / dmax.max(1e-6)).clamp(0.0, 1.0) * 4.0,
            suspect: SlopSuspect::DefensiveCargoCult, // see note below
            why: format!(
                "stylistic outlier: cosine distance {:.3} from repo embedding centroid",
                dist
            ),
        })
        .collect()
}

// Reusing DefensiveCargoCult as the dispatch label is pragmatic but
// imprecise. Outliers may be over-verbose, AI-pasted, or perfectly
// fine third-party blobs. The reviewer prompt should treat this row
// as "investigate why this file looks different from the rest of the
// codebase" rather than presuming a specific failure mode. A future
// schema bump can add `SlopSuspect::StylisticOutlier` if we want a
// dedicated reviewer prompt; held back here to avoid a breaking
// schema change for a single signal.

// ── Semantic duplicates ──────────────────────────────────────────

fn semantic_duplicates(sidecar: &Sidecar, top: usize) -> Vec<SlopTarget> {
    let dim = sidecar.header.dim;
    if dim == 0 {
        return Vec::new();
    }

    // Flatten: (file, chunk_index, name, vector_f32)
    let mut chunks: Vec<(String, usize, Option<String>, Vec<f32>)> = Vec::new();
    for (path, vectors) in &sidecar.vectors {
        for (i, v) in vectors.iter().enumerate() {
            if v.values.len() != dim {
                continue;
            }
            chunks.push((path.clone(), i, v.name.clone(), v.to_f32()));
        }
    }
    let n = chunks.len();
    if n < 2 {
        return Vec::new();
    }

    // O(n^2) pairwise within-file-group exclusion. Fine up to a few
    // thousand chunks; for huge repos we'd want LSH or HNSW. Threshold
    // 0.92 picked conservatively to suppress noise; semantic-dup
    // hits typically score 0.95+.
    const SIM_THRESHOLD: f32 = 0.92;

    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            if chunks[i].0 == chunks[j].0 {
                continue;
            }
            let sim = cosine_similarity(&chunks[i].3, &chunks[j].3);
            if sim >= SIM_THRESHOLD {
                pairs.push((sim, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    pairs.truncate(top);

    pairs
        .into_iter()
        .map(|(sim, i, j)| {
            let label_a = chunks[i]
                .2
                .clone()
                .unwrap_or_else(|| format!("chunk@{}", chunks[i].1));
            let label_b = chunks[j]
                .2
                .clone()
                .unwrap_or_else(|| format!("chunk@{}", chunks[j].1));
            SlopTarget::Area {
                paths: vec![chunks[i].0.clone(), chunks[j].0.clone()],
                tier: SlopTier::Opus,
                score: 6.0 + sim * 4.0,
                suspect: SlopSuspect::SingleImpl, // see note below
                why: format!(
                    "semantic duplicate: `{}` in {} and `{}` in {} (cosine similarity {:.3})",
                    label_a, chunks[i].0, label_b, chunks[j].0, sim
                ),
            }
        })
        .collect()
}

// Mapping semantic-duplicate to SlopSuspect::SingleImpl is a stretch
// — both flag "consolidate this somewhere else" but the dispatch is
// imprecise. Same compromise as stylistic_outliers above; a follow-up
// schema bump can add `SlopSuspect::SemanticDuplicate` for a tailored
// reviewer prompt.

// ── Vector helpers ───────────────────────────────────────────────

fn mean_vectors(vectors: &[StoredVector], dim: usize) -> Vec<f32> {
    let mut acc = vec![0.0_f32; dim];
    let mut count = 0;
    for v in vectors {
        if v.values.len() != dim {
            continue;
        }
        let f = v.to_f32();
        for (i, x) in f.iter().enumerate() {
            acc[i] += x;
        }
        count += 1;
    }
    if count > 0 {
        for v in acc.iter_mut() {
            *v /= count as f32;
        }
    }
    acc
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    1.0 - cosine_similarity(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_embed::chunk::ChunkKind;
    use analyzer_embed::sidecar::{Sidecar, StoredVector};

    fn vec_for(path_seed: f32, dim: usize, n_chunks: usize) -> Vec<StoredVector> {
        (0..n_chunks)
            .map(|i| {
                let values: Vec<f32> = (0..dim)
                    .map(|d| path_seed + (i as f32) * 0.01 + (d as f32) * 0.001)
                    .collect();
                StoredVector::from_f32(ChunkKind::Function, Some(format!("fn{i}")), 1, 10, &values)
            })
            .collect()
    }

    fn populate(sidecar: &mut Sidecar, files: &[(&str, f32, usize)]) {
        for (path, seed, n) in files {
            sidecar.insert((*path).into(), vec_for(*seed, sidecar.header.dim, *n));
        }
    }

    #[test]
    fn empty_sidecar_yields_no_targets() {
        let sidecar = Sidecar::new("m".into(), 8);
        assert!(nlp_targets(&sidecar, 5).is_empty());
    }

    #[test]
    fn under_five_files_yields_no_outliers() {
        let mut sidecar = Sidecar::new("m".into(), 8);
        populate(
            &mut sidecar,
            &[("a.rs", 0.1, 2), ("b.rs", 0.1, 2), ("c.rs", 0.1, 2)],
        );
        let out = stylistic_outliers(&sidecar, 5);
        assert!(out.is_empty());
    }

    fn make_vec(values: [f32; 8], name: &str) -> StoredVector {
        StoredVector::from_f32(ChunkKind::Function, Some(name.into()), 1, 5, &values)
    }

    #[test]
    fn outlier_file_surfaces_in_a_baseline() {
        let mut sidecar = Sidecar::new("m".into(), 8);
        // Five files all point along the +x axis (similar direction);
        // alien points along +y. Repo centroid is dominated by the
        // five and aligned with +x, so the alien sits at ~90°
        // (cosine distance ~1.0) from the centroid.
        let along_x = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let along_y = [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
            sidecar.insert(
                name.into(),
                vec![make_vec(along_x, "fn0"), make_vec(along_x, "fn1")],
            );
        }
        sidecar.insert(
            "alien.rs".into(),
            vec![make_vec(along_y, "fn0"), make_vec(along_y, "fn1")],
        );

        let out = stylistic_outliers(&sidecar, 5);
        assert!(
            out.iter().any(|t| matches!(
                t,
                SlopTarget::File { path, .. } if path == "alien.rs"
            )),
            "expected alien.rs to be flagged; got {:?}",
            out
        );
    }

    #[test]
    fn semantic_duplicate_pair_surfaces_across_files() {
        let mut sidecar = Sidecar::new("m".into(), 8);
        // a.rs and b.rs have one identical chunk each (same seed, same i).
        sidecar.insert(
            "a.rs".into(),
            vec![StoredVector::from_f32(
                ChunkKind::Function,
                Some("alpha".into()),
                1,
                5,
                &[0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
            )],
        );
        sidecar.insert(
            "b.rs".into(),
            vec![StoredVector::from_f32(
                ChunkKind::Function,
                Some("alpha_clone".into()),
                1,
                5,
                &[0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
            )],
        );
        let out = semantic_duplicates(&sidecar, 5);
        assert!(out.iter().any(
            |t| matches!(t, SlopTarget::Area { suspect, .. } if *suspect == SlopSuspect::SingleImpl)
        ));
    }

    #[test]
    fn semantic_duplicate_within_same_file_is_ignored() {
        let mut sidecar = Sidecar::new("m".into(), 8);
        sidecar.insert(
            "a.rs".into(),
            vec![
                StoredVector::from_f32(ChunkKind::Function, Some("first".into()), 1, 5, &[0.5; 8]),
                StoredVector::from_f32(
                    ChunkKind::Function,
                    Some("second".into()),
                    6,
                    10,
                    &[0.5; 8],
                ),
            ],
        );
        let out = semantic_duplicates(&sidecar, 5);
        assert!(out.is_empty());
    }

    #[test]
    fn cosine_similarity_handles_zero_vector() {
        let a = vec![0.0; 4];
        let b = vec![1.0, 1.0, 1.0, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_identical_vectors_returns_one() {
        let a = vec![0.5, 0.5, 0.5, 0.5];
        let s = cosine_similarity(&a, &a);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_clamps_above_one() {
        // Numerically slightly > 1 from floating point; verify clamp.
        let a = vec![1.0; 4];
        let s = cosine_similarity(&a, &a);
        assert!(s <= 1.0 + 1e-6);
    }
}
