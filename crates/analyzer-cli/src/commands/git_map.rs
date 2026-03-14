use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use analyzer_core::git;
use analyzer_core::output;
use analyzer_git_map::{aggregator, extractor, incremental, queries};

#[derive(Subcommand)]
pub enum GitMapAction {
    /// Full history scan - creates a new git-map
    Init {
        /// Repository path to analyze
        path: PathBuf,
        /// Maximum number of commits to process
        #[arg(long)]
        max_commits: Option<usize>,
    },
    /// Incremental update - process new commits since last scan
    Update {
        /// Repository path to analyze
        path: PathBuf,
        /// Path to existing git-map JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Check git-map validity against the repository
    Status {
        /// Repository path
        path: PathBuf,
        /// Path to existing git-map JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Run queries against a cached git-map
    Query {
        #[command(subcommand)]
        query: QueryAction,
    },
}

#[derive(Subcommand)]
pub enum QueryAction {
    /// Show most-changed files
    Hotspots {
        /// Repository path (or path to git-map JSON)
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to git-map JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show files coupled with a given file
    Coupling {
        /// File to find coupling for
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to git-map JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show ownership for a file or directory
    Ownership {
        /// File or directory path
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to git-map JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Calculate bus factor
    BusFactor {
        /// Repository path
        path: PathBuf,
        /// Adjust for AI-assisted commits
        #[arg(long)]
        adjust_for_ai: bool,
        /// Path to git-map JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
}

pub fn run(action: GitMapAction) -> Result<()> {
    match action {
        GitMapAction::Init { path, max_commits } => run_init(&path, max_commits),
        GitMapAction::Update { path, map_file } => run_update(&path, &map_file),
        GitMapAction::Status { path, map_file } => run_status(&path, &map_file),
        GitMapAction::Query { query } => run_query(query),
    }
}

fn run_init(path: &Path, _max_commits: Option<usize>) -> Result<()> {
    eprintln!("[INFO] Scanning full history at {}", path.display());

    let delta = extractor::extract_full(path).context("failed to extract git history")?;

    eprintln!("[INFO] Processed {} commits", delta.commits.len());

    let mut map = aggregator::create_empty_map();

    let repo = git::open_repo(path)?;
    map.git.shallow = git::is_shallow(&repo);

    aggregator::merge_delta(&mut map, &delta);

    println!("{}", output::to_json(&map));
    eprintln!("[OK] Git map created successfully");
    Ok(())
}

fn run_update(path: &Path, map_file: &Path) -> Result<()> {
    let map_json = std::fs::read_to_string(map_file)
        .with_context(|| format!("failed to read map file: {}", map_file.display()))?;
    let mut map: analyzer_core::types::GitMapData =
        serde_json::from_str(&map_json).context("failed to parse map JSON")?;

    let repo = git::open_repo(path)?;

    // Check if we need a full rebuild
    if incremental::needs_rebuild(&map, &repo) {
        eprintln!("[WARN] Force push detected, performing full rebuild");
        return run_init(path, None);
    }

    let since_sha =
        incremental::get_since_sha(&map).context("map has no analyzedUpTo - use init instead")?;

    eprintln!("[INFO] Updating from {} at {}", since_sha, path.display());

    let delta = extractor::extract_delta(path, &since_sha)?;

    eprintln!("[INFO] Processed {} new commits", delta.commits.len());

    aggregator::merge_delta(&mut map, &delta);

    println!("{}", output::to_json(&map));
    eprintln!("[OK] Git map updated successfully");
    Ok(())
}

fn run_status(path: &Path, map_file: &Path) -> Result<()> {
    let map_json = std::fs::read_to_string(map_file)
        .with_context(|| format!("failed to read map file: {}", map_file.display()))?;
    let map: analyzer_core::types::GitMapData =
        serde_json::from_str(&map_json).context("failed to parse map JSON")?;

    let repo = git::open_repo(path)?;
    let status = incremental::check_status(&map, &repo);

    #[derive(serde::Serialize)]
    struct StatusOutput {
        status: String,
        analyzed_up_to: String,
        total_commits: u64,
    }

    let output = StatusOutput {
        status: serde_json::to_value(&status)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string()),
        analyzed_up_to: map.git.analyzed_up_to,
        total_commits: map.git.total_commits_analyzed,
    };

    println!("{}", analyzer_core::output::to_json(&output));
    Ok(())
}

fn run_query(query: QueryAction) -> Result<()> {
    match query {
        QueryAction::Hotspots { map_file, top, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::hotspots(&map, None, top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Coupling { file, map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::coupling(&map, &file, false);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Ownership { file, map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::ownership(&map, &file);
            println!("{}", output::to_json(&result));
        }
        QueryAction::BusFactor {
            map_file,
            adjust_for_ai,
            ..
        } => {
            let map = load_map(&map_file)?;
            let bf = queries::bus_factor(&map, adjust_for_ai);
            #[derive(serde::Serialize)]
            struct BusFactorOutput {
                bus_factor: usize,
                adjust_for_ai: bool,
            }
            let output = BusFactorOutput {
                bus_factor: bf,
                adjust_for_ai,
            };
            println!("{}", analyzer_core::output::to_json(&output));
        }
    }
    Ok(())
}

fn load_map(path: &Path) -> Result<analyzer_core::types::GitMapData> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read map file: {}", path.display()))?;
    serde_json::from_str(&json).context("failed to parse map JSON")
}
