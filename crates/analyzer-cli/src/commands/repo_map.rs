use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use analyzer_core::output;
use analyzer_core::types::RepoIntelData;

#[derive(Subcommand)]
pub enum RepoMapAction {
    /// Extract AST symbols from a repository
    Generate {
        /// Repository path
        path: PathBuf,
    },
    /// Query symbols for a specific file
    Symbols {
        /// File to query symbols for
        file: String,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Find all files that depend on a symbol
    Dependents {
        /// Symbol name to search for
        symbol: String,
        /// Restrict to definitions in this file
        #[arg(long)]
        file: Option<String>,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
    },
}

pub fn run(action: RepoMapAction) -> Result<()> {
    match action {
        RepoMapAction::Generate { path } => {
            eprintln!("[INFO] Extracting symbols from {}", path.display());
            let (symbols, import_graph) =
                analyzer_repo_map::extractor::extract_symbols(&path)?;
            eprintln!("[OK] Extracted symbols from {} files", symbols.len());

            #[derive(serde::Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Output {
                files: usize,
                total_definitions: usize,
                total_exports: usize,
            }
            let total_defs: usize = symbols.values().map(|s| s.definitions.len()).sum();
            let total_exports: usize = symbols.values().map(|s| s.exports.len()).sum();
            println!(
                "{}",
                output::to_json(&Output {
                    files: symbols.len(),
                    total_definitions: total_defs,
                    total_exports,
                })
            );
            // Also report import graph stats
            eprintln!(
                "[INFO] Import graph: {} files with imports",
                import_graph.len()
            );
            Ok(())
        }
        RepoMapAction::Symbols { file, map_file } => {
            let map = load_map(&map_file)?;
            let symbols = map.symbols.as_ref();
            let import_graph = map.import_graph.as_ref();

            match (symbols, import_graph) {
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
                    eprintln!(
                        "[ERROR] No symbol data in map file. Run `repo-intel init` first."
                    );
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        RepoMapAction::Dependents {
            symbol,
            file,
            map_file,
        } => {
            let map = load_map(&map_file)?;
            match map.symbols.as_ref() {
                Some(syms) => {
                    let result = analyzer_repo_map::queries::dependents(
                        syms,
                        &symbol,
                        file.as_deref(),
                    );
                    println!("{}", output::to_json(&result));
                }
                None => {
                    eprintln!(
                        "[ERROR] No symbol data in map file. Run `repo-intel init` first."
                    );
                    std::process::exit(1);
                }
            }
            Ok(())
        }
    }
}

fn load_map(path: &PathBuf) -> Result<RepoIntelData> {
    let json = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}
