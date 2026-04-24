//! `slop-fixes` query — produces structured fix actions for the deslop
//! agent to apply.
//!
//! Each finding is self-contained: a file, a line range (where applicable),
//! and the action to take. Designed for Haiku-tier execution: the agent
//! reads the lines, confirms the shape still matches, and applies the
//! edit. No further research required.
//!
//! Detectors split by signal source:
//!
//! * **path-based** — tracked artifacts, stale CI configs, duplicate
//!   tooling. Cheap; just walk the repo or git index.
//! * **graph-based** — orphan exports (symbol with 0 importers in the
//!   import graph already collected by analyzer-repo-map).
//! * **AST-based** — empty catch blocks, tautological tests. Uses
//!   tree-sitter against source files.
//!
//! Categories not yet covered (tracked in #27):
//! orphan-files, unused-deps, orphan-snapshots, duplicate-constants,
//! old-todos, old-suppressions. These require additional data slots
//! (entry-points in the artifact, manifest parsing) or are expensive
//! per-finding (git blame).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use streaming_iterator::StreamingIterator;

use analyzer_core::types::RepoIntelData;

/// Concrete edit a deslop agent should apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum SlopAction {
    DeleteFile {
        path: String,
    },
    DeleteLines {
        path: String,
        lines: [u32; 2],
    },
    #[allow(dead_code)]
    ReplaceLines {
        path: String,
        lines: [u32; 2],
        with: String,
    },
}

/// Why a fix was emitted. Stable identifiers so downstream tools can
/// filter or group by category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SlopCategory {
    TrackedArtifact,
    StaleCiConfig,
    DuplicateTooling,
    OrphanExport,
    EmptyCatch,
    TautologicalTest,
}

/// One finding from the `slop-fixes` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlopFix {
    #[serde(flatten)]
    pub action: SlopAction,
    pub category: SlopCategory,
    pub reason: String,
}

/// Aggregate output piped to the deslop agent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlopFixesResult {
    pub fixes: Vec<SlopFix>,
}

/// Run every detector and aggregate.
///
/// `repo_root` is the working tree (used by path-based detectors).
/// `map` is the loaded repo-intel artifact (provides import graph,
/// project metadata, etc).
pub fn slop_fixes(repo_root: &Path, map: &RepoIntelData) -> SlopFixesResult {
    let mut fixes = Vec::new();

    fixes.extend(tracked_artifacts(repo_root));
    fixes.extend(stale_ci_configs(repo_root));
    fixes.extend(duplicate_tooling(repo_root));
    fixes.extend(orphan_exports(map));
    fixes.extend(ast_findings(repo_root));

    SlopFixesResult { fixes }
}

// ── Path-based detectors ─────────────────────────────────────────

/// Tracked log files, editor backups, OS junk, build artifacts. Files
/// matching any pattern are flagged for deletion.
fn tracked_artifacts(repo_root: &Path) -> Vec<SlopFix> {
    let mut out = Vec::new();
    for path in walk_repo_files(repo_root) {
        let rel = relative(&path, repo_root);
        if let Some(reason) = classify_artifact_by_rel(&rel) {
            out.push(SlopFix {
                action: SlopAction::DeleteFile { path: rel },
                category: SlopCategory::TrackedArtifact,
                reason,
            });
        }
    }
    out
}

/// Classify an artifact by its repo-relative path (forward-slash form,
/// lowercase comparison). Splitting by `/` here gives true repo depth
/// rather than absolute filesystem depth.
fn classify_artifact_by_rel(rel_path: &str) -> Option<String> {
    let lower = rel_path.to_ascii_lowercase();
    let name = lower.rsplit('/').next()?;

    // OS / editor junk — flag anywhere in the tree.
    if name == ".ds_store" {
        return Some("macOS Finder metadata".into());
    }
    if name == "thumbs.db" {
        return Some("Windows thumbnail cache".into());
    }
    if name.ends_with(".swp") || name.ends_with(".swo") {
        return Some("Vim swap file".into());
    }
    if name.ends_with(".bak") || name.ends_with(".orig") {
        return Some("backup file".into());
    }

    // Log files at the repo root only — logs inside test fixtures or
    // similar directories may be intentional sample data.
    if name.ends_with(".log") {
        let depth = lower.split('/').count();
        if depth == 1 {
            return Some("tracked log file at repo root".into());
        }
    }

    // Coverage reports — wherever they appear.
    if lower.starts_with("coverage/")
        || lower.contains("/coverage/")
        || lower.starts_with(".nyc_output/")
        || lower.contains("/.nyc_output/")
    {
        return Some("coverage report (should be in .gitignore)".into());
    }

    None
}

/// Stale CI configs — e.g. `.travis.yml` left behind after migrating to
/// GitHub Actions.
fn stale_ci_configs(repo_root: &Path) -> Vec<SlopFix> {
    let mut out = Vec::new();
    let has_gh_actions = repo_root.join(".github/workflows").is_dir();
    let has_gitlab_ci = repo_root.join(".gitlab-ci.yml").is_file();
    let has_circleci = repo_root.join(".circleci/config.yml").is_file();

    let active_count = [has_gh_actions, has_gitlab_ci, has_circleci]
        .iter()
        .filter(|b| **b)
        .count();

    let stale_candidates = [
        (".travis.yml", "Travis CI"),
        ("appveyor.yml", "AppVeyor"),
        (".drone.yml", "Drone CI"),
        ("bitbucket-pipelines.yml", "Bitbucket Pipelines"),
    ];

    for (file, name) in stale_candidates {
        if repo_root.join(file).is_file() && active_count > 0 {
            let other = if has_gh_actions {
                "GitHub Actions"
            } else if has_gitlab_ci {
                "GitLab CI"
            } else {
                "CircleCI"
            };
            out.push(SlopFix {
                action: SlopAction::DeleteFile {
                    path: file.to_string(),
                },
                category: SlopCategory::StaleCiConfig,
                reason: format!("{name} config present alongside active {other}"),
            });
        }
    }

    out
}

/// Two tools doing the same job — typically left over from migrations.
fn duplicate_tooling(repo_root: &Path) -> Vec<SlopFix> {
    let mut out = Vec::new();

    // ESLint + Biome
    let has_eslint = repo_root.join(".eslintrc.json").is_file()
        || repo_root.join(".eslintrc.js").is_file()
        || repo_root.join("eslint.config.js").is_file()
        || repo_root.join("eslint.config.mjs").is_file();
    let has_biome =
        repo_root.join("biome.json").is_file() || repo_root.join(".biome.json").is_file();
    if has_eslint && has_biome {
        out.push(SlopFix {
            action: SlopAction::DeleteFile {
                path: ".eslintrc.json".into(),
            },
            category: SlopCategory::DuplicateTooling,
            reason: "Biome present — ESLint config can usually be removed".into(),
        });
    }

    // Prettier + Biome (Biome formats too)
    let has_prettier = repo_root.join(".prettierrc").is_file()
        || repo_root.join(".prettierrc.json").is_file()
        || repo_root.join("prettier.config.js").is_file();
    if has_prettier && has_biome {
        out.push(SlopFix {
            action: SlopAction::DeleteFile {
                path: ".prettierrc".into(),
            },
            category: SlopCategory::DuplicateTooling,
            reason: "Biome present — Prettier config can usually be removed".into(),
        });
    }

    // Multiple JS lockfiles (unambiguous slop)
    let lockfiles: Vec<&str> = [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
    ]
    .iter()
    .filter(|f| repo_root.join(f).is_file())
    .copied()
    .collect();
    if lockfiles.len() > 1 {
        // Keep one; flag the rest as slop. We don't know which one is
        // canonical, so we flag all but the first alphabetically and
        // let the agent confirm.
        for lockfile in &lockfiles[1..] {
            out.push(SlopFix {
                action: SlopAction::DeleteFile {
                    path: lockfile.to_string(),
                },
                category: SlopCategory::DuplicateTooling,
                reason: format!(
                    "multiple JS lockfiles present ({}); only one package manager should be active",
                    lockfiles.join(", ")
                ),
            });
        }
    }

    out
}

// ── Graph-based detectors ────────────────────────────────────────

/// Symbols exported but never imported anywhere. Uses the import graph
/// already collected by analyzer-repo-map (Phase 2). Skips files where
/// no symbol data is available (graceful degradation).
fn orphan_exports(map: &RepoIntelData) -> Vec<SlopFix> {
    let symbols = match map.symbols.as_ref() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let import_graph = match map.import_graph.as_ref() {
        Some(g) => g,
        None => return Vec::new(),
    };

    // Build reverse import index: for each imported file path, who imports it?
    let mut importers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (importer, imports) in import_graph {
        for target in imports {
            importers
                .entry(target.as_str())
                .or_default()
                .push(importer.as_str());
        }
    }

    let mut out = Vec::new();
    for (file_path, file_symbols) in symbols {
        // A file with zero importers and at least one export is a
        // candidate for orphan-export reporting (file-level), but the
        // user may want it as an entry point. Be conservative: only
        // flag exports if the file has importers (so we know it's used)
        // but specific exports aren't named by any importer's known
        // symbols. Without per-symbol import resolution that requires
        // language-specific work, we defer the per-symbol case.
        //
        // For now, the high-confidence case: a file with EXPORTS but
        // ZERO importers in the entire graph is itself orphan. We emit
        // a single fix per file pointing at the export line ranges.
        let has_importers = importers
            .get(file_path.as_str())
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if has_importers {
            continue;
        }
        if file_symbols.exports.is_empty() {
            continue;
        }
        // Skip entry-point-ish files heuristically: paths matching
        // `main.rs`, `index.{ts,js}`, `__main__.py`, `cmd/.../main.go`.
        if looks_like_entry_point(file_path) {
            continue;
        }

        for export in &file_symbols.exports {
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: file_path.clone(),
                    lines: [export.line as u32, export.line as u32],
                },
                category: SlopCategory::OrphanExport,
                reason: format!(
                    "exported symbol `{}` — file has zero importers in the project graph",
                    export.name
                ),
            });
        }
    }
    out
}

fn looks_like_entry_point(path: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    lower.ends_with("/main.rs")
        || lower == "main.rs"
        || lower.ends_with("/main.go")
        || lower.ends_with("/main.py")
        || lower.ends_with("/__main__.py")
        || lower.ends_with("/index.ts")
        || lower.ends_with("/index.tsx")
        || lower.ends_with("/index.js")
        || lower.ends_with("/index.jsx")
        || lower.ends_with("/index.mjs")
        || lower == "index.ts"
        || lower == "index.js"
        || lower.ends_with("/lib.rs")
        || lower == "lib.rs"
}

// ── AST-based detectors ──────────────────────────────────────────

/// Walk source files and run language-specific tree-sitter queries to
/// find empty catch blocks and tautological tests.
fn ast_findings(repo_root: &Path) -> Vec<SlopFix> {
    let mut out = Vec::new();
    for path in walk_repo_files(repo_root) {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_ascii_lowercase(),
            None => continue,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rel = relative(&path, repo_root);
        match ext.as_str() {
            "ts" | "tsx" => {
                out.extend(empty_catches_ts_js(
                    &content,
                    &rel,
                    &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                ));
                out.extend(tautological_tests_ts_js(
                    &content,
                    &rel,
                    &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                ));
            }
            "js" | "jsx" | "mjs" | "cjs" => {
                out.extend(empty_catches_ts_js(
                    &content,
                    &rel,
                    &tree_sitter_javascript::LANGUAGE.into(),
                ));
                out.extend(tautological_tests_ts_js(
                    &content,
                    &rel,
                    &tree_sitter_javascript::LANGUAGE.into(),
                ));
            }
            "py" => {
                out.extend(empty_excepts_python(&content, &rel));
            }
            _ => {}
        }
    }
    out
}

fn empty_catches_ts_js(content: &str, rel: &str, language: &tree_sitter::Language) -> Vec<SlopFix> {
    let query_src = r#"(catch_clause body: (statement_block) @body)"#;
    run_ast_query(content, language, query_src, |node| {
        // Empty if body has no children (i.e. just `{}`).
        if node.named_child_count() != 0 {
            return None;
        }
        let start = (node.start_position().row as u32) + 1;
        let end = (node.end_position().row as u32) + 1;
        Some(SlopFix {
            action: SlopAction::DeleteLines {
                path: rel.to_string(),
                lines: [start, end],
            },
            category: SlopCategory::EmptyCatch,
            reason: "empty catch block silently swallows errors".into(),
        })
    })
}

fn empty_excepts_python(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_python::LANGUAGE.into();
    let query_src = r#"(except_clause (block) @body)"#;
    run_ast_query(content, &language, query_src, |node| {
        // "Empty" in Python means just `pass` or no statements. Look
        // for a block whose only statement is `pass`.
        let mut effective_children = 0;
        let mut only_pass = true;
        for i in 0..node.named_child_count() {
            let child = node.named_child(i)?;
            effective_children += 1;
            if child.kind() != "pass_statement" {
                only_pass = false;
            }
        }
        if effective_children == 0 || (effective_children == 1 && only_pass) {
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            return Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::EmptyCatch,
                reason: "bare except: pass silently swallows errors".into(),
            });
        }
        None
    })
}

fn tautological_tests_ts_js(
    content: &str,
    rel: &str,
    language: &tree_sitter::Language,
) -> Vec<SlopFix> {
    // Match `expect(<expr>).toBe(<expr>)` and `expect(<expr>).toEqual(<expr>)`
    // where both expressions are syntactically identical literals/identifiers.
    // Heuristic captures: just look for `expect(true).toBe(true)`,
    // `expect(1).toBe(1)`, etc. — false positives kept minimal by
    // requiring both sides to be literals.
    let query_src = r#"
    (call_expression
      function: (member_expression
        object: (call_expression
          function: (identifier) @expect_fn (#eq? @expect_fn "expect")
          arguments: (arguments . (_) @left .))
        property: (property_identifier) @method
        (#match? @method "^(toBe|toEqual|toStrictEqual)$"))
      arguments: (arguments . (_) @right .)) @call
    "#;

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let query = match tree_sitter::Query::new(language, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());

    let call_idx = query.capture_index_for_name("call");
    let left_idx = query.capture_index_for_name("left");
    let right_idx = query.capture_index_for_name("right");

    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    while let Some(m) = matches.next() {
        let mut call_node = None;
        let mut left_text: Option<&str> = None;
        let mut right_text: Option<&str> = None;
        for cap in m.captures {
            if Some(cap.index) == call_idx {
                call_node = Some(cap.node);
            } else if Some(cap.index) == left_idx {
                left_text = cap.node.utf8_text(content.as_bytes()).ok();
            } else if Some(cap.index) == right_idx {
                right_text = cap.node.utf8_text(content.as_bytes()).ok();
            }
        }
        let (Some(call), Some(left), Some(right)) = (call_node, left_text, right_text) else {
            continue;
        };
        if left.trim() != right.trim() {
            continue;
        }
        // Require both sides to be a literal-ish thing (number, string,
        // boolean, or simple identifier) to suppress false positives
        // where the same complex expression incidentally appears twice.
        if !is_literal_or_identifier(left.trim()) {
            continue;
        }
        let start = (call.start_position().row as u32) + 1;
        let end = (call.end_position().row as u32) + 1;
        if !seen.insert((start, end)) {
            continue;
        }
        out.push(SlopFix {
            action: SlopAction::DeleteLines {
                path: rel.to_string(),
                lines: [start, end],
            },
            category: SlopCategory::TautologicalTest,
            reason: format!(
                "tautological assertion: `expect({left}).{}` always passes",
                "toBe(...)"
            ),
        });
    }
    out
}

fn is_literal_or_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s == "true" || s == "false" || s == "null" || s == "undefined" {
        return true;
    }
    if s.starts_with('"') || s.starts_with('\'') || s.starts_with('`') {
        return true;
    }
    if s.chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        return true;
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn run_ast_query<F>(
    content: &str,
    language: &tree_sitter::Language,
    query_src: &str,
    mut on_node: F,
) -> Vec<SlopFix>
where
    F: FnMut(tree_sitter::Node) -> Option<SlopFix>,
{
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let query = match tree_sitter::Query::new(language, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());
    let body_idx = query.capture_index_for_name("body");

    let mut out = Vec::new();
    while let Some(m) = matches.next() {
        for cap in m.captures {
            if Some(cap.index) == body_idx {
                if let Some(fix) = on_node(cap.node) {
                    out.push(fix);
                }
            }
        }
    }
    out
}

// ── Helpers ──────────────────────────────────────────────────────

fn walk_repo_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(false)
        .build()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            out.push(entry.path().to_path_buf());
        }
    }
    out
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &TempDir, rel: &str, content: &str) {
        let path = dir.path().join(rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn detects_tracked_log_at_top_level() {
        let dir = TempDir::new().unwrap();
        write(&dir, "debug.log", "");
        let fixes = tracked_artifacts(dir.path());
        assert!(fixes.iter().any(|f| matches!(
            &f.action,
            SlopAction::DeleteFile { path } if path == "debug.log"
        )));
    }

    #[test]
    fn ignores_log_in_test_fixtures() {
        let dir = TempDir::new().unwrap();
        write(&dir, "tests/fixtures/sample/output.log", "");
        let fixes = tracked_artifacts(dir.path());
        assert!(!fixes.iter().any(
            |f| matches!(&f.action, SlopAction::DeleteFile { path } if path.ends_with("output.log"))
        ));
    }

    #[test]
    fn detects_ds_store_anywhere() {
        let dir = TempDir::new().unwrap();
        write(&dir, "src/.DS_Store", "");
        let fixes = tracked_artifacts(dir.path());
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].category, SlopCategory::TrackedArtifact);
    }

    #[test]
    fn detects_swap_files() {
        let dir = TempDir::new().unwrap();
        write(&dir, "src/foo.rs.swp", "");
        let fixes = tracked_artifacts(dir.path());
        assert_eq!(fixes.len(), 1);
    }

    #[test]
    fn detects_coverage_report() {
        let dir = TempDir::new().unwrap();
        write(&dir, "coverage/report.html", "");
        let fixes = tracked_artifacts(dir.path());
        assert!(fixes.iter().any(|f| matches!(&f.action, SlopAction::DeleteFile { path } if path == "coverage/report.html")));
    }

    #[test]
    fn flags_travis_when_gh_actions_present() {
        let dir = TempDir::new().unwrap();
        write(&dir, ".travis.yml", "");
        write(&dir, ".github/workflows/ci.yml", "");
        let fixes = stale_ci_configs(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleCiConfig)
        );
    }

    #[test]
    fn does_not_flag_travis_when_no_modern_ci() {
        let dir = TempDir::new().unwrap();
        write(&dir, ".travis.yml", "");
        let fixes = stale_ci_configs(dir.path());
        assert!(fixes.is_empty());
    }

    #[test]
    fn flags_eslint_alongside_biome() {
        let dir = TempDir::new().unwrap();
        write(&dir, ".eslintrc.json", "{}");
        write(&dir, "biome.json", "{}");
        let fixes = duplicate_tooling(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::DuplicateTooling)
        );
    }

    #[test]
    fn flags_multiple_lockfiles() {
        let dir = TempDir::new().unwrap();
        write(&dir, "package-lock.json", "");
        write(&dir, "yarn.lock", "");
        let fixes = duplicate_tooling(dir.path());
        // One of the two flagged (we keep one).
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::DuplicateTooling)
        );
    }

    #[test]
    fn does_not_flag_single_lockfile() {
        let dir = TempDir::new().unwrap();
        write(&dir, "package-lock.json", "");
        let fixes = duplicate_tooling(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::DuplicateTooling)
        );
    }

    #[test]
    fn detects_empty_catch_in_typescript() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "a.ts",
            "function f() { try { doThing() } catch (e) {} }\n",
        );
        let fixes = ast_findings(dir.path());
        assert!(fixes.iter().any(|f| f.category == SlopCategory::EmptyCatch));
    }

    #[test]
    fn ignores_non_empty_catch_in_typescript() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "a.ts",
            "function f() { try { doThing() } catch (e) { console.error(e) } }\n",
        );
        let fixes = ast_findings(dir.path());
        assert!(!fixes.iter().any(|f| f.category == SlopCategory::EmptyCatch));
    }

    #[test]
    fn detects_python_bare_except_pass() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "a.py",
            "def f():\n    try:\n        x = 1\n    except:\n        pass\n",
        );
        let fixes = ast_findings(dir.path());
        assert!(fixes.iter().any(|f| f.category == SlopCategory::EmptyCatch));
    }

    #[test]
    fn detects_tautological_tobe_in_typescript() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "a.test.ts",
            "test('x', () => { expect(true).toBe(true); });\n",
        );
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest)
        );
    }

    #[test]
    fn does_not_flag_real_assertion() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "a.test.ts",
            "test('x', () => { expect(getValue()).toBe(42); });\n",
        );
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest)
        );
    }

    #[test]
    fn slop_fixes_serializes_as_tagged_action() {
        let fix = SlopFix {
            action: SlopAction::DeleteFile {
                path: "x.log".into(),
            },
            category: SlopCategory::TrackedArtifact,
            reason: "test".into(),
        };
        let json = serde_json::to_string(&fix).unwrap();
        assert!(json.contains("\"action\":\"delete-file\""));
        assert!(json.contains("\"path\":\"x.log\""));
        assert!(json.contains("\"category\":\"tracked-artifact\""));
    }
}
