//! Query functions for doc-code sync checking.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use analyzer_core::types::{CodeRef, DocRefEntry, FileSymbols, RepoIntelData};
use analyzer_core::walk;

use crate::checker;
use crate::matcher;
use crate::parser;

/// Result of a stale-docs query.
#[derive(Debug, Serialize)]
pub struct StaleDocEntry {
    pub doc: String,
    pub line: usize,
    pub reference: String,
    pub issue: String,
    pub suggestion: String,
}

/// Run the full stale-docs analysis.
/// Requires both the git-map data (for renames/deletions/hotspots) and symbol data.
pub fn stale_docs(
    repo_path: &Path,
    map: &RepoIntelData,
    symbols: &HashMap<String, FileSymbols>,
    limit: usize,
) -> Result<Vec<StaleDocEntry>> {
    // Build hotspot set (top 10% most-changed files)
    let hotspot_files = compute_hotspot_set(map);

    let mut results = Vec::new();

    // Walk all markdown files
    walk::walk_files(repo_path, |path| {
        let rel = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Only process markdown files
        if !rel.ends_with(".md") {
            return;
        }

        // Skip noise directories
        if walk::is_noise(&rel) {
            return;
        }

        // Extract and match references
        if let Ok(raw_refs) = parser::extract_code_refs(path) {
            let mut matched = matcher::match_refs(&raw_refs, symbols);
            checker::check_staleness(&mut matched, &map.renames, &map.deletions, &hotspot_files);

            // Collect entries with issues
            for code_ref in &matched {
                if let Some(issue) = &code_ref.issue {
                    let suggestion = make_suggestion(issue, code_ref, map);
                    results.push(StaleDocEntry {
                        doc: rel.clone(),
                        line: code_ref.line.unwrap_or(0),
                        reference: code_ref.text.clone(),
                        issue: issue.clone(),
                        suggestion,
                    });
                }
            }
        }
    })?;

    // Sort by issue severity, then by doc path
    results.sort_by(|a, b| {
        severity(&a.issue)
            .cmp(&severity(&b.issue))
            .then(a.doc.cmp(&b.doc))
            .then(a.line.cmp(&b.line))
    });

    results.truncate(limit);
    Ok(results)
}

/// Build the full doc_refs map for storage in RepoIntelData.
pub fn build_doc_refs(
    repo_path: &Path,
    map: &RepoIntelData,
    symbols: &HashMap<String, FileSymbols>,
) -> Result<HashMap<String, DocRefEntry>> {
    let hotspot_files = compute_hotspot_set(map);
    let mut doc_refs = HashMap::new();

    walk::walk_files(repo_path, |path| {
        let rel = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if !rel.ends_with(".md") || walk::is_noise(&rel) {
            return;
        }

        if let Ok(raw_refs) = parser::extract_code_refs(path) {
            if raw_refs.is_empty() {
                return;
            }

            let mut matched = matcher::match_refs(&raw_refs, symbols);
            checker::check_staleness(&mut matched, &map.renames, &map.deletions, &hotspot_files);

            let references_hot = matched
                .iter()
                .any(|r| r.file.as_ref().is_some_and(|f| hotspot_files.contains(f)));

            // Get doc file last modified date
            let last_updated = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_default();

            doc_refs.insert(
                rel,
                DocRefEntry {
                    code_refs: matched,
                    last_updated,
                    references_hot_files: references_hot,
                },
            );
        }
    })?;

    Ok(doc_refs)
}

fn compute_hotspot_set(map: &RepoIntelData) -> HashSet<String> {
    let mut files: Vec<(&String, u64)> = map
        .file_activity
        .iter()
        .map(|(path, activity)| (path, activity.changes))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));

    let top_10_pct = (files.len() as f64 * 0.1).ceil() as usize;
    files
        .into_iter()
        .take(top_10_pct.max(1))
        .map(|(path, _)| path.clone())
        .collect()
}

fn severity(issue: &str) -> u8 {
    match issue {
        "symbol-deleted" => 0,
        "symbol-renamed" => 1,
        "symbol-not-found" => 2,
        "references-hotspot" => 3,
        _ => 4,
    }
}

fn make_suggestion(issue: &str, code_ref: &CodeRef, map: &RepoIntelData) -> String {
    match issue {
        "symbol-not-found" => {
            format!(
                "Symbol `{}` not found in codebase - may have been removed",
                code_ref.symbol
            )
        }
        "symbol-deleted" => {
            if let Some(del) = map
                .deletions
                .iter()
                .find(|d| code_ref.file.as_ref().is_some_and(|f| f == &d.path))
            {
                format!("File `{}` was deleted in commit {}", del.path, del.commit)
            } else {
                "Referenced file was deleted".to_string()
            }
        }
        "symbol-renamed" => {
            if let Some(rename) = map
                .renames
                .iter()
                .find(|r| code_ref.file.as_ref().is_some_and(|f| f == &r.from))
            {
                format!("Renamed from `{}` to `{}`", rename.from, rename.to)
            } else {
                "Referenced symbol was renamed".to_string()
            }
        }
        "references-hotspot" => {
            if let Some(file) = &code_ref.file {
                if let Some(activity) = map.file_activity.get(file) {
                    format!(
                        "References `{}` which changed {} times - doc may be stale",
                        file, activity.changes
                    )
                } else {
                    "References a frequently-changed file".to_string()
                }
            } else {
                "References a frequently-changed file".to_string()
            }
        }
        _ => "Unknown issue".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hotspot_set() {
        use analyzer_core::types::FileActivity;

        let mut map = test_empty_map();
        map.file_activity.insert(
            "hot.rs".to_string(),
            FileActivity {
                changes: 100,
                recent_changes: 50,
                authors: vec![],
                created: String::new(),
                last_changed: String::new(),
                additions: 0,
                deletions: 0,
                bug_fix_changes: 0,
                refactor_changes: 0,
                last_bug_fix: String::new(),
            },
        );
        map.file_activity.insert(
            "cold.rs".to_string(),
            FileActivity {
                changes: 1,
                recent_changes: 0,
                authors: vec![],
                created: String::new(),
                last_changed: String::new(),
                additions: 0,
                deletions: 0,
                bug_fix_changes: 0,
                refactor_changes: 0,
                last_bug_fix: String::new(),
            },
        );

        let hotspots = compute_hotspot_set(&map);
        assert!(hotspots.contains("hot.rs"));
    }

    fn test_empty_map() -> RepoIntelData {
        use analyzer_core::types::*;
        use chrono::Utc;

        RepoIntelData {
            version: "1.0".to_string(),
            generated: Utc::now(),
            updated: Utc::now(),
            partial: false,
            git: GitInfo {
                analyzed_up_to: String::new(),
                total_commits_analyzed: 0,
                first_commit_date: String::new(),
                last_commit_date: String::new(),
                scope: None,
                shallow: false,
            },
            contributors: Contributors {
                humans: HashMap::new(),
                bots: HashMap::new(),
            },
            file_activity: HashMap::new(),
            coupling: HashMap::new(),
            conventions: ConventionInfo {
                prefixes: HashMap::new(),
                style: "unknown".to_string(),
                uses_scopes: false,
                naming_patterns: None,
                test_patterns: None,
            },
            releases: Releases {
                tags: vec![],
                cadence: "unknown".to_string(),
            },
            renames: vec![],
            deletions: vec![],
            symbols: None,
            import_graph: None,
            project: None,
            doc_refs: None,
            graph: None,
        }
    }
}
