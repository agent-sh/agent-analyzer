use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;

use analyzer_core::output;
use analyzer_core::types::RepoIntelData;

#[derive(Subcommand)]
pub enum SyncCheckAction {
    /// Check documentation for stale code references
    Check {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file (for rename/deletion/hotspot data)
        #[arg(long)]
        map_file: PathBuf,
    },
    /// Query stale docs from cached data
    StaleDocs {
        /// Repository path
        path: PathBuf,
        /// Path to repo-intel JSON file
        #[arg(long)]
        map_file: PathBuf,
        /// Maximum number of results
        #[arg(long, default_value = "20")]
        top: usize,
    },
}

pub fn run(action: SyncCheckAction) -> Result<()> {
    match action {
        SyncCheckAction::Check { path, map_file } => {
            let map = load_map(&map_file)?;
            let symbols = map.symbols.as_ref();

            match symbols {
                Some(syms) => {
                    eprintln!("[INFO] Checking doc-code sync at {}", path.display());
                    let doc_refs = analyzer_sync_check::queries::build_doc_refs(&path, &map, syms)?;

                    let total_refs: usize = doc_refs.values().map(|d| d.code_refs.len()).sum();
                    let issues: usize = doc_refs
                        .values()
                        .flat_map(|d| &d.code_refs)
                        .filter(|r| r.issue.is_some())
                        .count();

                    #[derive(serde::Serialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Summary {
                        docs_checked: usize,
                        total_refs: usize,
                        issues_found: usize,
                    }
                    println!(
                        "{}",
                        output::to_json(&Summary {
                            docs_checked: doc_refs.len(),
                            total_refs,
                            issues_found: issues,
                        })
                    );
                    eprintln!(
                        "[OK] Found {} issues in {} refs across {} docs",
                        issues,
                        total_refs,
                        doc_refs.len()
                    );
                    Ok(())
                }
                None => {
                    eprintln!("[ERROR] No symbol data in map file. Run `repo-intel init` first.");
                    std::process::exit(1);
                }
            }
        }
        SyncCheckAction::StaleDocs {
            path,
            map_file,
            top,
        } => {
            let map = load_map(&map_file)?;
            match map.symbols.as_ref() {
                Some(syms) => {
                    let results = analyzer_sync_check::queries::stale_docs(&path, &map, syms, top)?;
                    println!("{}", output::to_json(&results));
                    Ok(())
                }
                None => {
                    eprintln!("[ERROR] No symbol data in map file. Run `repo-intel init` first.");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn load_map(path: &PathBuf) -> Result<RepoIntelData> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read map file: {}", path.display()))?;
    serde_json::from_str(&json).context("failed to parse map JSON")
}
