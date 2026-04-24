//! `agent-analyzer-embed` binary.
//!
//! Two subcommands:
//!
//! - `scan` — full re-embed of all eligible files, JSON to stdout.
//! - `update` — delta-only: read existing sidecar, hash files, re-embed
//!   only changed/added, drop removed, JSON to stdout.
//!
//! The JSON document is intended to be piped to
//! `agent-analyzer set-embeddings --input -` which merges it into the
//! sidecar file alongside `repo-intel.json`.
//!
//! This entry point currently parses arguments and validates inputs.
//! Actual embedding inference (loading the ONNX model, running tokenizer,
//! producing vectors) lands in a follow-up PR — the [`Embedder`] trait in
//! `analyzer_embed::embedder` is the boundary.

use std::path::PathBuf;
use std::process::ExitCode;

use analyzer_embed::chunk::Granularity;
use analyzer_embed::embedder::ModelVariant;
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
        /// available next to this binary.
        #[arg(long, value_enum)]
        variant: VariantArg,

        /// Chunking granularity. Picked at install time by the skill.
        #[arg(long, value_enum, default_value = "balanced")]
        detail: DetailArg,
    },

    /// Re-embed only files whose content hash differs from the existing
    /// sidecar. Drops entries for removed files. The sidecar must already
    /// exist (created by a prior `scan`).
    Update {
        /// Repository root.
        #[arg(default_value = ".")]
        repo: PathBuf,

        /// Path to the existing JSON artifact (`repo-intel.json`). The
        /// sidecar is found at the same path with `.embeddings.bin`
        /// appended.
        #[arg(long)]
        map_file: PathBuf,

        #[arg(long, value_enum)]
        variant: VariantArg,

        #[arg(long, value_enum, default_value = "balanced")]
        detail: DetailArg,
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
    // Wired into run_scan/run_update once the embedder layer lands.
    #[allow(dead_code)]
    fn granularity(self) -> Granularity {
        match self {
            DetailArg::Compact => Granularity::PerFile,
            DetailArg::Balanced | DetailArg::Maximum => Granularity::PerFunction,
        }
    }

    #[allow(dead_code)]
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
    match cli.command {
        Command::Scan {
            repo,
            variant,
            detail,
        } => run_scan(repo, variant.into(), detail),
        Command::Update {
            repo,
            map_file,
            variant,
            detail,
        } => run_update(repo, map_file, variant.into(), detail),
    }
}

fn run_scan(_repo: PathBuf, _variant: ModelVariant, _detail: DetailArg) -> ExitCode {
    // The embedder layer lands in the follow-up PR. This binary currently
    // validates arguments and exits with a clear message rather than
    // emitting a fake JSON document — better for downstream callers to see
    // an obvious not-implemented error than partial output that looks real.
    eprintln!(
        "[NOT IMPLEMENTED] embed scan: chunking + sidecar plumbing landed; \
         model loading + ONNX inference ship in the next PR."
    );
    ExitCode::from(2)
}

fn run_update(
    _repo: PathBuf,
    _map_file: PathBuf,
    _variant: ModelVariant,
    _detail: DetailArg,
) -> ExitCode {
    eprintln!(
        "[NOT IMPLEMENTED] embed update: chunking + sidecar plumbing landed; \
         model loading + ONNX inference ship in the next PR."
    );
    ExitCode::from(2)
}
