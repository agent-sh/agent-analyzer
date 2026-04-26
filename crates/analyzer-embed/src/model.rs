//! Concrete [`Embedder`] implementation backed by fastembed-rs.
//!
//! `fastembed` wraps ONNX Runtime + HuggingFace tokenizers + model
//! download / cache. We pick from its `EmbeddingModel` enum based on
//! [`ModelVariant`] and pass through the embed call.
//!
//! # Cache location
//!
//! By default fastembed writes to `./.fastembed_cache` relative to the
//! current working directory. That is wrong for us: every user repo
//! would download its own 66 MB+ copy and pollute the user's working
//! tree. Worse, if the process is killed mid-download fastembed's
//! cache layout ends up half-built (`refs/main` written, `snapshots/`
//! still empty) and subsequent runs hang inside `TextEmbedding::try_new`
//! with no log output.
//!
//! We resolve a stable cache directory in this precedence:
//!
//! 1. `FASTEMBED_CACHE_DIR` env var if set (fastembed already respects
//!    this; we mirror the precedence explicitly so our error messages
//!    can name the directory).
//! 2. `~/.agent-sh/cache/fastembed` (matches where the binary lives in
//!    `~/.agent-sh/bin/`).
//! 3. Fastembed's built-in default as a last resort, if no home dir can
//!    be resolved.
//!
//! We also best-effort detect and remove a "poisoned" cache layout
//! before calling into fastembed, so an interrupted first run doesn't
//! strand the user.

use std::path::{Path, PathBuf};

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
        let fastembed_model = map_variant(variant);
        let cache_dir = resolve_cache_dir();

        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            eprintln!(
                "analyzer-embed: warning: could not create cache dir {}: {e}",
                cache_dir.display()
            );
        }

        prune_poisoned_cache(&cache_dir, model_code_for(&fastembed_model));

        let mut opts = InitOptions::new(fastembed_model.clone()).with_show_download_progress(true);
        opts = opts.with_cache_dir(cache_dir.clone());

        let model = TextEmbedding::try_new(opts).with_context(|| {
            format!(
                "initialize fastembed model for variant {variant:?} (cache dir: {}). \
                 If the download appears to hang, check network/proxy settings or set \
                 FASTEMBED_CACHE_DIR to an alternative writable path.",
                cache_dir.display()
            )
        })?;
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

/// HuggingFace model code for a fastembed model. This is the string
/// fastembed uses to form the on-disk cache dir: `models--<org>--<name>`
/// where `/` in the code becomes `--`. Hardcoding the two variants we
/// support avoids a runtime lookup and keeps the cache layout logic
/// independent of fastembed's private model registry.
fn model_code_for(model: &EmbeddingModel) -> &'static str {
    match model {
        EmbeddingModel::BGESmallENV15Q => "Qdrant/bge-small-en-v1.5-onnx-Q",
        EmbeddingModel::EmbeddingGemma300M => "onnx-community/embeddinggemma-300m-ONNX",
        _ => "",
    }
}

/// Resolve the cache directory per the precedence documented on
/// [`FastEmbedder`]: env var, then `~/.agent-sh/cache/fastembed`, then
/// fastembed's default.
fn resolve_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FASTEMBED_CACHE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = home_dir() {
        return home.join(".agent-sh").join("cache").join("fastembed");
    }
    // Last resort: fastembed's built-in default, `./.fastembed_cache`
    // relative to CWD. Better than nothing, but prints a warning so
    // users know their cache is ephemeral.
    eprintln!(
        "analyzer-embed: warning: could not resolve a home directory; \
         falling back to CWD-relative .fastembed_cache. Set \
         FASTEMBED_CACHE_DIR to control the cache location."
    );
    PathBuf::from(".fastembed_cache")
}

fn home_dir() -> Option<PathBuf> {
    // Platform-appropriate home lookup without pulling in the `dirs`
    // crate. USERPROFILE is Windows; HOME covers macOS/Linux.
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(p) = std::env::var("HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

/// Best-effort detection of a half-built model cache. Fastembed creates
/// `models--<org>--<name>/refs/main` early in the download; if the
/// process is killed before it finalizes, `snapshots/` stays empty and
/// the next run hangs. Remove such a directory so fastembed re-downloads
/// cleanly.
///
/// This is best-effort: any IO error is logged to stderr and swallowed.
fn prune_poisoned_cache(cache_dir: &Path, model_code: &str) {
    if model_code.is_empty() {
        return;
    }
    let dir_name = format!("models--{}", model_code.replace('/', "--"));
    let model_dir = cache_dir.join(&dir_name);
    if !model_dir.is_dir() {
        return;
    }
    let refs_main = model_dir.join("refs").join("main");
    let snapshots = model_dir.join("snapshots");
    let has_refs = refs_main.is_file();
    let has_snapshot = snapshots
        .read_dir()
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);

    if has_refs && !has_snapshot {
        eprintln!(
            "analyzer-embed: detected poisoned fastembed cache at {}; \
             removing so the model can be re-downloaded",
            model_dir.display()
        );
        if let Err(e) = std::fs::remove_dir_all(&model_dir) {
            eprintln!(
                "analyzer-embed: warning: failed to remove poisoned cache {}: {e}",
                model_dir.display()
            );
        }
        // Also drop any stale `.lock` file next to it, which fastembed
        // uses to serialize downloads. A leftover lock after a crash
        // will not block us (fastembed uses advisory locks) but it is
        // visual noise.
        let lock = cache_dir.join(format!("{dir_name}.lock"));
        if lock.exists() {
            let _ = std::fs::remove_file(lock);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Tests that manipulate process-wide env vars must not run
    // concurrently. Rust's default test harness runs tests in parallel
    // per-binary, so we serialize with a mutex.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    #[test]
    fn prune_removes_poisoned_layout() {
        let tmp = tempdir().unwrap();
        let code = "Qdrant/bge-small-en-v1.5-onnx-Q";
        let dir_name = "models--Qdrant--bge-small-en-v1.5-onnx-Q";
        let model_dir = tmp.path().join(dir_name);
        std::fs::create_dir_all(model_dir.join("refs")).unwrap();
        std::fs::create_dir_all(model_dir.join("snapshots")).unwrap();
        std::fs::create_dir_all(model_dir.join("blobs")).unwrap();
        std::fs::write(model_dir.join("refs").join("main"), b"deadbeef").unwrap();
        // Drop a fake blob to mimic a partial download.
        std::fs::write(model_dir.join("blobs").join("abc"), b"x").unwrap();

        prune_poisoned_cache(tmp.path(), code);

        assert!(
            !model_dir.exists(),
            "poisoned model dir should have been removed"
        );
    }

    #[test]
    fn prune_leaves_healthy_layout_alone() {
        let tmp = tempdir().unwrap();
        let code = "Qdrant/bge-small-en-v1.5-onnx-Q";
        let dir_name = "models--Qdrant--bge-small-en-v1.5-onnx-Q";
        let model_dir = tmp.path().join(dir_name);
        let snapshot = model_dir.join("snapshots").join("deadbeef");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::create_dir_all(model_dir.join("refs")).unwrap();
        std::fs::write(model_dir.join("refs").join("main"), b"deadbeef").unwrap();
        std::fs::write(snapshot.join("model.onnx"), b"fake-onnx").unwrap();

        prune_poisoned_cache(tmp.path(), code);

        assert!(model_dir.exists(), "healthy cache should be preserved");
        assert!(snapshot.join("model.onnx").exists());
    }

    #[test]
    fn prune_is_noop_when_model_dir_missing() {
        let tmp = tempdir().unwrap();
        // Should not panic, should not create anything.
        prune_poisoned_cache(tmp.path(), "Qdrant/bge-small-en-v1.5-onnx-Q");
        assert!(tmp.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn resolve_cache_dir_prefers_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        let expected = tmp.path().join("custom-cache");
        // SAFETY: guarded by ENV_LOCK; std::env::set_var is unsafe in
        // edition 2024 but fine here because tests in this binary are
        // serialized via the mutex.
        unsafe {
            std::env::set_var("FASTEMBED_CACHE_DIR", &expected);
        }
        let got = resolve_cache_dir();
        unsafe {
            std::env::remove_var("FASTEMBED_CACHE_DIR");
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn resolve_cache_dir_falls_back_to_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        let fake_home = tmp.path().to_path_buf();

        // Save + clear env we care about.
        let saved_fec = std::env::var_os("FASTEMBED_CACHE_DIR");
        let saved_home = std::env::var_os("HOME");
        let saved_userprofile = std::env::var_os("USERPROFILE");

        unsafe {
            std::env::remove_var("FASTEMBED_CACHE_DIR");
            // Set both so the lookup succeeds on all platforms.
            std::env::set_var("HOME", &fake_home);
            std::env::set_var("USERPROFILE", &fake_home);
        }

        let got = resolve_cache_dir();

        unsafe {
            match saved_fec {
                Some(v) => std::env::set_var("FASTEMBED_CACHE_DIR", v),
                None => std::env::remove_var("FASTEMBED_CACHE_DIR"),
            }
            match saved_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match saved_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }

        assert_eq!(
            got,
            fake_home.join(".agent-sh").join("cache").join("fastembed")
        );
    }

    // Integration tests that actually download + run the model live in
    // tests/integration_embed.rs, gated behind --ignored so CI doesn't
    // pull ~225 MB of model files on every PR. Run locally with:
    //
    //   cargo test -p analyzer-embed -- --ignored
    //
    // Those tests now write to `~/.agent-sh/cache/fastembed` (or
    // `$FASTEMBED_CACHE_DIR` if set) instead of `./.fastembed_cache`.
}
