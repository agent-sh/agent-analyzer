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
        // Make ONNX Runtime loadable BEFORE fastembed touches `ort`. With the
        // `ort-load-dynamic` feature, a missing/unloadable libonnxruntime sends
        // fastembed's lazy init into a futex deadlock with no output (observed
        // on machines without a system ORT). Resolve a dylib, export
        // ORT_DYLIB_PATH for `ort` to consume, and fail fast with guidance if
        // none loads.
        resolve_and_preflight_ort()?;

        let cache_dir = resolve_cache_dir();

        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            eprintln!(
                "analyzer-embed: warning: could not create cache dir {}: {e}",
                cache_dir.display()
            );
        }

        prune_poisoned_cache(&cache_dir, &model_code_for(map_variant(variant))?);

        let mut opts = InitOptions::new(map_variant(variant)).with_show_download_progress(true);
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

/// HuggingFace model code for a fastembed model. Fastembed uses this to
/// form the on-disk cache dir: `models--<org>--<name>` where `/` in the
/// code becomes `--`.
///
/// We delegate to `TextEmbedding::get_model_info`, which is the stable
/// accessor into fastembed's own model registry. This matters because
/// `EmbeddingModel` is `#[non_exhaustive]` upstream: a hardcoded `match`
/// with a catch-all `_ => ""` branch would silently break cache-layout
/// logic (e.g., `prune_poisoned_cache`) if fastembed added a variant we
/// later started using - the catch-all would return the empty string and
/// the poisoned-cache detector would no-op. Using the upstream lookup
/// forwards the error instead so breakage is loud.
fn model_code_for(model: EmbeddingModel) -> Result<String> {
    TextEmbedding::get_model_info(&model)
        .map(|info| info.model_code.clone())
        .with_context(|| format!("fastembed has no ModelInfo for {model:?}"))
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

/// Platform-specific ONNX Runtime shared-library file name.
fn ort_dylib_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
}

/// Ensure ONNX Runtime is loadable before fastembed initializes `ort`.
///
/// `ort-load-dynamic` defers ORT loading until first use and, on failure to
/// find the dylib, fastembed's init wedges in a futex with no diagnostic. We
/// pre-empt that: pick a dylib, verify it actually `dlopen`s, and publish it
/// via `ORT_DYLIB_PATH` (which `ort` honors). Resolution order:
///
///   1. `ORT_DYLIB_PATH` if already set (respect the operator's choice; still
///      preflighted so a bad path is reported, not hung on).
///   2. `libonnxruntime.{so,dylib,dll}` next to the running executable - the
///      release tarball bundles it beside `agent-analyzer-embed`.
///   3. System default: let the loader search (LD_LIBRARY_PATH, standard dirs)
///      by dlopen-ing the bare library name.
///
/// Returns a clear, actionable error if nothing loads - never a silent hang.
///
/// Runs at most once per process. `FastEmbedder::new` may be called more than
/// once (multiple variants) and potentially from multiple threads; the actual
/// resolve - which calls `std::env::set_var`, a data race if run concurrently -
/// is guarded by a `OnceLock` so it executes exactly once and the result is
/// cached.
fn resolve_and_preflight_ort() -> Result<()> {
    static ORT_INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ORT_INIT
        .get_or_init(|| resolve_and_preflight_ort_inner().map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

fn resolve_and_preflight_ort_inner() -> Result<()> {
    // Candidate paths in precedence order. `None` => bare name (system search).
    let mut candidates: Vec<Option<PathBuf>> = Vec::new();

    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        if !p.is_empty() {
            // `ort` accepts ORT_DYLIB_PATH as either the dylib file or a
            // directory containing it. dlopen needs the file, so normalize a
            // directory to <dir>/<libname> before probing.
            let pb = PathBuf::from(&p);
            candidates.push(Some(if pb.is_dir() {
                pb.join(ort_dylib_name())
            } else {
                pb
            }));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(Some(dir.join(ort_dylib_name())));
        }
    }
    candidates.push(None); // system default search

    let mut tried: Vec<String> = Vec::new();
    for cand in &candidates {
        let load_target: PathBuf = match cand {
            Some(p) => {
                if !p.exists() {
                    tried.push(format!("{} (not found)", p.display()));
                    continue;
                }
                p.clone()
            }
            None => PathBuf::from(ort_dylib_name()),
        };

        match try_dlopen(&load_target) {
            Ok(()) => {
                // Only export an explicit path for real files; for the system
                // search case leave ORT_DYLIB_PATH unset so `ort` does its own
                // default resolution (matching what just succeeded here).
                if cand.is_some() {
                    // SAFETY: serialized by the OnceLock in the public wrapper -
                    // this inner fn runs exactly once per process, before any
                    // embedding threads spawn, so the set_var has no concurrent
                    // reader/writer.
                    unsafe {
                        std::env::set_var("ORT_DYLIB_PATH", &load_target);
                    }
                }
                return Ok(());
            }
            Err(e) => {
                tried.push(format!("{}: {e}", load_target.display()));
            }
        }
    }

    anyhow::bail!(
        "could not load ONNX Runtime ({lib}). The embedder needs ONNX Runtime \
         at runtime. Fixes:\n  \
         - reinstall the embed binary so the bundled library is restored \
         (it ships in the agent-analyzer-embed release tarball), or\n  \
         - install ONNX Runtime and set ORT_DYLIB_PATH to its \
         {lib}.\nTried:\n  {tried}",
        lib = ort_dylib_name(),
        tried = tried.join("\n  ")
    );
}

/// Attempt to `dlopen` a shared library and immediately release it.
///
/// Returns `Ok(())` if the library loads. Loading ORT runs its initializers;
/// this is the same load `ort` performs internally - we only do it first to
/// surface failures as a clean error rather than a downstream deadlock. The
/// handle is dropped right away; `ort` loads its own copy via `ORT_DYLIB_PATH`.
fn try_dlopen(lib: &Path) -> Result<(), String> {
    // SAFETY: dlopen of a shared library by path. Standard dynamic-loading;
    // any unsafe initializer the library runs is the same one `ort` would run.
    match unsafe { libloading::Library::new(lib) } {
        Ok(_handle) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
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
    fn model_code_for_supported_variants_succeeds() {
        // Both variants we care about must resolve through fastembed's
        // ModelInfo registry. If fastembed ever drops one of these the
        // test fires before users hit a runtime error.
        let small = model_code_for(map_variant(ModelVariant::Small)).unwrap();
        assert_eq!(small, "Qdrant/bge-small-en-v1.5-onnx-Q");
        let big = model_code_for(map_variant(ModelVariant::Big)).unwrap();
        assert_eq!(big, "onnx-community/embeddinggemma-300m-ONNX");
    }

    #[test]
    fn ort_dylib_name_matches_platform() {
        let name = ort_dylib_name();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "onnxruntime.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libonnxruntime.dylib");
        } else {
            assert_eq!(name, "libonnxruntime.so");
        }
    }

    #[test]
    fn try_dlopen_rejects_a_non_library_file() {
        // The preflight's load primitive must report failure (so the resolver
        // can move to the next candidate / bail with guidance) rather than
        // succeed or hang on a file that is not a real shared object. Host-
        // independent: a text file never dlopens anywhere.
        let tmp = tempdir().unwrap();
        let bogus = tmp.path().join(ort_dylib_name());
        std::fs::write(&bogus, b"not a real shared object").unwrap();

        let err = try_dlopen(&bogus).expect_err("a text file must not dlopen");
        assert!(!err.is_empty(), "dlopen failure should carry a message");
    }

    #[test]
    fn try_dlopen_reports_missing_file() {
        // A path that does not exist must error, not panic.
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join(ort_dylib_name());
        assert!(try_dlopen(&missing).is_err());
    }

    #[test]
    fn try_dlopen_rejects_a_directory() {
        // `ort` accepts ORT_DYLIB_PATH as a directory, but dlopen needs the
        // file. The resolver normalizes dir -> dir/<libname> before probing;
        // this locks in that dlopen-ing a directory itself fails (so the
        // normalization is load-bearing, not cosmetic).
        let tmp = tempdir().unwrap();
        assert!(
            try_dlopen(tmp.path()).is_err(),
            "dlopen of a directory must fail"
        );
    }

    #[test]
    fn preflight_runs_once_and_caches() {
        // The OnceLock wrapper must return a stable result across calls (no
        // re-resolve, no repeated set_var). We can't assert success without a
        // real ORT on the host, but we can assert idempotence: two calls agree.
        let first = resolve_and_preflight_ort().is_ok();
        let second = resolve_and_preflight_ort().is_ok();
        assert_eq!(first, second, "preflight result must be stable/cached");
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
