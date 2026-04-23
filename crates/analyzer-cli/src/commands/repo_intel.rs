use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use analyzer_core::git;
use analyzer_core::output;
use analyzer_git_map::{aggregator, extractor, incremental, queries};

#[derive(Subcommand)]
pub enum RepoIntelAction {
    /// Full history scan - creates a new repo-intel map
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
        /// Path to existing repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Check repo-intel map validity against the repository
    Status {
        /// Repository path
        path: PathBuf,
        /// Path to existing repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Run queries against a cached repo-intel map
    Query {
        #[command(subcommand)]
        query: QueryAction,
    },
}

#[derive(Subcommand)]
pub enum QueryAction {
    /// Show most-changed files
    Hotspots {
        /// Repository path (or path to repo-intel JSON)
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show files coupled with a given file
    Coupling {
        /// File to find coupling for
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show ownership for a file or directory
    Ownership {
        /// File or directory path
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Calculate bus factor
    BusFactor {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show files with highest bug-fix density
    Bugspots {
        /// Repository path
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show project norms detected from git history
    Norms {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show area-level health overview
    Areas {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show least-changed files (no recent activity)
    Coldspots {
        /// Repository path
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show contributors sorted by commit count
    Contributors {
        /// Repository path
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "20")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show release cadence and tag information
    ReleaseInfo {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show repository health summary
    Health {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show history for a specific file
    FileHistory {
        /// File path to look up
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show commit message conventions
    Conventions {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show hot source files with no co-changing test file
    TestGaps {
        /// Repository path
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "10")]
        top: usize,
        /// Minimum changes to consider a file
        #[arg(long, default_value = "2")]
        min_changes: u64,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Score changed files by risk (takes comma-separated file list)
    DiffRisk {
        /// Repository path
        path: PathBuf,
        /// Comma-separated list of changed files
        #[arg(long)]
        files: String,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show doc files with low code coupling (likely stale)
    DocDrift {
        /// Repository path
        path: PathBuf,
        /// Number of results to show
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Human-readable summary for someone new to the repo
    Onboard {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Guidance for outside contributors
    CanIHelp {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show AST symbols for a specific file
    Symbols {
        /// File to query symbols for
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Find all files that depend on a symbol
    Dependents {
        /// Symbol name to search for
        symbol: String,
        /// Repository path
        path: PathBuf,
        /// Restrict to definitions in this file
        #[arg(long)]
        file: Option<String>,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show stale documentation references (symbol-level)
    StaleDocs {
        /// Repository path
        path: PathBuf,
        /// Maximum number of results
        #[arg(long, default_value = "20")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show project metadata (languages, CI, license, README)
    ProjectInfo {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show pain spots (hotspot × complexity × bug density intersection)
    Painspots {
        /// Repository path
        path: PathBuf,
        /// Maximum number of results
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// List communities discovered by co-change graph Louvain partitioning
    Communities {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show files bridging communities (high betweenness centrality)
    Boundaries {
        /// Repository path
        path: PathBuf,
        /// Maximum number of results
        #[arg(long, default_value = "10")]
        top: usize,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Look up which community a file belongs to
    AreaOf {
        /// File path (relative to repo root)
        file: String,
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Show composite health metrics for one community by id
    CommunityHealth {
        /// Community id (from `communities` query)
        id: u32,
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// List every place execution can start (binaries, main fns, npm scripts)
    EntryPoints {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file (optional - enables AST main detection)
        #[arg(long)]
        map_file: Option<PathBuf>,
    },
}

pub fn run(action: RepoIntelAction) -> Result<()> {
    match action {
        RepoIntelAction::Init { path, max_commits } => run_init(&path, max_commits),
        RepoIntelAction::Update { path, map_file } => run_update(&path, &map_file),
        RepoIntelAction::Status { path, map_file } => run_status(&path, &map_file),
        RepoIntelAction::Query { query } => run_query(query),
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

    // Phase 2: AST symbol extraction
    eprintln!("[INFO] Extracting AST symbols...");
    match analyzer_repo_map::extractor::extract_symbols(path) {
        Ok((symbols, import_graph)) => {
            eprintln!("[INFO] Extracted symbols from {} files", symbols.len());

            // Detect naming conventions and test patterns
            let naming = analyzer_repo_map::conventions::detect_naming(&symbols);
            let test_patterns =
                analyzer_repo_map::conventions::detect_test_patterns(path, &symbols);
            map.conventions.naming_patterns = Some(naming);
            map.conventions.test_patterns = Some(test_patterns);

            map.symbols = Some(symbols);
            map.import_graph = Some(import_graph);
        }
        Err(e) => eprintln!("[WARN] AST extraction failed: {e}"),
    }

    // Phase 3: Project metadata
    eprintln!("[INFO] Collecting project metadata...");
    match analyzer_collectors::collect_metadata(path) {
        Ok(metadata) => {
            map.project = Some(metadata);
        }
        Err(e) => eprintln!("[WARN] Metadata collection failed: {e}"),
    }

    // Phase 4: Doc-code cross-references (requires Phase 2 symbols)
    if let Some(ref symbols) = map.symbols {
        eprintln!("[INFO] Checking doc-code references...");
        match analyzer_sync_check::queries::build_doc_refs(path, &map, symbols) {
            Ok(doc_refs) => {
                eprintln!("[INFO] Checked {} doc files", doc_refs.len());
                map.doc_refs = Some(doc_refs);
            }
            Err(e) => eprintln!("[WARN] Doc-code sync check failed: {e}"),
        }
    }

    // Phase 5: Graph-derived analytics (co-change communities + centrality)
    eprintln!("[INFO] Building co-change graph...");
    analyzer_graph::finalize(&mut map);
    if let Some(cg) = map.graph.as_ref().and_then(|g| g.cochange.as_ref()) {
        eprintln!(
            "[INFO] Discovered {} communities from {} edges",
            cg.communities.len(),
            cg.edges.len()
        );
    } else {
        eprintln!("[INFO] Insufficient co-change signal - graph not built");
    }

    println!("{}", output::to_json(&map));
    eprintln!("[OK] Repo intel map created successfully");
    Ok(())
}

fn run_update(path: &Path, map_file: &Path) -> Result<()> {
    let map_json = std::fs::read_to_string(map_file)
        .with_context(|| format!("failed to read map file: {}", map_file.display()))?;
    let mut map: analyzer_core::types::RepoIntelData =
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

    // Re-run AST extraction on update (re-parses changed files only would be ideal,
    // but for now we do a full rescan since it's fast enough)
    eprintln!("[INFO] Refreshing AST symbols...");
    match analyzer_repo_map::extractor::extract_symbols(path) {
        Ok((symbols, import_graph)) => {
            let naming = analyzer_repo_map::conventions::detect_naming(&symbols);
            let test_patterns =
                analyzer_repo_map::conventions::detect_test_patterns(path, &symbols);
            map.conventions.naming_patterns = Some(naming);
            map.conventions.test_patterns = Some(test_patterns);
            map.symbols = Some(symbols);
            map.import_graph = Some(import_graph);
        }
        Err(e) => eprintln!("[WARN] AST extraction failed: {e}"),
    }

    // Refresh project metadata
    match analyzer_collectors::collect_metadata(path) {
        Ok(metadata) => map.project = Some(metadata),
        Err(e) => eprintln!("[WARN] Metadata collection failed: {e}"),
    }

    // Refresh doc-code references
    if let Some(ref symbols) = map.symbols {
        match analyzer_sync_check::queries::build_doc_refs(path, &map, symbols) {
            Ok(doc_refs) => map.doc_refs = Some(doc_refs),
            Err(e) => eprintln!("[WARN] Doc-code sync check failed: {e}"),
        }
    }

    // Refresh Phase 5 graph analytics. Full re-cluster on update for now -
    // incremental Louvain on dirty subgraphs is a future optimisation.
    eprintln!("[INFO] Rebuilding co-change graph...");
    analyzer_graph::finalize(&mut map);

    println!("{}", output::to_json(&map));
    eprintln!("[OK] Repo intel map updated successfully");
    Ok(())
}

fn run_status(path: &Path, map_file: &Path) -> Result<()> {
    let map_json = std::fs::read_to_string(map_file)
        .with_context(|| format!("failed to read map file: {}", map_file.display()))?;
    let map: analyzer_core::types::RepoIntelData =
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
            let result = queries::coupling(&map, &file);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Ownership { file, map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::ownership(&map, &file);
            println!("{}", output::to_json(&result));
        }
        QueryAction::BusFactor { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::bus_factor_detailed(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Bugspots { map_file, top, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::bugspots(&map, top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Norms { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::norms(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Areas { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::areas(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Coldspots { map_file, top, .. } => {
            let map = load_map(&map_file)?;
            let mut result = queries::coldspots(&map, None);
            result.truncate(top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Contributors { map_file, top, .. } => {
            let map = load_map(&map_file)?;
            let mut result = queries::contributors(&map, None);
            result.truncate(top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::ReleaseInfo { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::release_info(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Health { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::health(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::FileHistory { file, map_file, .. } => {
            let map = load_map(&map_file)?;
            match queries::file_history(&map, &file) {
                Some(activity) => println!("{}", output::to_json(activity)),
                None => {
                    eprintln!("[WARN] No history found for {}", file);
                    println!("null");
                }
            }
        }
        QueryAction::Conventions { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::conventions(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::TestGaps {
            map_file,
            top,
            min_changes,
            ..
        } => {
            let map = load_map(&map_file)?;
            let result = queries::test_gaps(&map, min_changes, top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::DiffRisk {
            map_file, files, ..
        } => {
            let map = load_map(&map_file)?;
            let file_list: Vec<String> = files.split(',').map(|s| s.trim().to_string()).collect();
            let result = queries::diff_risk(&map, &file_list);
            println!("{}", output::to_json(&result));
        }
        QueryAction::DocDrift { map_file, top, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::doc_drift(&map, top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Onboard { path, map_file } => {
            let map = load_map(&map_file)?;
            let result = queries::onboard(&map, Some(&path));
            println!("{}", output::to_json(&result));
        }
        QueryAction::CanIHelp { path, map_file } => {
            let map = load_map(&map_file)?;
            let result = queries::can_i_help(&map, Some(&path));
            println!("{}", output::to_json(&result));
        }
        QueryAction::Symbols { file, map_file, .. } => {
            let map = load_map(&map_file)?;
            match (map.symbols.as_ref(), map.import_graph.as_ref()) {
                (Some(syms), Some(graph)) => {
                    match analyzer_repo_map::queries::symbols(syms, graph, &file) {
                        Some(result) => println!("{}", output::to_json(&result)),
                        None => {
                            eprintln!("[WARN] No symbols found for {}", file);
                            println!("null");
                        }
                    }
                }
                _ => {
                    eprintln!("[WARN] No symbol data in map. Run repo-intel init to generate.");
                    println!("null");
                }
            }
        }
        QueryAction::Dependents {
            symbol,
            file,
            map_file,
            ..
        } => {
            let map = load_map(&map_file)?;
            match map.symbols.as_ref() {
                Some(syms) => {
                    let result =
                        analyzer_repo_map::queries::dependents(syms, &symbol, file.as_deref());
                    println!("{}", output::to_json(&result));
                }
                None => {
                    eprintln!("[WARN] No symbol data in map. Run repo-intel init to generate.");
                    println!("null");
                }
            }
        }
        QueryAction::StaleDocs {
            path,
            top,
            map_file,
        } => {
            let map = load_map(&map_file)?;
            match map.symbols.as_ref() {
                Some(syms) => {
                    let result = analyzer_sync_check::queries::stale_docs(&path, &map, syms, top)?;
                    println!("{}", output::to_json(&result));
                }
                None => {
                    eprintln!("[WARN] No symbol data in map. Run repo-intel init to generate.");
                    println!("[]");
                }
            }
        }
        QueryAction::ProjectInfo { map_file, .. } => {
            let map = load_map(&map_file)?;
            match map.project {
                Some(project) => println!("{}", output::to_json(&project)),
                None => {
                    eprintln!(
                        "[WARN] No project metadata in map. Run repo-intel init to generate."
                    );
                    println!("null");
                }
            }
        }
        QueryAction::Painspots { top, map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = queries::painspots(&map, top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Communities { map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = analyzer_graph::queries::communities(&map);
            println!("{}", output::to_json(&result));
        }
        QueryAction::Boundaries { top, map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = analyzer_graph::queries::boundaries(&map, top);
            println!("{}", output::to_json(&result));
        }
        QueryAction::AreaOf { file, map_file, .. } => {
            let map = load_map(&map_file)?;
            let result = analyzer_graph::queries::area_of(&map, &file);
            println!("{}", output::to_json(&result));
        }
        QueryAction::CommunityHealth { id, map_file, .. } => {
            let map = load_map(&map_file)?;
            match analyzer_graph::queries::community_health(&map, id) {
                Some(health) => println!("{}", output::to_json(&health)),
                None => {
                    eprintln!(
                        "[WARN] Community id {id} not found (run `repo-intel query communities` to list ids)"
                    );
                    println!("null");
                }
            }
        }
        QueryAction::EntryPoints { path, map_file } => {
            // The symbol index is optional - it adds AST-derived `main`
            // functions to the manifest-derived results. Without it the
            // query still returns Cargo/npm/pyproject entries.
            let symbols = match map_file.as_ref() {
                Some(mf) => load_map(mf)?.symbols,
                None => None,
            };
            let result = analyzer_collectors::entry_points::detect(&path, symbols.as_ref());
            println!("{}", output::to_json(&result));
        }
    }
    Ok(())
}

fn load_map(path: &Path) -> Result<analyzer_core::types::RepoIntelData> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read map file: {}", path.display()))?;
    serde_json::from_str(&json).context("failed to parse map JSON")
}
