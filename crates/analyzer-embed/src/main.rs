//! `agent-analyzer-embed` binary.
//!
//! Two subcommands:
//!
//! - `scan` — full re-embed of all eligible files, JSON to stdout.
//! - `update` — delta-only: re-embed only changed/added files, drop
//!   removed entries, JSON to stdout.
//!
//! The JSON document is intended to be piped to
//! `agent-analyzer set-embeddings --input -` which merges it into the
//! sidecar file alongside `repo-intel.json`.

use std::path::PathBuf;
use std::process::ExitCode;

use analyzer_embed::chunk::Granularity;
use analyzer_embed::embedder::ModelVariant;
use analyzer_embed::model::FastEmbedder;
use analyzer_embed::scan::{ScanOptions, run_scan, run_update};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "agent-analyzer-embed",
    version,
    about = "Local embedding generation for agent-analyzer"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Embed every eligible file in the repo. Emits a JSON document to
    /// stdout; pipe to `agent-analyzer set-embeddings --input -`.
    Scan {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,

        /// Which model variant is installed. Must match the model file
        /// available next to this binary (or in the fastembed cache).
        #[arg(long, value_enum)]
        variant: VariantArg,

        /// Chunking + dim preset. Picked at install time by the skill.
        #[arg(long, value_enum, default_value = "balanced")]
        detail: DetailArg,

        /// Cap on total files visited.
        #[arg(long, default_value_t = 500)]
        max_files: usize,
    },

    /// Re-embed only files whose content hash differs from the existing
    /// sidecar. Drops entries for removed files. Falls back to a full
    /// scan if no sidecar exists yet.
    Update {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,

        /// Path to the existing JSON artifact (`repo-intel.json`). The
        /// sidecar is found at the same path with `.embeddings.bin`
        /// substituted.
        #[arg(long)]
        map_file: PathBuf,

        #[arg(long, value_enum)]
        variant: VariantArg,

        #[arg(long, value_enum, default_value = "balanced")]
        detail: DetailArg,

        #[arg(long, default_value_t = 500)]
        max_files: usize,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum VariantArg {
    Small,
    Big,
}

impl From<VariantArg> for ModelVariant {
    fn from(v: VariantArg) -> Self {
        match v {
            VariantArg::Small => ModelVariant::Small,
            VariantArg::Big => ModelVariant::Big,
        }
    }
}

#[derive(Copy, Clone, ValueEnum)]
enum DetailArg {
    /// per-file × 128 dim
    Compact,
    /// per-function × 256 dim (recommended)
    Balanced,
    /// per-function × 768 dim (best recall)
    Maximum,
}

impl DetailArg {
    fn granularity(self) -> Granularity {
        match self {
            DetailArg::Compact => Granularity::PerFile,
            DetailArg::Balanced | DetailArg::Maximum => Granularity::PerFunction,
        }
    }

    fn dim(self) -> usize {
        match self {
            DetailArg::Compact => 128,
            DetailArg::Balanced => 256,
            DetailArg::Maximum => 768,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Scan {
            repo,
            variant,
            detail,
            max_files,
        } => execute_scan(repo, variant.into(), detail, max_files),
        Command::Update {
            repo,
            map_file,
            variant,
            detail,
            max_files,
        } => execute_update(repo, map_file, variant.into(), detail, max_files),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ERROR] {e:#}");
            ExitCode::from(1)
        }
    }
}

fn execute_scan(
    repo: PathBuf,
    variant: ModelVariant,
    detail: DetailArg,
    max_files: usize,
) -> anyhow::Result<()> {
    let mut embedder = FastEmbedder::new(variant)?;
    let opts = ScanOptions {
        repo,
        granularity: detail.granularity(),
        dim: detail.dim(),
        max_files,
    };
    let doc = run_scan(&mut embedder, &opts)?;
    serde_json::to_writer(std::io::stdout(), &doc)?;
    Ok(())
}

fn execute_update(
    repo: PathBuf,
    map_file: PathBuf,
    variant: ModelVariant,
    detail: DetailArg,
    max_files: usize,
) -> anyhow::Result<()> {
    let sidecar_path = derive_sidecar_path(&map_file);
    let mut embedder = FastEmbedder::new(variant)?;
    let opts = ScanOptions {
        repo,
        granularity: detail.granularity(),
        dim: detail.dim(),
        max_files,
    };
    let doc = run_update(&mut embedder, &opts, &sidecar_path)?;
    serde_json::to_writer(std::io::stdout(), &doc)?;
    Ok(())
}

fn derive_sidecar_path(map_file: &std::path::Path) -> PathBuf {
    // repo-intel.json -> repo-intel.embeddings.bin
    if let Some(stem) = map_file.file_stem().and_then(|s| s.to_str()) {
        if let Some(parent) = map_file.parent() {
            return parent.join(format!("{stem}.embeddings.bin"));
        }
    }
    map_file.with_extension("embeddings.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_compact_maps_to_per_file_128() {
        assert_eq!(DetailArg::Compact.granularity(), Granularity::PerFile);
        assert_eq!(DetailArg::Compact.dim(), 128);
    }

    #[test]
    fn detail_balanced_maps_to_per_function_256() {
        assert_eq!(DetailArg::Balanced.granularity(), Granularity::PerFunction);
        assert_eq!(DetailArg::Balanced.dim(), 256);
    }

    #[test]
    fn detail_maximum_maps_to_per_function_768() {
        assert_eq!(DetailArg::Maximum.granularity(), Granularity::PerFunction);
        assert_eq!(DetailArg::Maximum.dim(), 768);
    }

    #[test]
    fn sidecar_path_substitutes_extension() {
        let p = std::path::PathBuf::from("/x/y/repo-intel.json");
        let sc = derive_sidecar_path(&p);
        assert_eq!(sc, std::path::PathBuf::from("/x/y/repo-intel.embeddings.bin"));
    }
}
