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
/// find empty error handling and tautological assertions.
///
/// "Empty error handling" is a language-by-language judgment call:
///
/// * Java / TS / JS — `catch` block with empty body, plus
///   `Promise.catch(() => {})`.
/// * Python — `except: pass`, `except: ...` (Ellipsis body), bare
///   `except:` without a specific exception class.
/// * Rust — no `catch`. We flag idiomatic error-swallowing patterns:
///   `let _ = expr;` (assignment discards), `expr.ok();` (drops Err
///   variant of Result), `if let Err(_) = expr {}` (empty err arm).
/// * Go — no `catch`. We flag empty `if err != nil { }` blocks and
///   `_ = expr` discards (where the discarded value is an `error`).
///
/// "Tautological assertion" covers the common test-framework call
/// shapes per language. The umbrella category stays
/// `SlopCategory::TautologicalTest`; the precise framework + form is
/// recorded in the `reason` field so downstream consumers can route
/// per-pattern reviewer prompts.
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
                let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
                out.extend(empty_catches_ts_js(&content, &rel, &lang));
                out.extend(promise_empty_catches_ts_js(&content, &rel, &lang));
                out.extend(tautological_tests_ts_js(&content, &rel, &lang));
            }
            "js" | "jsx" | "mjs" | "cjs" => {
                let lang = tree_sitter_javascript::LANGUAGE.into();
                out.extend(empty_catches_ts_js(&content, &rel, &lang));
                out.extend(promise_empty_catches_ts_js(&content, &rel, &lang));
                out.extend(tautological_tests_ts_js(&content, &rel, &lang));
            }
            "py" => {
                out.extend(empty_excepts_python(&content, &rel));
                out.extend(tautological_tests_python(&content, &rel));
            }
            "rs" => {
                out.extend(error_swallowing_rust(&content, &rel));
                out.extend(tautological_tests_rust(&content, &rel));
            }
            "go" => {
                out.extend(error_swallowing_go(&content, &rel));
                out.extend(tautological_tests_go(&content, &rel));
            }
            "java" => {
                out.extend(empty_catches_java(&content, &rel));
                out.extend(tautological_tests_java(&content, &rel));
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

// ── Per-language detectors (added for richer coverage) ──────────

fn empty_catches_java(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_java::LANGUAGE.into();
    let query_src = r#"(catch_clause body: (block) @body)"#;
    run_ast_query(content, &language, query_src, |node| {
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
            reason: "empty Java catch block silently swallows the exception".into(),
        })
    })
}

fn promise_empty_catches_ts_js(
    content: &str,
    rel: &str,
    language: &tree_sitter::Language,
) -> Vec<SlopFix> {
    // Match `.catch(() => {})` and `.catch((e) => {})` and `.catch(function () {})`
    // — Promise rejections silently dropped.
    let query_src = r#"
    (call_expression
      function: (member_expression
        property: (property_identifier) @prop (#eq? @prop "catch"))
      arguments: (arguments
        [(arrow_function body: (statement_block) @body)
         (function_expression body: (statement_block) @body)])) @call
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
    let body_idx = query.capture_index_for_name("body");

    let mut out = Vec::new();
    while let Some(m) = matches.next() {
        let mut call_node = None;
        let mut body_node = None;
        for cap in m.captures {
            if Some(cap.index) == call_idx {
                call_node = Some(cap.node);
            } else if Some(cap.index) == body_idx {
                body_node = Some(cap.node);
            }
        }
        let (Some(call), Some(body)) = (call_node, body_node) else {
            continue;
        };
        if body.named_child_count() != 0 {
            continue;
        }
        let start = (call.start_position().row as u32) + 1;
        let end = (call.end_position().row as u32) + 1;
        out.push(SlopFix {
            action: SlopAction::DeleteLines {
                path: rel.to_string(),
                lines: [start, end],
            },
            category: SlopCategory::EmptyCatch,
            reason: "Promise .catch() with empty handler silently swallows rejections".into(),
        });
    }
    out
}

fn error_swallowing_rust(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_rust::LANGUAGE.into();
    let mut out = Vec::new();

    // `let _ = expr;` — discards the result. tree-sitter-rust elides
    // the `_` pattern entirely (it's the default), so we can't query
    // for it as a typed node. Instead match every let_declaration
    // with a value and string-check the text starts with `let _`.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(let_declaration value: (_)) @body"#,
        |node| {
            let text = node.utf8_text(content.as_bytes()).ok()?;
            // Tolerate whitespace between `let` and `_`.
            let trimmed = text.trim_start_matches("let").trim_start();
            if !trimmed.starts_with('_') {
                return None;
            }
            // Reject `let _x = …` — that's a normal binding starting
            // with underscore-prefixed identifier, not a discard.
            let after = trimmed[1..].chars().next();
            if let Some(c) = after
                && (c.is_ascii_alphanumeric() || c == '_')
            {
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
                reason: "Rust `let _ = …` discards the value (often an unhandled Result)".into(),
            })
        },
    ));

    // `expr.ok();` as a statement — Result::ok() called for side
    // effect, dropping the Err arm.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(expression_statement (call_expression
            function: (field_expression field: (field_identifier) @field
                       (#eq? @field "ok"))
            arguments: (arguments))) @body"#,
        |node| {
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::EmptyCatch,
                reason: "Rust `.ok();` as a statement drops the Err variant".into(),
            })
        },
    ));

    // `if let Err(_) = expr {}` empty body
    out.extend(run_ast_query(
        content,
        &language,
        r#"(if_expression
            condition: (let_condition
                pattern: (tuple_struct_pattern type: (identifier) @ty (#eq? @ty "Err")))
            consequence: (block) @body)"#,
        |node| {
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
                reason: "Rust `if let Err(_) = … {}` empty body silently ignores errors".into(),
            })
        },
    ));

    out
}

fn error_swallowing_go(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_go::LANGUAGE.into();
    let mut out = Vec::new();

    // Empty `if err != nil { }` block — error checked but not handled.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(if_statement
            condition: (binary_expression
                left: (identifier) @lhs (#eq? @lhs "err")
                operator: "!="
                right: (nil))
            consequence: (block) @body)"#,
        |node| {
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
                reason: "Go `if err != nil { }` empty block silently ignores the error".into(),
            })
        },
    ));

    // `_ = expr` where expr looks error-returning — common idiom for
    // intentionally discarding errors.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(assignment_statement
            left: (expression_list (identifier) @lhs (#eq? @lhs "_"))) @body"#,
        |node| {
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::EmptyCatch,
                reason: "Go `_ = …` explicitly discards the value (often an unchecked error)"
                    .into(),
            })
        },
    ));

    out
}

fn tautological_tests_rust(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_rust::LANGUAGE.into();
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();

    // assert!(true) / debug_assert!(true) — always passes.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(macro_invocation
            macro: (identifier) @name
            (#match? @name "^(assert|debug_assert)$")
            (token_tree . (boolean_literal) @arg .)) @body"#,
        |node| {
            let arg_text = node
                .descendant_for_byte_range(node.start_byte(), node.end_byte())
                .and_then(|_| node.utf8_text(content.as_bytes()).ok())
                .unwrap_or("");
            if !arg_text.contains("true") {
                return None;
            }
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: "Rust `assert!(true)` always passes".into(),
            })
        },
    ));

    // assert_eq!(x, x) / assert_ne!(x, !x) — same expression both sides
    // captured via macro_invocation token_tree pairs. tree-sitter-rust
    // exposes the macro args as raw tokens, so we string-match the
    // arg text.
    out.extend(detect_rust_assert_eq_pairs(content, rel, &mut seen));

    out
}

fn detect_rust_assert_eq_pairs(
    content: &str,
    rel: &str,
    seen: &mut HashSet<(u32, u32)>,
) -> Vec<SlopFix> {
    let language = tree_sitter_rust::LANGUAGE.into();
    let mut out = Vec::new();
    let _ = run_ast_query(
        content,
        &language,
        r#"(macro_invocation
            macro: (identifier) @name
            (#match? @name "^(assert_eq|assert_ne|debug_assert_eq|debug_assert_ne)$")) @body"#,
        |node| {
            let macro_text = node.utf8_text(content.as_bytes()).ok()?;
            // Strip "name!(" prefix and trailing ")"
            let open = macro_text.find('(')?;
            let close = macro_text.rfind(')')?;
            if close <= open + 1 {
                return None;
            }
            let inner = &macro_text[open + 1..close];
            // Split top-level commas only (depth 0).
            let parts = split_top_commas(inner);
            if parts.len() < 2 {
                return None;
            }
            let lhs = parts[0].trim();
            let rhs = parts[1].trim();
            if !is_literal_or_identifier(lhs) {
                return None;
            }
            if lhs != rhs {
                return None;
            }
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: format!(
                    "Rust `{}!({lhs}, {rhs})` always passes",
                    macro_name(macro_text)
                ),
            });
            None
        },
    );
    out
}

fn macro_name(macro_text: &str) -> &str {
    macro_text.split('!').next().unwrap_or("assert")
}

fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn tautological_tests_python(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_python::LANGUAGE.into();
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();

    // `assert <literal>` — assert True / assert 1 / assert "x" etc.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(assert_statement [(true) (integer) (string)] @body)"#,
        |node| {
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: "Python `assert <truthy literal>` always passes".into(),
            })
        },
    ));

    // `assert x == x` — same expression both sides
    out.extend(run_ast_query(
        content,
        &language,
        r#"(assert_statement
            (comparison_operator
                (_) @lhs
                (_) @rhs)) @body"#,
        |node| {
            // Walk into the comparison, compare lhs and rhs text.
            let cmp = node.named_child(0)?;
            if cmp.named_child_count() < 2 {
                return None;
            }
            let lhs = cmp.named_child(0)?.utf8_text(content.as_bytes()).ok()?;
            let rhs = cmp.named_child(1)?.utf8_text(content.as_bytes()).ok()?;
            if lhs.trim() != rhs.trim() || !is_literal_or_identifier(lhs.trim()) {
                return None;
            }
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: format!("Python `assert {lhs} == {rhs}` always passes"),
            })
        },
    ));

    // `self.assertEqual(x, x)`, `self.assertTrue(True)`, etc.
    out.extend(detect_python_unittest_assert(content, rel, &mut seen));

    out
}

fn detect_python_unittest_assert(
    content: &str,
    rel: &str,
    seen: &mut HashSet<(u32, u32)>,
) -> Vec<SlopFix> {
    let language = tree_sitter_python::LANGUAGE.into();
    let mut out = Vec::new();
    let _ = run_ast_query(
        content,
        &language,
        r#"(call
            function: (attribute attribute: (identifier) @method
                (#match? @method "^(assertEqual|assertEquals|assertIs|assertSame|assertTrue|assertFalse)$"))
            arguments: (argument_list) @args) @body"#,
        |node| {
            let method = node
                .named_child(0)?
                .named_child(1)?
                .utf8_text(content.as_bytes())
                .ok()?;
            let args = node.child_by_field_name("arguments")?;
            // Slice args excluding the parens: "(a, b, c)" -> "a, b, c"
            let args_text = args.utf8_text(content.as_bytes()).ok()?;
            let trimmed = args_text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            let parts = split_top_commas(trimmed);
            let tautological = match method {
                "assertTrue" => parts.first().map(|s| s.trim() == "True").unwrap_or(false),
                "assertFalse" => parts.first().map(|s| s.trim() == "False").unwrap_or(false),
                _ => {
                    if parts.len() < 2 {
                        false
                    } else {
                        let lhs = parts[0].trim();
                        let rhs = parts[1].trim();
                        lhs == rhs && is_literal_or_identifier(lhs)
                    }
                }
            };
            if !tautological {
                return None;
            }
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: format!("Python `{method}({args_text})` always passes"),
            });
            None
        },
    );
    out
}

fn tautological_tests_java(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_java::LANGUAGE.into();
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    let _ = run_ast_query(
        content,
        &language,
        r#"(method_invocation
            name: (identifier) @method
            (#match? @method "^(assertEquals|assertSame|assertTrue|assertFalse|assertNotNull|assertNull)$")
            arguments: (argument_list) @args) @body"#,
        |node| {
            let method = node
                .child_by_field_name("name")?
                .utf8_text(content.as_bytes())
                .ok()?;
            let args = node.child_by_field_name("arguments")?;
            let args_text = args.utf8_text(content.as_bytes()).ok()?;
            let trimmed = args_text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            let parts = split_top_commas(trimmed);
            let tautological = match method {
                "assertTrue" => parts.iter().any(|s| s.trim() == "true"),
                "assertFalse" => parts.iter().any(|s| s.trim() == "false"),
                "assertEquals" | "assertSame" => {
                    if parts.len() < 2 {
                        false
                    } else {
                        let lhs = parts[0].trim();
                        let rhs = parts[1].trim();
                        lhs == rhs && is_literal_or_identifier(lhs)
                    }
                }
                "assertNotNull" => parts
                    .first()
                    .map(|s| {
                        let t = s.trim();
                        t.starts_with('"') || t == "Boolean.TRUE" || t == "Boolean.FALSE"
                    })
                    .unwrap_or(false),
                "assertNull" => parts.first().map(|s| s.trim() == "null").unwrap_or(false),
                _ => false,
            };
            if !tautological {
                return None;
            }
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: format!("Java `{method}{args_text}` always passes"),
            });
            None
        },
    );
    out
}

fn tautological_tests_go(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_go::LANGUAGE.into();
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    // testify: `assert.Equal(t, x, x)`, `assert.True(t, true)`, etc.
    let _ = run_ast_query(
        content,
        &language,
        r#"(call_expression
            function: (selector_expression
                operand: (identifier) @pkg (#match? @pkg "^(assert|require)$")
                field: (field_identifier) @method
                    (#match? @method "^(Equal|NotEqual|Same|True|False|Nil|NotNil|Equals)$"))
            arguments: (argument_list) @args) @body"#,
        |node| {
            let method_node = node
                .child_by_field_name("function")?
                .child_by_field_name("field")?;
            let method = method_node.utf8_text(content.as_bytes()).ok()?;
            let args = node.child_by_field_name("arguments")?;
            let args_text = args.utf8_text(content.as_bytes()).ok()?;
            let trimmed = args_text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            // testify args: (t, expected, actual) or (t, value)
            let parts = split_top_commas(trimmed);
            let tautological = match method {
                "True" => parts.get(1).map(|s| s.trim() == "true").unwrap_or(false),
                "False" => parts.get(1).map(|s| s.trim() == "false").unwrap_or(false),
                "Nil" => parts.get(1).map(|s| s.trim() == "nil").unwrap_or(false),
                "NotNil" => parts
                    .get(1)
                    .map(|s| {
                        let t = s.trim();
                        t.starts_with('"') || t == "true" || t == "false"
                    })
                    .unwrap_or(false),
                "Equal" | "Equals" | "Same" => {
                    if parts.len() < 3 {
                        false
                    } else {
                        let lhs = parts[1].trim();
                        let rhs = parts[2].trim();
                        lhs == rhs && is_literal_or_identifier(lhs)
                    }
                }
                _ => false,
            };
            if !tautological {
                return None;
            }
            let start = (node.start_position().row as u32) + 1;
            let end = (node.end_position().row as u32) + 1;
            if !seen.insert((start, end)) {
                return None;
            }
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::TautologicalTest,
                reason: format!("Go testify `{method}{args_text}` always passes"),
            });
            None
        },
    );
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

    /// Convenience: build a TempDir + write all files in one call.
    /// Used by per-language tests below.
    fn make_repo(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (rel, content) in files {
            write(&dir, rel, content);
        }
        dir
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

    // ── Per-language coverage (rich detector additions) ──

    #[test]
    fn detects_empty_catch_in_java() {
        let dir = make_repo(&[(
            "A.java",
            "class A { void m() { try { } catch (Exception e) {} } }\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(fixes.iter().any(|f| f.category == SlopCategory::EmptyCatch));
    }

    #[test]
    fn detects_promise_empty_catch_in_typescript() {
        let dir = make_repo(&[("a.ts", "doThing().catch(() => {});\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch && f.reason.contains("Promise")),
            "expected Promise .catch() flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_promise_catch_with_handler_body() {
        let dir = make_repo(&[("a.ts", "doThing().catch((e) => { console.error(e); });\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("Promise")),
            "should not flag non-empty catch handler"
        );
    }

    #[test]
    fn detects_rust_let_underscore_discard() {
        let dir = make_repo(&[("lib.rs", "fn f() {\n    let _ = some_call();\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch && f.reason.contains("let _")),
            "expected `let _ = …` flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_rust_dot_ok_statement() {
        let dir = make_repo(&[("lib.rs", "fn f() {\n    some_call().ok();\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes.iter().any(|f| f.reason.contains(".ok();")),
            "expected `.ok();` flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_go_empty_err_check() {
        let dir = make_repo(&[(
            "x.go",
            "package x\nfunc f() {\n    err := call()\n    if err != nil {\n    }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch && f.reason.contains("err != nil")),
            "expected empty err-check flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_go_err_check_with_handler() {
        let dir = make_repo(&[(
            "x.go",
            "package x\nfunc f() {\n    err := call()\n    if err != nil {\n        return\n    }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("err != nil")),
            "should not flag handled err"
        );
    }

    #[test]
    fn detects_rust_assert_eq_with_identical_literals() {
        let dir = make_repo(&[("tests.rs", "#[test]\nfn t() {\n    assert_eq!(1, 1);\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected assert_eq!(1,1) flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_rust_assert_macro_with_true() {
        let dir = make_repo(&[("tests.rs", "#[test]\nfn t() {\n    assert!(true);\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected assert!(true) flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_rust_assert_eq_with_different_args() {
        let dir = make_repo(&[(
            "tests.rs",
            "#[test]\nfn t() {\n    assert_eq!(compute(), 42);\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "should not flag real assertion"
        );
    }

    #[test]
    fn detects_python_assert_x_eq_x() {
        let dir = make_repo(&[("t.py", "def test_thing():\n    x = 1\n    assert x == x\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected `assert x == x` flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_python_unittest_assert_equal_with_identical_args() {
        let dir = make_repo(&[(
            "t.py",
            "import unittest\nclass T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(1, 1)\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected assertEqual(1,1) flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_java_assert_equals_with_identical_args() {
        let dir = make_repo(&[(
            "T.java",
            "class T {\n  void t() {\n    assertEquals(1, 1);\n  }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected assertEquals(1,1) flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_java_assert_true_with_literal_true() {
        let dir = make_repo(&[(
            "T.java",
            "class T {\n  void t() {\n    assertTrue(true);\n  }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected assertTrue(true) flag; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_go_testify_equal_with_identical_args() {
        let dir = make_repo(&[(
            "x_test.go",
            "package x\nimport \"testing\"\nfunc TestX(t *testing.T) {\n    assert.Equal(t, 1, 1)\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::TautologicalTest),
            "expected testify Equal(t,1,1) flag; got {:?}",
            fixes
        );
    }
}
