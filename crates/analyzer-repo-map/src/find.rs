//! Concept-to-file search: rank files by relevance to a fuzzy query.
//!
//! Replaces the agent's first-foothold reflex of `grep -r <concept>`,
//! which returns 100s of noisy hits. This module returns a ranked list
//! of paths with a one-line `why` string per result so the caller can
//! skim instead of opening every file.
//!
//! # Scoring (v0, deterministic)
//!
//! Per query term (case-insensitive substring), each file gains:
//! - basename match: **5.0** (e.g. query "worker" hits `worker.rs`)
//! - path-component match: **3.0** (query "auth" hits `src/auth/login.ts`)
//! - exported-symbol name match: **2.0** per symbol, capped at 3/term
//! - top-of-file doc-comment match: **1.5** (first ~500 bytes scanned)
//! - import-name match: **1.0**
//!
//! Final score is the sum across terms. Files with score 0 are dropped.
//! Ties break on path ascending for stable output.
//!
//! # Out of scope (v0)
//!
//! - Semantic synonyms (worker ↔ executor, queue ↔ channel) - see
//!   issue #20 v1, which layers Haiku-generated descriptors on top of
//!   this scorer at init/update time
//! - Files outside the symbol index (markdown, config, manifests) -
//!   only files that the AST extractor processed are considered
//! - Full-body content search - that's what `grep` is for; we only
//!   look at the doc-comment header

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use analyzer_core::types::FileSymbols;

/// One match in a find result, ordered most-relevant first.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindResult {
    pub path: String,
    pub score: f64,
    /// One-line, human-readable rationale. Lists the strongest signals
    /// that contributed to the score, e.g.
    /// `"basename matches 'worker'; exports `WorkerPool`, `Worker`"`.
    pub why: String,
}

/// Find files relevant to `query` in the repo.
///
/// `symbols` is the AST symbol index; `repo_path` is needed only for
/// reading the first ~500 bytes of each candidate file to score
/// doc-comment matches. Pass `limit = 0` to get all matches.
pub fn find(
    symbols: &HashMap<String, FileSymbols>,
    repo_path: &Path,
    query: &str,
    limit: usize,
) -> Vec<FindResult> {
    let terms = tokenize(query);
    if terms.is_empty() {
        return Vec::new();
    }

    let mut results: Vec<FindResult> = symbols
        .iter()
        .filter_map(|(path, file_syms)| score_file(path, file_syms, repo_path, &terms))
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    if limit > 0 {
        results.truncate(limit);
    }
    results
}

/// Common English stopwords. Filtered out *after* the length check so
/// real 2-letter tokens like `io`, `fs`, `rs`, `go` still match.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "be", "by", "do", "for", "from", "if", "in", "is", "it", "of",
    "on", "or", "that", "the", "this", "to", "was", "with",
];

/// Tokenize a query into lowercase terms of length >= 2 with stopwords
/// stripped. Whitespace and punctuation split tokens.
fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect()
}

fn score_file(
    path: &str,
    file_syms: &FileSymbols,
    repo_path: &Path,
    terms: &[String],
) -> Option<FindResult> {
    let path_lower = path.to_ascii_lowercase();
    let basename_lower = path_lower
        .rsplit('/')
        .next()
        .unwrap_or(path_lower.as_str())
        .to_string();

    let mut total_score = 0.0_f64;
    let mut why_fragments: Vec<String> = Vec::new();

    // HashSets for the rationale-display de-duplication. Membership
    // checks are O(1) and crucially decoupled from scoring - every
    // substring match increments score (per term), HashSet only
    // controls whether the name appears in the `why` preview.
    let mut matched_symbols: HashSet<String> = HashSet::new();
    let mut matched_imports: HashSet<String> = HashSet::new();
    let mut path_component_hits = 0;
    let mut basename_hit = false;

    // Pass 1: cheap signals (basename, path, exports, imports). No
    // file I/O. This drives whether we bother reading the doc header
    // at all.
    for term in terms {
        // 1. Basename substring match (very strong)
        if basename_lower.contains(term) {
            total_score += 5.0;
            basename_hit = true;
        }

        // 2. Path component matches (strong, but lower than basename
        // alone since basename already counted as one path component)
        let component_hits: usize = path_lower.split('/').filter(|c| c.contains(term)).count();
        // Subtract 1 if basename already contributed, to avoid
        // double-counting the basename as both "basename" and "path".
        let extra_components = if basename_lower.contains(term) {
            component_hits.saturating_sub(1)
        } else {
            component_hits
        };
        if extra_components > 0 {
            total_score += 3.0 * extra_components as f64;
            path_component_hits += extra_components;
        }

        // 3. Exported symbol name matches (cap 3 per term so a file
        // exporting 50 things named after the term doesn't dominate)
        let mut sym_count = 0;
        for sym in &file_syms.exports {
            if sym_count >= 3 {
                break;
            }
            if sym.name.to_ascii_lowercase().contains(term) {
                total_score += 2.0;
                sym_count += 1;
                matched_symbols.insert(sym.name.clone());
            }
        }

        // 4. Import name matches (low). Score every match, regardless
        // of whether the name was already added to the display set -
        // otherwise multi-term queries silently under-count when the
        // same import string matches several terms.
        for imp in &file_syms.imports {
            for name in &imp.names {
                if name.to_ascii_lowercase().contains(term) {
                    total_score += 1.0;
                    matched_imports.insert(name.clone());
                }
            }
            if imp.from.to_ascii_lowercase().contains(term) {
                total_score += 1.0;
                matched_imports.insert(imp.from.clone());
            }
        }
    }

    // Pass 2: doc-comment header. Read the file once (only if some
    // cheap signal already fired), then score each term against it
    // independently. This is order-independent unlike the previous
    // gate-on-running-total approach.
    let mut doc_hits = 0;
    if total_score > 0.0
        && let Some(header) = read_doc_header(repo_path, path)
    {
        let header_lower = header.to_ascii_lowercase();
        for term in terms {
            if header_lower.contains(term) {
                total_score += 1.5;
                doc_hits += 1;
            }
        }
    }

    if total_score == 0.0 {
        return None;
    }

    // Build the `why` string from the strongest signals.
    if basename_hit {
        why_fragments.push("basename matches".to_string());
    }
    if path_component_hits > 0 {
        why_fragments.push("path matches".to_string());
    }
    if !matched_symbols.is_empty() {
        // Sort to keep `why` strings stable across HashSet iteration.
        let mut symbols_sorted: Vec<&String> = matched_symbols.iter().collect();
        symbols_sorted.sort();
        let preview: Vec<String> = symbols_sorted
            .iter()
            .take(3)
            .map(|s| format!("`{s}`"))
            .collect();
        why_fragments.push(format!("exports {}", preview.join(", ")));
    }
    if doc_hits > 0 {
        why_fragments.push("module doc mentions term".to_string());
    }
    if !matched_imports.is_empty() {
        let mut imports_sorted: Vec<&String> = matched_imports.iter().collect();
        imports_sorted.sort();
        let preview: Vec<String> = imports_sorted
            .iter()
            .take(2)
            .map(|s| format!("`{}`", short_import_label(s)))
            .collect();
        why_fragments.push(format!("imports {}", preview.join(", ")));
    }
    let why = if why_fragments.is_empty() {
        "scored above zero".to_string()
    } else {
        why_fragments.join("; ")
    };

    Some(FindResult {
        path: path.to_string(),
        score: total_score,
        why,
    })
}

/// Render an import label fit for a one-line `why` string. Some
/// language parsers (notably Rust) store multi-line `crate::{ ... }`
/// blocks as a single import `from`, which would otherwise dominate
/// the rationale. Collapse whitespace and clip to ~40 chars so the
/// caller still sees a readable hint.
fn short_import_label(raw: &str) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 40 {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(37).collect();
        format!("{truncated}...")
    }
}

/// Read up to ~500 bytes from the head of a source file and return the
/// portion that is actually doc-comment content (lines starting with
/// `//`, `///`, `#`, `"""`, `'''`, `/*`, `*`). Returns `None` if the
/// file can't be read.
fn read_doc_header(repo_path: &Path, file_rel: &str) -> Option<String> {
    let abs = repo_path.join(file_rel);
    let bytes = std::fs::read(&abs).ok()?;
    let truncated = if bytes.len() > 512 {
        &bytes[..512]
    } else {
        &bytes[..]
    };
    let text = std::str::from_utf8(truncated).ok()?;

    let mut header = String::new();
    let mut in_block_comment = false;
    for line in text.lines() {
        let trimmed = line.trim_start();

        if in_block_comment {
            // Inside a /* ... */ block: include every line verbatim
            // until we hit the closer, even if intermediate lines
            // don't start with `*`.
            header.push_str(trimmed);
            header.push('\n');
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }

        if trimmed.starts_with("/*") {
            header.push_str(trimmed);
            header.push('\n');
            // Single-line `/* ... */` closes immediately; multi-line
            // `/*` without a closer flips us into block mode.
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }

        if trimmed.starts_with("//")
            || trimmed.starts_with('*')
            || trimmed.starts_with('#')
            || trimmed.starts_with("\"\"\"")
            || trimmed.starts_with("'''")
        {
            header.push_str(trimmed);
            header.push('\n');
        } else if !trimmed.is_empty() {
            // First non-comment, non-blank line ends the header.
            break;
        }
    }
    Some(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{ImportEntry, SymbolEntry, SymbolKind};
    use std::fs;
    use tempfile::TempDir;

    fn fs_with(name: &str, kind: SymbolKind) -> SymbolEntry {
        SymbolEntry {
            name: name.to_string(),
            kind,
            line: 1,
        }
    }

    fn make_file_syms(exports: Vec<SymbolEntry>, imports: Vec<ImportEntry>) -> FileSymbols {
        FileSymbols {
            exports,
            imports,
            definitions: vec![],
        }
    }

    #[test]
    fn tokenize_drops_short_punctuation_and_stopwords() {
        assert_eq!(tokenize("worker pool"), vec!["worker", "pool"]);
        // Single-letter "a" dropped by length, "of" "to" by stopword
        // list - so common-word noise like `prof_*` / `auto*` matches
        // does not flood results.
        assert_eq!(tokenize("a of to"), Vec::<String>::new());
        // Real 2-letter technical tokens survive: `io`, `fs`, `rs`,
        // `go` are common in Rust/Go projects and should still match.
        assert_eq!(tokenize("io fs"), vec!["io", "fs"]);
        assert_eq!(
            tokenize("worker-pool, async!"),
            vec!["worker", "pool", "async"]
        );
        assert_eq!(tokenize("WORKER"), vec!["worker"]);
    }

    #[test]
    fn empty_query_yields_empty_results() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert(
            "src/lib.rs".to_string(),
            make_file_syms(vec![fs_with("Worker", SymbolKind::Struct)], vec![]),
        );
        assert!(find(&syms, dir.path(), "", 10).is_empty());
        assert!(find(&syms, dir.path(), "   ", 10).is_empty());
    }

    #[test]
    fn basename_match_scores_highest() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert("src/worker.rs".to_string(), make_file_syms(vec![], vec![]));
        syms.insert("src/lib.rs".to_string(), make_file_syms(vec![], vec![]));

        let results = find(&syms, dir.path(), "worker", 10);
        // worker.rs should appear (basename hit); lib.rs should not.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "src/worker.rs");
        assert!(results[0].score >= 5.0);
        assert!(results[0].why.contains("basename"));
    }

    #[test]
    fn path_component_match_scores_lower_than_basename() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        // "auth" appears in path but not basename
        syms.insert(
            "src/auth/login.ts".to_string(),
            make_file_syms(vec![], vec![]),
        );
        // "auth" appears in basename
        syms.insert("src/auth.rs".to_string(), make_file_syms(vec![], vec![]));

        let results = find(&syms, dir.path(), "auth", 10);
        assert_eq!(results.len(), 2);
        // basename hit (auth.rs) should rank above path-only (auth/login.ts).
        assert_eq!(results[0].path, "src/auth.rs");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn exported_symbol_match_contributes_score() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert(
            "src/lib.rs".to_string(),
            make_file_syms(
                vec![
                    fs_with("WorkerPool", SymbolKind::Struct),
                    fs_with("spawn_worker", SymbolKind::Function),
                ],
                vec![],
            ),
        );
        syms.insert(
            "src/other.rs".to_string(),
            make_file_syms(vec![fs_with("Foo", SymbolKind::Struct)], vec![]),
        );

        let results = find(&syms, dir.path(), "worker", 10);
        let lib = results.iter().find(|r| r.path == "src/lib.rs").unwrap();
        assert!(lib.why.contains("exports"));
        assert!(lib.why.contains("WorkerPool"));
        assert!(!results.iter().any(|r| r.path == "src/other.rs"));
    }

    #[test]
    fn multi_term_query_scores_independently() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert(
            "src/worker_pool.rs".to_string(),
            make_file_syms(vec![], vec![]),
        );
        syms.insert("src/worker.rs".to_string(), make_file_syms(vec![], vec![]));

        let results = find(&syms, dir.path(), "worker pool", 10);
        // worker_pool.rs matches BOTH terms in the basename, so it
        // should outrank worker.rs which only matches one.
        assert_eq!(results[0].path, "src/worker_pool.rs");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn doc_comment_match_adds_signal_when_file_readable() {
        let dir = TempDir::new().unwrap();
        let path = "src/runtime.rs";
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join(path),
            "//! Worker pool implementation.\n//!\n//! Spawns N threads.\nfn run() {}\n",
        )
        .unwrap();

        let mut syms = HashMap::new();
        // No basename match, no symbol export match - the only signal
        // for "worker" should be the doc-comment header. To trigger
        // the doc-comment scan we still need a non-zero base score, so
        // include an import that matches.
        syms.insert(
            path.to_string(),
            make_file_syms(
                vec![],
                vec![ImportEntry {
                    from: "worker".to_string(),
                    names: vec!["spawn".to_string()],
                }],
            ),
        );

        let results = find(&syms, dir.path(), "worker", 10);
        let runtime = results.iter().find(|r| r.path == path).unwrap();
        assert!(runtime.why.contains("module doc mentions term"));
    }

    #[test]
    fn limit_truncates_and_sort_is_stable() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        for n in &["a", "b", "c", "d"] {
            syms.insert(format!("src/{n}_worker.rs"), make_file_syms(vec![], vec![]));
        }
        let results = find(&syms, dir.path(), "worker", 2);
        assert_eq!(results.len(), 2);
        // All four have the same basename score; ties break on path asc,
        // so a_worker.rs and b_worker.rs come first.
        assert_eq!(results[0].path, "src/a_worker.rs");
        assert_eq!(results[1].path, "src/b_worker.rs");
    }

    #[test]
    fn block_comment_intermediate_lines_are_included() {
        // Reviewer-caught: multi-line /* */ block comments where
        // intermediate lines don't start with `*` were terminating
        // the header scan early. Ensure the second line is now
        // captured for scoring.
        let dir = TempDir::new().unwrap();
        let path = "src/blockdoc.rs";
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join(path),
            "/* A block comment\n   that mentions worker pools without leading-star\n   on every line. */\nfn run() {}\n",
        )
        .unwrap();

        let mut syms = HashMap::new();
        // Need a non-zero base score to trigger the doc read.
        syms.insert(
            path.to_string(),
            make_file_syms(
                vec![],
                vec![ImportEntry {
                    from: "worker".to_string(),
                    names: vec![],
                }],
            ),
        );

        // Multi-term query: "worker" hits via the import (cheap
        // signal → seeds the base score → doc gets read), then
        // "pool" must match in the intermediate block-comment line.
        let results = find(&syms, dir.path(), "worker pool", 10);
        let r = results
            .iter()
            .find(|r| r.path == path)
            .expect("file should match at least one term");
        // Score breakdown: 1.0 (worker import) + 1.5 (worker in doc)
        // + 1.5 (pool in doc, only possible if intermediate line read).
        assert!(
            r.score >= 4.0,
            "expected pool to match block-comment intermediate line (score >= 4.0); got {} with why={:?}",
            r.score,
            r.why
        );
    }

    #[test]
    fn multi_term_import_score_is_order_independent() {
        // Reviewer-caught: a file with a single `from = "worker_pool"`
        // import previously only scored that import once even when
        // the multi-term query matched both halves. Score must add
        // 1.0 per (term, matching from) - independent of dedup.
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert(
            "src/uses.rs".to_string(),
            make_file_syms(
                vec![],
                vec![ImportEntry {
                    from: "worker_pool".to_string(),
                    names: vec![],
                }],
            ),
        );

        let r1 = find(&syms, dir.path(), "worker pool", 10);
        let r2 = find(&syms, dir.path(), "pool worker", 10);
        let s1 = r1.iter().find(|r| r.path == "src/uses.rs").unwrap().score;
        let s2 = r2.iter().find(|r| r.path == "src/uses.rs").unwrap().score;
        assert!(
            (s1 - s2).abs() < f64::EPSILON,
            "score must not depend on term order"
        );
        // Both terms match the single `from`, so we expect 2.0 (1.0
        // per term-match), regardless of HashSet dedup.
        assert!(
            s1 >= 2.0,
            "expected at least 2.0 from two term matches; got {s1}"
        );
    }

    #[test]
    fn doc_comment_score_is_order_independent() {
        // Reviewer-caught: doc-comment scoring previously gated on
        // running total > 0 inside the per-term loop, making the
        // signal order-dependent. After the two-pass refactor it
        // must give identical scores regardless of term order.
        let dir = TempDir::new().unwrap();
        let path = "src/elegant.rs";
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join(path),
            "//! An elegant worker pool implementation.\nfn run() {}\n",
        )
        .unwrap();

        let mut syms = HashMap::new();
        syms.insert(path.to_string(), make_file_syms(vec![], vec![]));

        // basename `elegant.rs` matches "elegant"; doc mentions both
        // "elegant" and "worker". Order shouldn't matter.
        let s1 = find(&syms, dir.path(), "elegant worker", 10)
            .iter()
            .find(|r| r.path == path)
            .unwrap()
            .score;
        let s2 = find(&syms, dir.path(), "worker elegant", 10)
            .iter()
            .find(|r| r.path == path)
            .unwrap()
            .score;
        assert!(
            (s1 - s2).abs() < f64::EPSILON,
            "doc-comment scoring must not depend on term order ({s1} vs {s2})"
        );
    }

    #[test]
    fn long_multiline_import_is_truncated_in_why() {
        // Rust parsers store `use crate::{\n    foo,\n    bar,\n}` as
        // a single multi-line `from`. The why string must collapse
        // whitespace and clip so the rationale stays one readable line.
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert(
            "src/uses_lots.rs".to_string(),
            make_file_syms(
                vec![],
                vec![ImportEntry {
                    from: "crate::{\n    Worker,\n    Pool,\n    Spawner,\n    Builder,\n    Helper,\n}".to_string(),
                    names: vec![],
                }],
            ),
        );

        let results = find(&syms, dir.path(), "worker", 10);
        let r = results
            .iter()
            .find(|r| r.path == "src/uses_lots.rs")
            .unwrap();
        // Why string must not contain a literal newline.
        assert!(!r.why.contains('\n'), "why must be one line: {:?}", r.why);
        // Truncation marker should appear since the input is long.
        assert!(
            r.why.contains("..."),
            "long import preview should be truncated: {:?}",
            r.why
        );
    }

    #[test]
    fn no_match_returns_empty() {
        let dir = TempDir::new().unwrap();
        let mut syms = HashMap::new();
        syms.insert("src/lib.rs".to_string(), make_file_syms(vec![], vec![]));
        assert!(find(&syms, dir.path(), "nonexistent", 10).is_empty());
    }
}
