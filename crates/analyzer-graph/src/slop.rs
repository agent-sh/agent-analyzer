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

impl SlopAction {
    /// Target file for this action. Every variant has a path, so
    /// this is total. Used by the CLI `--files` filter and consumers
    /// that group / dedupe by target path.
    pub fn path(&self) -> &str {
        match self {
            SlopAction::DeleteFile { path }
            | SlopAction::DeleteLines { path, .. }
            | SlopAction::ReplaceLines { path, .. } => path.as_str(),
        }
    }
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
    /// A function whose entire body is a single call to another
    /// function with the same arguments — a wrapper that adds no
    /// transformation, validation, or composition. Common AI-slop
    /// pattern (`fn get_user(id) { fetch_user(id) }`) and a clear
    /// candidate for inlining at the call site.
    PassthroughWrapper,
    /// A condition that always evaluates to the same value: `if true`,
    /// `if false`, `if 1 == 1`, `if x != null && x == null`. Either
    /// the branch is dead code or the surrounding logic is wrong.
    AlwaysTrueCondition,
    /// A comment block whose content is valid code in the surrounding
    /// language. Three or more lines that re-parse cleanly with no
    /// ERROR nodes and contain at least one substantive statement
    /// (function, class, assignment, control flow) — a strong signal
    /// the code was commented out rather than explained in prose.
    CommentedOutCode,
    /// A `#[allow(dead_code)]` / `#[allow(unused)]` attribute on a
    /// symbol that the import graph proves IS being used. The
    /// suppression was correct at some point but the symbol became
    /// reachable again and the annotation was never removed. Keeping
    /// stale suppressions around blinds the real dead-code lint.
    /// Rust-only; other languages have different suppression shapes
    /// (`@ts-ignore`, `# noqa`, `@SuppressWarnings`) that serve
    /// different purposes.
    StaleSuppression,
}

/// Confidence in a fix, on a 0.0-1.0 scale.
///
/// - **0.95+** — safe for direct apply by a Haiku-tier agent. Shape
///   is mechanical and the action is unambiguous.
/// - **0.80-0.95** — apply with shape-confirm. Detector is sure but
///   the fix may need a human-readable comment added.
/// - **0.60-0.80** — human review recommended. Detector is right
///   often but not always (e.g. orphan-export when import graph is
///   incomplete for a language).
/// - **< 0.60** — flagged only; do not auto-apply.
///
/// Consumers can filter by threshold:
///
/// ```ignore
/// let safe = result.fixes.iter().filter(|f| f.confidence >= 0.95);
/// ```
pub type Confidence = f32;

/// Default confidence for each category. Detectors override this
/// when they have additional context (e.g. orphan-export drops to
/// 0.7 when the import graph completeness is uncertain). Centralized
/// so consumers see a consistent default per category.
pub fn default_confidence(category: SlopCategory) -> Confidence {
    match category {
        // Mechanical, no judgment needed
        SlopCategory::TrackedArtifact => 0.97,
        SlopCategory::EmptyCatch => 0.95,
        SlopCategory::TautologicalTest => 0.95,
        SlopCategory::AlwaysTrueCondition => 0.92,
        // Pre-located but pattern-suggested rather than dead-certain
        SlopCategory::StaleCiConfig => 0.90,
        SlopCategory::DuplicateTooling => 0.85,
        SlopCategory::PassthroughWrapper => 0.85,
        SlopCategory::CommentedOutCode => 0.85,
        SlopCategory::StaleSuppression => 0.90,
        // Sensitive to import-graph completeness; recommend review
        SlopCategory::OrphanExport => 0.75,
    }
}

/// One finding from the `slop-fixes` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlopFix {
    #[serde(flatten)]
    pub action: SlopAction,
    pub category: SlopCategory,
    pub reason: String,
    /// 0.0-1.0 confidence. See [`default_confidence`] for the typical
    /// value per category. Defaults to category default when omitted
    /// from input JSON, so older consumers reading new artifacts
    /// don't need to deal with `Option<f32>`.
    #[serde(default = "default_confidence_full")]
    pub confidence: Confidence,
}

fn default_confidence_full() -> Confidence {
    // Used by serde when deserializing artifacts that pre-date the
    // confidence field. The category-aware default isn't reachable
    // here (we'd need access to the surrounding struct) so we return
    // a conservative middle value. Real per-category defaults come
    // from the in-process detectors via [`default_confidence`].
    0.80
}

/// Aggregate output piped to the deslop agent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlopFixesResult {
    pub fixes: Vec<SlopFix>,
    /// Convenience grouping: same `fixes` content, partitioned by
    /// target file path. Lets consumers apply edits in batch per
    /// file (one open/edit/save cycle for N fixes against the same
    /// file) rather than re-opening the file once per fix.
    /// Populated by [`group_by_file`]; an alphabetically-sorted
    /// `Vec<FileFixes>` keeps output deterministic and diffable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_file: Vec<FileFixes>,
}

/// All fixes targeting one source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFixes {
    pub path: String,
    pub fixes: Vec<SlopFix>,
}

/// Group flat fixes by their target path, sorted alphabetically by
/// path. File-level deletions (`tracked-artifact`) and per-line
/// fixes share the same group when they hit the same file.
pub fn group_by_file(fixes: &[SlopFix]) -> Vec<FileFixes> {
    use std::collections::BTreeMap;
    let mut by_path: BTreeMap<String, Vec<SlopFix>> = BTreeMap::new();
    for fix in fixes {
        let path = match &fix.action {
            SlopAction::DeleteFile { path }
            | SlopAction::DeleteLines { path, .. }
            | SlopAction::ReplaceLines { path, .. } => path.clone(),
        };
        by_path.entry(path).or_default().push(fix.clone());
    }
    by_path
        .into_iter()
        .map(|(path, fixes)| FileFixes { path, fixes })
        .collect()
}

/// Run every detector and aggregate.
///
/// `repo_root` is the working tree (used by path-based detectors).
/// `map` is the loaded repo-intel artifact (provides import graph,
/// project metadata, etc).
///
/// Findings are filtered against per-line suppression comments
/// (`// agentsys-ignore: <category>` or `# agentsys-ignore: <category>`)
/// before being returned — see [`apply_suppressions`].
pub fn slop_fixes(repo_root: &Path, map: &RepoIntelData) -> SlopFixesResult {
    let mut fixes = Vec::new();

    fixes.extend(tracked_artifacts(repo_root));
    fixes.extend(stale_ci_configs(repo_root));
    fixes.extend(duplicate_tooling(repo_root));
    fixes.extend(orphan_exports(map));
    fixes.extend(ast_findings(repo_root));
    fixes.extend(stale_suppressions_rust(repo_root, map));

    let fixes = apply_suppressions(repo_root, fixes);
    let by_file = group_by_file(&fixes);
    SlopFixesResult { fixes, by_file }
}

/// Drop fixes whose target line is annotated with an
/// `agentsys-ignore: <category>` comment on the same line or the line
/// immediately above. Comment forms recognized:
///
///   - `// agentsys-ignore: orphan-export`           (Rust / TS / JS / Java / Go / C)
///   - `# agentsys-ignore: empty-catch`              (Python / Shell / TOML)
///   - `// agentsys-ignore: tracked-artifact, orphan-export`  (comma-separated list)
///   - `// agentsys-ignore-all`                      (suppresses every category)
///
/// File-deletion fixes (`tracked-artifact`, `stale-ci-config`,
/// `duplicate-tooling`) are suppressible by an `agentsys-ignore` comment
/// in the file's first 5 lines (file-header convention).
///
/// Per-line fixes (`orphan-export`, `empty-catch`, `tautological-test`)
/// are suppressible by a comment on the fix line OR the line above —
/// matching how `eslint-disable-next-line` and `// noqa` work in
/// other linters.
fn apply_suppressions(repo_root: &Path, fixes: Vec<SlopFix>) -> Vec<SlopFix> {
    use std::collections::HashMap;
    // Cache file → suppression map across fixes from the same file
    // so we don't re-read+re-parse the source for each finding.
    let mut cache: HashMap<String, Option<FileSuppressions>> = HashMap::new();

    fixes
        .into_iter()
        .filter(|fix| {
            let path = fix_target_path(fix);
            if path.is_none() {
                return true;
            }
            let path = path.unwrap();
            // Avoid the per-call String allocation that `entry()` would
            // force on every cache hit; only allocate when inserting.
            if !cache.contains_key(path) {
                cache.insert(path.to_string(), FileSuppressions::load(repo_root, path));
            }
            let Some(supp) = cache.get(path).and_then(|o| o.as_ref()) else {
                return true;
            };
            !supp.suppresses(fix)
        })
        .collect()
}

fn fix_target_path(fix: &SlopFix) -> Option<&str> {
    match &fix.action {
        SlopAction::DeleteFile { path } => Some(path.as_str()),
        SlopAction::DeleteLines { path, .. } => Some(path.as_str()),
        SlopAction::ReplaceLines { path, .. } => Some(path.as_str()),
    }
}

/// Per-file suppression index: `agentsys-ignore` comments parsed once
/// per file and reused across every fix targeting that file.
struct FileSuppressions {
    /// Header suppressions (lines 1-5) apply to file-level actions.
    header_categories: std::collections::HashSet<String>,
    /// Per-line suppressions: line N suppresses fixes on N or N+1.
    line_categories: std::collections::HashMap<u32, std::collections::HashSet<String>>,
    /// `agentsys-ignore-all` line numbers — suppress every category.
    line_all: std::collections::HashSet<u32>,
    /// Header-scope `agentsys-ignore-all` flag — suppresses every category
    /// for any fix targeting this file.
    header_all: bool,
}

impl FileSuppressions {
    fn load(repo_root: &Path, rel_path: &str) -> Option<Self> {
        let abs = repo_root.join(rel_path);
        let content = std::fs::read_to_string(&abs).ok()?;
        let mut s = FileSuppressions {
            header_categories: Default::default(),
            line_categories: Default::default(),
            line_all: Default::default(),
            header_all: false,
        };
        for (idx, line) in content.lines().enumerate() {
            let line_no = (idx as u32) + 1;
            let in_header = line_no <= 5;

            // Find any agentsys-ignore directive on this line, whether
            // the line is a comment-only line OR a code line with a
            // trailing comment (`catch {} // agentsys-ignore: …`).
            // We scan for the directive substring directly, then walk
            // back to confirm it's preceded by a recognized comment
            // marker (`//`, `#`, `--`). This avoids the trim_start +
            // strip_prefix path which only handled comment-only lines.
            let Some(directive) = find_agentsys_directive(line) else {
                continue;
            };

            if directive == "agentsys-ignore-all" || directive.starts_with("agentsys-ignore-all ") {
                if in_header {
                    s.header_all = true;
                }
                s.line_all.insert(line_no);
                continue;
            }
            if let Some(rest) = directive.strip_prefix("agentsys-ignore:") {
                // Strip trailing comments / whitespace per category so
                // `// agentsys-ignore: empty-catch trailing notes` works.
                let cats = rest
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .filter_map(|c| c.split_whitespace().next().map(str::to_string))
                    .filter(|c| !c.is_empty());
                let entry = s.line_categories.entry(line_no).or_default();
                for cat in cats {
                    if in_header {
                        s.header_categories.insert(cat.clone());
                    }
                    entry.insert(cat);
                }
            }
        }
        Some(s)
    }

    fn suppresses(&self, fix: &SlopFix) -> bool {
        let cat_str = category_kebab(fix.category);
        match &fix.action {
            SlopAction::DeleteFile { .. } => {
                self.header_all || self.header_categories.contains(cat_str)
            }
            SlopAction::DeleteLines { lines, .. } | SlopAction::ReplaceLines { lines, .. } => {
                let target_line = lines[0];
                // Lookback window matches the `eslint-disable-next-line`
                // convention but extends 3 lines to handle Python's
                // try/except where the directive sits above `try:` and
                // the offending body line is two below — and Rust's
                // multi-line attribute prefixes (`#[cfg(test)]` then
                // `#[allow(...)]` then the function).
                for n in 0..=3u32 {
                    let candidate = target_line.saturating_sub(n);
                    if candidate == 0 {
                        break;
                    }
                    if self.line_all.contains(&candidate) {
                        return true;
                    }
                    if self
                        .line_categories
                        .get(&candidate)
                        .map(|set| set.contains(cat_str))
                        .unwrap_or(false)
                    {
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// Locate an `agentsys-ignore...` directive on a single source line.
/// Handles both comment-only lines (`// agentsys-ignore: …`) and
/// inline trailing comments (`catch {} // agentsys-ignore: …`) by
/// scanning for the directive substring and confirming a recognized
/// comment marker (`//`, `#`, `--`) appears before it.
fn find_agentsys_directive(line: &str) -> Option<&str> {
    let pos = line.find("agentsys-ignore")?;
    // Walk back through whitespace to the comment marker.
    let prefix = line[..pos].trim_end();
    let valid_marker = prefix.ends_with("//") || prefix.ends_with('#') || prefix.ends_with("--");
    if !valid_marker {
        return None;
    }
    Some(line[pos..].trim_end())
}

/// Map a [`SlopCategory`] to its kebab-case string. Avoids the
/// per-call `serde_json::to_value` allocation flagged by reviewers —
/// this is hit on every fix during suppression filtering.
fn category_kebab(c: SlopCategory) -> &'static str {
    match c {
        SlopCategory::TrackedArtifact => "tracked-artifact",
        SlopCategory::StaleCiConfig => "stale-ci-config",
        SlopCategory::DuplicateTooling => "duplicate-tooling",
        SlopCategory::OrphanExport => "orphan-export",
        SlopCategory::EmptyCatch => "empty-catch",
        SlopCategory::TautologicalTest => "tautological-test",
        SlopCategory::PassthroughWrapper => "passthrough-wrapper",
        SlopCategory::AlwaysTrueCondition => "always-true-condition",
        SlopCategory::CommentedOutCode => "commented-out-code",
        SlopCategory::StaleSuppression => "stale-suppression",
    }
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
                confidence: default_confidence(SlopCategory::TrackedArtifact),
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
                confidence: default_confidence(SlopCategory::StaleCiConfig),
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
            confidence: default_confidence(SlopCategory::DuplicateTooling),
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
            confidence: default_confidence(SlopCategory::DuplicateTooling),
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
                confidence: default_confidence(SlopCategory::DuplicateTooling),
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

    // External entry points: Cargo bins/tests/benches/examples,
    // npm bin/scripts, framework configs (Docusaurus, Next.js, …),
    // Python __main__.py. Files referenced this way look orphan to
    // the import graph but are absolutely used. The artifact carries
    // a precomputed list (Phase 3.5 of init); old artifacts that
    // pre-date this field fall back to the per-path heuristic below.
    let entry_point_paths: HashSet<&str> = map
        .entry_points
        .as_deref()
        .map(|eps| eps.iter().map(|e| e.path.as_str()).collect())
        .unwrap_or_default();

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
        // Skip files registered as entry points by Cargo manifests,
        // package.json bin/scripts, pyproject scripts, framework
        // configs, or AST-detected `main` functions. Falls back to a
        // path heuristic when the artifact pre-dates the cached list.
        if entry_point_paths.contains(file_path.as_str()) || looks_like_entry_point(file_path) {
            continue;
        }

        for export in &file_symbols.exports {
            // Filter to top-level kinds. Field, EnumVariant, Property
            // are member symbols — flagging them as "orphan exports"
            // is wrong (their parent struct/enum/class is the actual
            // export; the member just goes wherever the parent goes).
            if !is_top_level_kind(&export.kind) {
                continue;
            }
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: file_path.clone(),
                    lines: [export.line as u32, export.line as u32],
                },
                category: SlopCategory::OrphanExport,
                confidence: default_confidence(SlopCategory::OrphanExport),
                reason: format!(
                    "exported {kind} `{name}` — file has zero importers in the project graph",
                    kind = symbol_kind_name(&export.kind),
                    name = export.name
                ),
            });
        }
    }
    out
}

fn is_top_level_kind(k: &analyzer_core::types::SymbolKind) -> bool {
    use analyzer_core::types::SymbolKind::*;
    matches!(
        k,
        Function | Class | Struct | Trait | Interface | Enum | Constant | TypeAlias | Module
    )
}

fn symbol_kind_name(k: &analyzer_core::types::SymbolKind) -> &'static str {
    use analyzer_core::types::SymbolKind::*;
    match k {
        Function => "function",
        Class => "class",
        Struct => "struct",
        Trait => "trait",
        Interface => "interface",
        Enum => "enum",
        Constant => "constant",
        TypeAlias => "type alias",
        Module => "module",
        Field => "field",
        EnumVariant => "variant",
        Property => "property",
    }
}

/// Recognize intentional `let _ = <expr>;` shapes that are NOT slop.
///
/// These all match the pattern textually but represent code the
/// author meant to write that way:
///
///   * `let _ = my_pattern();` / `let _ = my_regex();` — lazy-static
///     getters being warmed up at module init or test setup.
///   * `let _ = thread::spawn(…)` / `tokio::spawn` / `task::spawn` —
///     fire-and-forget concurrency. The discarded JoinHandle is the
///     point of the line.
///   * `let _ = fs::remove_file(…)` / `remove_dir(…)` / `flush()` —
///     best-effort cleanup; failure is acceptable.
///   * `let _ = mutex.lock()` / `rwlock.read()` / `rwlock.write()` —
///     hold a guard for the rest of the scope.
///   * `let _ = identifier;` (single bare identifier) — explicit drop
///     to silence "unused variable" without renaming the binding.
///
/// Returns true when the value matches one of these shapes; the
/// caller skips emitting a slop fix in that case.
fn is_intentional_let_underscore_discard(value_text: &str) -> bool {
    let trimmed = value_text.trim();

    // Lazy-static warmup: `xxx_pattern()` or `xxx_regex()` with no
    // args. Common in agnix and similar Rust codebases that pre-warm
    // lazy_static / OnceLock regexes during init or test setup.
    if let Some(call) = trimmed.strip_suffix("()") {
        if call.ends_with("_pattern")
            || call.ends_with("_regex")
            || call.ends_with("_patterns")
            || call.ends_with("_regexes")
        {
            return true;
        }
    }

    // Fire-and-forget thread/task spawns.
    if trimmed.starts_with("thread::spawn")
        || trimmed.starts_with("tokio::spawn")
        || trimmed.starts_with("task::spawn")
        || trimmed.starts_with("tokio::task::spawn")
        || trimmed.starts_with("std::thread::spawn")
    {
        return true;
    }

    // Best-effort cleanup / flushes. The Result is intentionally
    // discarded because failure is non-fatal.
    if trimmed.starts_with("fs::remove_")
        || trimmed.starts_with("std::fs::remove_")
        || trimmed.contains("::Write::flush")
        || trimmed.contains(".flush()")
    {
        return true;
    }

    // Lock acquisition for scope-tied guard. The result IS the value
    // being held, not "discarded".
    if trimmed.ends_with(".lock()")
        || trimmed.ends_with(".lock().unwrap()")
        || trimmed.ends_with(".read()")
        || trimmed.ends_with(".write()")
        || trimmed.ends_with(".read().unwrap()")
        || trimmed.ends_with(".write().unwrap()")
    {
        return true;
    }

    // Explicit drop of an existing binding — `let _ = outcome;` after
    // a pattern match. Single bare identifier.
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return true;
    }

    false
}

/// Path-based heuristic for "this file is dedicated to tests / benches"
/// — those files freely use `let _ = …`, `.ok();`, and `.unwrap()` and
/// flagging them as slop generates noise without value.
///
/// Does NOT catch `#[cfg(test)] mod tests { … }` blocks inside production
/// `.rs` files; those would need AST-level cfg-attribute walking. The
/// path-based filter handles the common conventions:
///   - `tests/*` and `benches/*` directories
///   - `*_test.rs`, `*_tests.rs`, `tests.rs` filename suffixes
fn is_rust_test_file(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase().replace('\\', "/");
    if lower.starts_with("tests/") || lower.contains("/tests/") {
        return true;
    }
    if lower.starts_with("benches/") || lower.contains("/benches/") {
        return true;
    }
    if lower.ends_with("_test.rs") || lower.ends_with("_tests.rs") || lower.ends_with("/tests.rs") {
        return true;
    }
    false
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
                out.extend(always_true_conditions_ts_js(&content, &rel, &lang));
                out.extend(passthrough_wrappers_ts_js(&content, &rel, &lang));
                out.extend(commented_out_code(
                    &content,
                    &rel,
                    &lang,
                    CommentStyle::CStyle,
                ));
            }
            "js" | "jsx" | "mjs" | "cjs" => {
                let lang = tree_sitter_javascript::LANGUAGE.into();
                out.extend(empty_catches_ts_js(&content, &rel, &lang));
                out.extend(promise_empty_catches_ts_js(&content, &rel, &lang));
                out.extend(tautological_tests_ts_js(&content, &rel, &lang));
                out.extend(always_true_conditions_ts_js(&content, &rel, &lang));
                out.extend(passthrough_wrappers_ts_js(&content, &rel, &lang));
                out.extend(commented_out_code(
                    &content,
                    &rel,
                    &lang,
                    CommentStyle::CStyle,
                ));
            }
            "py" => {
                out.extend(empty_excepts_python(&content, &rel));
                out.extend(tautological_tests_python(&content, &rel));
                out.extend(always_true_conditions_python(&content, &rel));
                out.extend(passthrough_wrappers_python(&content, &rel));
                let lang = tree_sitter_python::LANGUAGE.into();
                out.extend(commented_out_code(
                    &content,
                    &rel,
                    &lang,
                    CommentStyle::Hash,
                ));
            }
            "rs" => {
                if !is_rust_test_file(&rel) {
                    out.extend(error_swallowing_rust(&content, &rel));
                    out.extend(passthrough_wrappers_rust(&content, &rel));
                }
                out.extend(tautological_tests_rust(&content, &rel));
                out.extend(always_true_conditions_rust(&content, &rel));
                let lang = tree_sitter_rust::LANGUAGE.into();
                out.extend(commented_out_code(
                    &content,
                    &rel,
                    &lang,
                    CommentStyle::CStyle,
                ));
            }
            "go" => {
                out.extend(error_swallowing_go(&content, &rel));
                out.extend(tautological_tests_go(&content, &rel));
                out.extend(always_true_conditions_go(&content, &rel));
                out.extend(passthrough_wrappers_go(&content, &rel));
                let lang = tree_sitter_go::LANGUAGE.into();
                out.extend(commented_out_code(
                    &content,
                    &rel,
                    &lang,
                    CommentStyle::CStyle,
                ));
            }
            "java" => {
                out.extend(empty_catches_java(&content, &rel));
                out.extend(tautological_tests_java(&content, &rel));
                out.extend(always_true_conditions_java(&content, &rel));
                out.extend(passthrough_wrappers_java(&content, &rel));
                let lang = tree_sitter_java::LANGUAGE.into();
                out.extend(commented_out_code(
                    &content,
                    &rel,
                    &lang,
                    CommentStyle::CStyle,
                ));
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
            confidence: default_confidence(SlopCategory::EmptyCatch),
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
                confidence: default_confidence(SlopCategory::EmptyCatch),
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
            confidence: default_confidence(SlopCategory::TautologicalTest),
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
            confidence: default_confidence(SlopCategory::EmptyCatch),
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
            confidence: default_confidence(SlopCategory::EmptyCatch),
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
    //
    // Several common shapes are intentional, not slop — see
    // `is_intentional_let_underscore_discard` for the skip list. The
    // remaining flagged cases are genuine "silently dropped Result"
    // candidates worth a human eye.
    out.extend(run_ast_query(
        content,
        &language,
        r#"(let_declaration value: (_)) @body"#,
        |node| {
            let text = node.utf8_text(content.as_bytes()).ok()?;
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
            // Extract the value expression text so we can recognize
            // intentional patterns (lazy-static warmup, fire-and-forget
            // spawns, best-effort cleanup, etc).
            let value_node = node.child_by_field_name("value")?;
            let value_text = value_node.utf8_text(content.as_bytes()).ok()?;
            if is_intentional_let_underscore_discard(value_text) {
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
                confidence: default_confidence(SlopCategory::EmptyCatch),
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
                confidence: default_confidence(SlopCategory::EmptyCatch),
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
                confidence: default_confidence(SlopCategory::EmptyCatch),
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
                confidence: default_confidence(SlopCategory::EmptyCatch),
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
                confidence: default_confidence(SlopCategory::EmptyCatch),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
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
                confidence: default_confidence(SlopCategory::TautologicalTest),
                reason: format!("Go testify `{method}{args_text}` always passes"),
            });
            None
        },
    );
    out
}

// ── Always-true / always-false condition detection ─────────────
//
// Conditions that always evaluate to a known value: `if true`,
// `if false`, `if 1 == 1`, `if x != null && x == null`. Either the
// branch is dead code (always-false) or the condition is structurally
// wrong (always-true masking a real check that was meant). Both are
// worth a human eye.
//
// Detection strategy: per-language tree-sitter query for `if`
// conditions, then string-classify the condition text. Same approach
// works across all 5 languages because the patterns are identical at
// the textual level (`true`, `false`, `x == x`, `x && !x`).

fn always_true_conditions_rust(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_rust::LANGUAGE.into();
    run_ast_query(
        content,
        &language,
        r#"(if_expression condition: (_) @body) @decl"#,
        |cond_node| classify_always_true(cond_node, content.as_bytes(), rel, "Rust `if`"),
    )
}

fn always_true_conditions_ts_js(
    content: &str,
    rel: &str,
    language: &tree_sitter::Language,
) -> Vec<SlopFix> {
    let mut out = Vec::new();
    out.extend(run_ast_query(
        content,
        language,
        r#"(if_statement condition: (parenthesized_expression (_) @body)) @decl"#,
        |cond_node| classify_always_true(cond_node, content.as_bytes(), rel, "TS/JS `if`"),
    ));
    // Ternary `x ? a : b` with constant condition.
    out.extend(run_ast_query(
        content,
        language,
        r#"(ternary_expression condition: (_) @body) @decl"#,
        |cond_node| classify_always_true(cond_node, content.as_bytes(), rel, "TS/JS ternary"),
    ));
    out
}

fn always_true_conditions_python(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_python::LANGUAGE.into();
    run_ast_query(
        content,
        &language,
        r#"(if_statement condition: (_) @body) @decl"#,
        |cond_node| classify_always_true(cond_node, content.as_bytes(), rel, "Python `if`"),
    )
}

fn always_true_conditions_go(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_go::LANGUAGE.into();
    run_ast_query(
        content,
        &language,
        r#"(if_statement condition: (_) @body) @decl"#,
        |cond_node| classify_always_true(cond_node, content.as_bytes(), rel, "Go `if`"),
    )
}

fn always_true_conditions_java(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_java::LANGUAGE.into();
    run_ast_query(
        content,
        &language,
        r#"(if_statement condition: (parenthesized_expression (_) @body)) @decl"#,
        |cond_node| classify_always_true(cond_node, content.as_bytes(), rel, "Java `if`"),
    )
}

/// Inspect a condition text. If it's structurally always-true or
/// always-false, emit a fix pointing at the condition's line range.
/// Returns None for normal conditions.
fn classify_always_true(
    cond_node: tree_sitter::Node,
    source: &[u8],
    rel: &str,
    lang_label: &str,
) -> Option<SlopFix> {
    let text = cond_node.utf8_text(source).ok()?.trim();

    // Direct boolean literals.
    let constant = matches!(text, "true" | "True" | "false" | "False");

    // `x == x` / `x === x` / `x is x` — same expression both sides.
    let same_eq = is_same_expression_compare(text, &["==", "===", " is "]);

    // Contradictory conjunction: `x != null && x == null`,
    // `x && !x`, etc.
    let contradiction = is_contradiction(text);

    if !constant && !same_eq && !contradiction {
        return None;
    }
    let kind = if constant {
        format!("constant condition `{text}` always evaluates the same way")
    } else if same_eq {
        format!("self-comparison `{text}` always evaluates true")
    } else {
        format!("contradictory condition `{text}` always evaluates false")
    };

    let start = (cond_node.start_position().row as u32) + 1;
    let end = (cond_node.end_position().row as u32) + 1;
    let category = SlopCategory::AlwaysTrueCondition;
    Some(SlopFix {
        action: SlopAction::DeleteLines {
            path: rel.to_string(),
            lines: [start, end],
        },
        category,
        reason: format!("{lang_label}: {kind}"),
        confidence: default_confidence(category),
    })
}

fn is_same_expression_compare(text: &str, ops: &[&str]) -> bool {
    for op in ops {
        // Use top-level split so parens/brackets don't trip us up on
        // expressions like `(a() == a())` (not that is_simple_atom
        // would accept them; still, robust delimiter location first).
        if let Some((lhs, rhs)) = split_top_level_op(text, op) {
            let lhs_inner = strip_one_paren_pair(lhs.trim());
            let rhs_inner = strip_one_paren_pair(rhs.trim());
            if !lhs_inner.is_empty() && lhs_inner == rhs_inner && is_simple_atom(lhs_inner) {
                return true;
            }
        }
    }
    false
}

fn strip_one_paren_pair(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.starts_with('(') && trimmed.ends_with(')') {
        let inner = &trimmed[1..trimmed.len() - 1];
        // Only strip when the outer `(` and `)` form a single
        // balanced pair. Walk the inner text tracking depth: if it
        // ever drops below 0, the outer `(` was already closed mid
        // way (e.g. `(a)+(b)` with inner `a)+(b`), so the outer
        // parens are not a single pair.
        let mut depth = 0i32;
        for c in inner.chars() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return trimmed;
                    }
                }
                _ => {}
            }
        }
        if depth == 0 {
            return inner.trim();
        }
    }
    trimmed
}

fn is_simple_atom(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if matches!(
        s,
        "true" | "false" | "null" | "undefined" | "None" | "True" | "False"
    ) {
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
    // Bare identifier or simple field access (`x.y`, `x.y.z`).
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn is_contradiction(text: &str) -> bool {
    // `x && !x` — same atom both sides of `&&`, one negated.
    if let Some((lhs, rhs)) = split_top_level_op(text, "&&") {
        // Strip optional outer parens — `(x == null) && (x != null)`
        // is semantically identical to the unparenthesized form.
        let lhs = strip_one_paren_pair(lhs.trim());
        let rhs = strip_one_paren_pair(rhs.trim());
        if let Some(neg_lhs) = rhs.strip_prefix('!') {
            if lhs == neg_lhs.trim() && is_simple_atom(lhs) {
                return true;
            }
        }
        if let Some(neg_rhs) = lhs.strip_prefix('!') {
            if neg_rhs.trim() == rhs && is_simple_atom(rhs) {
                return true;
            }
        }
        // `x == null && x != null`, `x === null && x !== null`
        if let (Some((l_lhs, l_rhs)), Some((r_lhs, r_rhs))) =
            (split_first_compare(lhs), split_first_compare(rhs))
            && l_lhs.trim() == r_lhs.trim()
            && l_rhs.trim() == r_rhs.trim()
        {
            let l_op = compare_op(lhs).unwrap_or("");
            let r_op = compare_op(rhs).unwrap_or("");
            if (l_op == "==" && r_op == "!=")
                || (l_op == "!=" && r_op == "==")
                || (l_op == "===" && r_op == "!==")
                || (l_op == "!==" && r_op == "===")
            {
                return true;
            }
        }
    }
    false
}

fn split_top_level_op<'a>(text: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let mut depth = 0i32;
    let bytes = text.as_bytes();
    let op_bytes = op.as_bytes();
    let mut i = 0;
    while i + op_bytes.len() <= bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        if depth == 0 && &bytes[i..i + op_bytes.len()] == op_bytes {
            return Some((&text[..i], &text[i + op_bytes.len()..]));
        }
        i += 1;
    }
    None
}

fn split_first_compare(text: &str) -> Option<(&str, &str)> {
    for op in ["===", "!==", "==", "!=", "<=", ">=", "<", ">"] {
        if let Some(pos) = text.find(op) {
            return Some((&text[..pos], &text[pos + op.len()..]));
        }
    }
    None
}

fn compare_op(text: &str) -> Option<&'static str> {
    ["===", "!==", "==", "!=", "<=", ">=", "<", ">"]
        .into_iter()
        .find(|op| text.contains(op))
}

// ── Passthrough wrapper detection ─────────────────────────────
//
// Match functions whose entire body is a single call to ANOTHER
// function with the SAME arguments — pure delegation that adds no
// transformation, validation, logging, or composition. Common AI-
// slop pattern (`function getUser(id) { return fetchUser(id); }`).
//
// Skip patterns that are legitimately not slop:
//
//   - Single-arg single-call methods that delegate to a member
//     (`this.x()`, `self.x`) — proper encapsulation
//   - Wrappers that DO transform (`f(x.trim())`) — already different
//   - Wrappers with even one extra statement (logging, etc.)
//   - Generic / type-parameterized wrappers (mostly Rust) where the
//     wrapper's signature provides a concrete type for an otherwise
//     generic API — pragmatic skip by checking for `<` in the
//     declaration line; skipping the whole class is acceptable since
//     these are intentional even if the body looks like passthrough
//   - Trait-impl methods (Rust) — the trait contract requires the
//     method to exist with that signature

fn passthrough_wrappers_rust(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_rust::LANGUAGE.into();
    // Match any function_item; we filter the body shape in the
    // closure since tree-sitter can't easily express "exactly one
    // statement" without missing edge cases.
    run_ast_query(
        content,
        &language,
        r#"(function_item parameters: (parameters) body: (block) @body) @decl"#,
        |body_node| {
            let func_node = body_node.parent()?;
            let params_node = func_node.child_by_field_name("parameters")?;
            let param_names = rust_param_names(&params_node, content.as_bytes());
            if param_names.is_empty() {
                // Zero-arg passthroughs `fn f() { g() }` are also
                // valid candidates — proceed.
            }
            // Generic functions are often pragmatic wrappers
            // providing concrete types over a more general API. Use
            // tree-sitter's `type_parameters` field rather than text
            // scanning for `<` so that ordinary return types like
            // `Vec<Item>` or `Result<T, E>` don't trigger the skip.
            if func_node.child_by_field_name("type_parameters").is_some() {
                return None;
            }
            // Trait method impls — skip if the enclosing impl is a
            // trait impl. Uses the AST `trait` field (set on
            // `impl Trait for Type`, absent on bare `impl Type`)
            // rather than substring-matching " for " in the header.
            if rust_function_is_trait_impl(&func_node, content.as_bytes()) {
                return None;
            }
            let call = rust_body_single_call(body_node)?;
            let call_args_text = call
                .child_by_field_name("arguments")?
                .utf8_text(content.as_bytes())
                .ok()?;
            // Expect `(arg, arg, …)` — strip parens, split on top-
            // level commas.
            let inner = call_args_text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            let arg_names: Vec<String> = split_top_commas(inner)
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if arg_names != param_names {
                return None;
            }
            let callee = call
                .child_by_field_name("function")?
                .utf8_text(content.as_bytes())
                .ok()?;
            // Don't flag delegation to `self.x` / `Self::x` — those
            // are intentional encapsulation patterns, not slop.
            if callee.starts_with("self.") || callee.starts_with("Self::") {
                return None;
            }
            let func_name = func_node
                .child_by_field_name("name")?
                .utf8_text(content.as_bytes())
                .ok()?;
            let start = (func_node.start_position().row as u32) + 1;
            let end = (func_node.end_position().row as u32) + 1;
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::PassthroughWrapper,
                confidence: default_confidence(SlopCategory::PassthroughWrapper),
                reason: format!(
                    "Rust `fn {func_name}(…)` is a single-call passthrough to `{callee}` with identical args"
                ),
            })
        },
    )
}

fn rust_param_names(params: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        // Skip `self` / `&self` / `&mut self` parameters; they're
        // implicit in the body and never appear as arg-names.
        if child.kind() == "self_parameter" {
            continue;
        }
        // Parameter pattern: `name: Type` → grab the leftmost
        // identifier-shaped child as the name.
        if let Some(pat) = child.child_by_field_name("pattern") {
            if let Ok(text) = pat.utf8_text(source) {
                out.push(text.trim().to_string());
            }
        } else if let Ok(text) = child.utf8_text(source) {
            // Fallback: take the whole parameter text up to the colon.
            let name = text.split(':').next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out
}

fn rust_function_is_trait_impl(func: &tree_sitter::Node, _source: &[u8]) -> bool {
    let mut cursor = func.parent();
    while let Some(node) = cursor {
        if node.kind() == "impl_item" {
            // tree-sitter-rust sets the `trait` field only on trait
            // impls (`impl Trait for Type`); inherent impls
            // (`impl Type`) have no such field. Using the AST field
            // is more robust than substring-matching " for " in the
            // header (e.g. generic bounds like `for<'a>` would
            // false-positive that approach).
            return node.child_by_field_name("trait").is_some();
        }
        cursor = node.parent();
    }
    false
}

fn rust_body_single_call(body: tree_sitter::Node) -> Option<tree_sitter::Node> {
    // Body must be a block with exactly one effective child. Three
    // shapes count:
    //
    //   tail expression: `{ inner(x) }`     → call_expression
    //   expression-stmt: `{ inner(x); }`    → expr_stmt → call
    //   bare return:     `{ return inner(x) }` → return_expression → call
    //   return + semi:   `{ return inner(x); }` → expr_stmt → return_expression → call
    if body.named_child_count() != 1 {
        return None;
    }
    let child = body.named_child(0)?;
    fn unwrap_return_or_call<'a>(n: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
        match n.kind() {
            "call_expression" => Some(n),
            "return_expression" => {
                let inner = n.named_child(0)?;
                (inner.kind() == "call_expression").then_some(inner)
            }
            _ => None,
        }
    }
    let target = if child.kind() == "expression_statement" {
        child.named_child(0)?
    } else {
        child
    };
    unwrap_return_or_call(target)
}

fn passthrough_wrappers_ts_js(
    content: &str,
    rel: &str,
    language: &tree_sitter::Language,
) -> Vec<SlopFix> {
    // Three function shapes to cover:
    //   - function declaration:   function f(x) { return g(x); }
    //   - function expression:    const f = function(x) { return g(x); };
    //   - arrow function (block): const f = (x) => { return g(x); };
    //   - arrow function (expr):  const f = (x) => g(x);
    //
    // We match `function_declaration` and `arrow_function` nodes and
    // inspect the body shape inline.
    let mut out = Vec::new();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(content, None) else {
        return out;
    };
    let query_src = r#"
    [
      (function_declaration
        parameters: (formal_parameters) @params
        body: (statement_block) @body) @decl
      (arrow_function
        parameters: (formal_parameters) @params
        body: (_) @body) @decl
    ]
    "#;
    let Ok(query) = tree_sitter::Query::new(language, query_src) else {
        return out;
    };
    use streaming_iterator::StreamingIterator;
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());

    let decl_idx = query.capture_index_for_name("decl");
    let params_idx = query.capture_index_for_name("params");
    let body_idx = query.capture_index_for_name("body");

    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    while let Some(m) = matches.next() {
        let mut decl: Option<tree_sitter::Node> = None;
        let mut params: Option<tree_sitter::Node> = None;
        let mut body: Option<tree_sitter::Node> = None;
        for cap in m.captures {
            if Some(cap.index) == decl_idx {
                decl = Some(cap.node);
            } else if Some(cap.index) == params_idx {
                params = Some(cap.node);
            } else if Some(cap.index) == body_idx {
                body = Some(cap.node);
            }
        }
        let (Some(decl), Some(params), Some(body)) = (decl, params, body) else {
            continue;
        };
        // Skip TS generics like `function f<T>(x: T) { … }` —
        // pragmatic wrappers providing a concrete type. Use the AST
        // `type_parameters` field rather than text scanning so we
        // don't false-positive on return types like `Array<string>`
        // or arrow function bodies that contain `<` (the previous
        // header-text approach was broken for arrow expression
        // bodies because `split('{')` returned the entire decl
        // including the body for those).
        if decl.child_by_field_name("type_parameters").is_some() {
            continue;
        }

        let param_names = ts_js_param_names(&params, content.as_bytes());
        let call: tree_sitter::Node = match ts_js_body_single_call(&body) {
            Some(c) => c,
            None => continue,
        };
        let args_node: tree_sitter::Node = match call.child_by_field_name("arguments") {
            Some(a) => a,
            None => continue,
        };
        let args_text = args_node.utf8_text(content.as_bytes()).unwrap_or("");
        let inner = args_text
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')');
        let arg_names: Vec<String> = split_top_commas(inner)
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if arg_names != param_names {
            continue;
        }
        let callee: &str = match call.child_by_field_name("function") {
            Some(f) => f.utf8_text(content.as_bytes()).unwrap_or(""),
            None => continue,
        };
        // Skip method delegation (this.foo / self.foo) — encapsulation,
        // not slop.
        if callee.starts_with("this.") {
            continue;
        }
        let start = (decl.start_position().row as u32) + 1;
        let end = (decl.end_position().row as u32) + 1;
        if !seen.insert((start, end)) {
            continue;
        }
        // Try to derive the function name (declaration vs arrow's
        // parent variable_declarator). Falls back to `<anonymous>`
        // for arrow functions assigned to non-variable contexts.
        let name = decl
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(content.as_bytes()).ok())
            .unwrap_or_else(|| {
                decl.parent()
                    .and_then(|p| p.child_by_field_name("name"))
                    .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                    .unwrap_or("<anonymous>")
            });
        out.push(SlopFix {
            action: SlopAction::DeleteLines {
                path: rel.to_string(),
                lines: [start, end],
            },
            category: SlopCategory::PassthroughWrapper,
            confidence: default_confidence(SlopCategory::PassthroughWrapper),
            reason: format!(
                "TS/JS `function {name}(…)` is a single-call passthrough to `{callee}` with identical args"
            ),
        });
    }
    out
}

fn ts_js_param_names(params: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        let text = child.utf8_text(source).unwrap_or("").trim();
        // Strip TypeScript type annotation: `x: string` → `x`
        let name = text.split(':').next().unwrap_or("").trim();
        // Strip default value: `x = 5` → `x`
        let name = name.split('=').next().unwrap_or("").trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

fn ts_js_body_single_call<'a>(body: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    // Block body: `{ return f(x); }` → body has 1 named child that's
    // a return_statement wrapping a call_expression. OR `{ f(x); }`
    // → 1 named child that's an expression_statement wrapping a call.
    //
    // Arrow expression body: body IS the call_expression directly.
    if body.kind() == "call_expression" {
        return Some(*body);
    }
    if body.kind() == "statement_block" {
        if body.named_child_count() != 1 {
            return None;
        }
        let child = body.named_child(0)?;
        match child.kind() {
            "return_statement" => {
                let inner = child.named_child(0)?;
                if inner.kind() == "call_expression" {
                    return Some(inner);
                }
            }
            "expression_statement" => {
                let inner = child.named_child(0)?;
                if inner.kind() == "call_expression" {
                    return Some(inner);
                }
            }
            _ => {}
        }
    }
    None
}

fn passthrough_wrappers_python(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_python::LANGUAGE.into();
    run_ast_query(
        content,
        &language,
        r#"(function_definition
            parameters: (parameters) @params
            body: (block) @body) @decl"#,
        |body_node| {
            let func_node = body_node.parent()?;
            let params_node = func_node.child_by_field_name("parameters")?;
            let mut param_names = python_param_names(&params_node, content.as_bytes());
            // Drop leading `self` / `cls` — same encapsulation
            // exception we apply for Rust.
            if matches!(
                param_names.first().map(|s| s.as_str()),
                Some("self" | "cls")
            ) {
                param_names.remove(0);
            }

            // Single statement body that's `return <call>(…)`.
            if body_node.named_child_count() != 1 {
                return None;
            }
            let stmt = body_node.named_child(0)?;
            let inner = match stmt.kind() {
                "return_statement" | "expression_statement" => stmt.named_child(0)?,
                _ => return None,
            };
            let call = inner;
            if call.kind() != "call" {
                return None;
            }
            let args_node = call.child_by_field_name("arguments")?;
            let args_text = args_node.utf8_text(content.as_bytes()).ok()?;
            let inner = args_text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            let arg_names: Vec<String> = split_top_commas(inner)
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if arg_names != param_names {
                return None;
            }
            let callee = call
                .child_by_field_name("function")?
                .utf8_text(content.as_bytes())
                .ok()?;
            if callee.starts_with("self.") {
                return None;
            }
            let func_name = func_node
                .child_by_field_name("name")?
                .utf8_text(content.as_bytes())
                .ok()?;
            let start = (func_node.start_position().row as u32) + 1;
            let end = (func_node.end_position().row as u32) + 1;
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::PassthroughWrapper,
                confidence: default_confidence(SlopCategory::PassthroughWrapper),
                reason: format!(
                    "Python `def {func_name}(…)` is a single-call passthrough to `{callee}` with identical args"
                ),
            })
        },
    )
}

fn python_param_names(params: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        let text = child.utf8_text(source).unwrap_or("").trim();
        // Type annotation `x: int` → `x`; default `x=5` → `x`.
        let name = text.split(':').next().unwrap_or("").trim();
        let name = name.split('=').next().unwrap_or("").trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

fn passthrough_wrappers_go(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_go::LANGUAGE.into();
    let mut out = Vec::new();
    out.extend(go_passthrough(
        content,
        rel,
        &language,
        r#"(function_declaration
            name: (identifier) @name
            parameters: (parameter_list) @params
            body: (block) @body) @decl"#,
    ));
    // Method declaration: also capture the receiver so we can
    // distinguish "method delegating to self" (encapsulation, skip)
    // from "method delegating to argument or another package"
    // (passthrough, flag).
    out.extend(go_passthrough(
        content,
        rel,
        &language,
        r#"(method_declaration
            receiver: (parameter_list (parameter_declaration name: (identifier) @receiver))
            name: (field_identifier) @name
            parameters: (parameter_list) @params
            body: (block) @body) @decl"#,
    ));
    out
}

fn go_passthrough(
    content: &str,
    rel: &str,
    language: &tree_sitter::Language,
    query_src: &str,
) -> Vec<SlopFix> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let Ok(query) = tree_sitter::Query::new(language, query_src) else {
        return Vec::new();
    };
    use streaming_iterator::StreamingIterator;
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());

    let decl_idx = query.capture_index_for_name("decl");
    let name_idx = query.capture_index_for_name("name");
    let params_idx = query.capture_index_for_name("params");
    let body_idx = query.capture_index_for_name("body");
    let receiver_idx = query.capture_index_for_name("receiver");

    let mut out = Vec::new();
    while let Some(m) = matches.next() {
        let mut decl = None;
        let mut name_node = None;
        let mut params = None;
        let mut body = None;
        let mut receiver_text: Option<String> = None;
        for cap in m.captures {
            if Some(cap.index) == decl_idx {
                decl = Some(cap.node);
            } else if Some(cap.index) == name_idx {
                name_node = Some(cap.node);
            } else if Some(cap.index) == params_idx {
                params = Some(cap.node);
            } else if Some(cap.index) == body_idx {
                body = Some(cap.node);
            } else if Some(cap.index) == receiver_idx {
                receiver_text = cap
                    .node
                    .utf8_text(content.as_bytes())
                    .ok()
                    .map(str::to_string);
            }
        }
        let (Some(decl), Some(name_node), Some(params), Some(body)) =
            (decl, name_node, params, body)
        else {
            continue;
        };
        let param_names = go_param_names(&params, content.as_bytes());
        if body.named_child_count() != 1 {
            continue;
        }
        let stmt = body.named_child(0).unwrap();
        let call = if stmt.kind() == "return_statement" {
            stmt.named_child(0).and_then(|n| {
                if n.kind() == "expression_list" {
                    n.named_child(0)
                } else {
                    Some(n)
                }
            })
        } else if stmt.kind() == "expression_statement" {
            stmt.named_child(0)
        } else {
            None
        };
        let Some(call) = call else { continue };
        if call.kind() != "call_expression" {
            continue;
        }
        let Some(args_node) = call.child_by_field_name("arguments") else {
            continue;
        };
        let args_text = args_node.utf8_text(content.as_bytes()).unwrap_or("");
        let inner = args_text
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')');
        let arg_names: Vec<String> = split_top_commas(inner)
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if arg_names != param_names {
            continue;
        }
        let callee = call
            .child_by_field_name("function")
            .and_then(|f| f.utf8_text(content.as_bytes()).ok())
            .unwrap_or("");
        // Method delegation `(r *Receiver) Foo() { return r.Foo() }`
        // is intentional encapsulation, not slop. We capture the
        // receiver name from the AST and only skip when the call's
        // object exactly matches it. This correctly flags genuine
        // passthroughs to argument methods (`func P(c *Client) {
        // c.Process() }`) and stdlib calls (`os.Open`, `fmt.Println`).
        if let Some(recv) = receiver_text.as_deref() {
            let object = callee.split('.').next().unwrap_or("");
            if object == recv {
                continue;
            }
        }
        let func_name = name_node.utf8_text(content.as_bytes()).unwrap_or("");
        let start = (decl.start_position().row as u32) + 1;
        let end = (decl.end_position().row as u32) + 1;
        out.push(SlopFix {
            action: SlopAction::DeleteLines {
                path: rel.to_string(),
                lines: [start, end],
            },
            category: SlopCategory::PassthroughWrapper,
            confidence: default_confidence(SlopCategory::PassthroughWrapper),
            reason: format!(
                "Go `func {func_name}(…)` is a single-call passthrough to `{callee}` with identical args"
            ),
        });
    }
    out
}

fn go_param_names(params: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "parameter_declaration" {
            continue;
        }
        let mut inner = child.walk();
        for sub in child.named_children(&mut inner) {
            if sub.kind() == "identifier"
                && let Ok(text) = sub.utf8_text(source)
            {
                out.push(text.to_string());
            }
        }
    }
    out
}

fn passthrough_wrappers_java(content: &str, rel: &str) -> Vec<SlopFix> {
    let language = tree_sitter_java::LANGUAGE.into();
    run_ast_query(
        content,
        &language,
        r#"(method_declaration
            name: (identifier) @name
            parameters: (formal_parameters) @params
            body: (block) @body) @decl"#,
        |body_node| {
            let func_node = body_node.parent()?;
            let params_node = func_node.child_by_field_name("parameters")?;
            let param_names = java_param_names(&params_node, content.as_bytes());
            if body_node.named_child_count() != 1 {
                return None;
            }
            let stmt = body_node.named_child(0)?;
            let call = match stmt.kind() {
                "return_statement" | "expression_statement" => stmt.named_child(0)?,
                _ => return None,
            };
            if call.kind() != "method_invocation" {
                return None;
            }
            let args_node = call.child_by_field_name("arguments")?;
            let args_text = args_node.utf8_text(content.as_bytes()).ok()?;
            let inner = args_text
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')');
            let arg_names: Vec<String> = split_top_commas(inner)
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if arg_names != param_names {
                return None;
            }
            // `this.foo(x)` / `super.foo(x)` — encapsulation, skip.
            let callee_name = call
                .child_by_field_name("name")?
                .utf8_text(content.as_bytes())
                .ok()?;
            let object = call
                .child_by_field_name("object")
                .and_then(|o| o.utf8_text(content.as_bytes()).ok());
            if object == Some("this") || object == Some("super") {
                return None;
            }
            let callee_repr = match object {
                Some(o) => format!("{o}.{callee_name}"),
                None => callee_name.to_string(),
            };
            let func_name = func_node
                .child_by_field_name("name")?
                .utf8_text(content.as_bytes())
                .ok()?;
            let start = (func_node.start_position().row as u32) + 1;
            let end = (func_node.end_position().row as u32) + 1;
            Some(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.to_string(),
                    lines: [start, end],
                },
                category: SlopCategory::PassthroughWrapper,
                confidence: default_confidence(SlopCategory::PassthroughWrapper),
                reason: format!(
                    "Java `{func_name}(…)` is a single-call passthrough to `{callee_repr}` with identical args"
                ),
            })
        },
    )
}

fn java_param_names(params: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        if let Some(name) = child.child_by_field_name("name")
            && let Ok(text) = name.utf8_text(source)
        {
            out.push(text.to_string());
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

// ── Commented-out code detection ─────────────────────────────────
//
// Find comment blocks whose content re-parses as valid code in the
// surrounding language. Three shapes trigger:
//
//   - contiguous single-line comments (`// a\n// b\n// c`)
//   - a single block comment (`/* a\n b\n c */`)
//   - Python triple-quoted string used as a comment (`""" … """` at
//     module top level, not as a docstring)
//
// Guards against false positives:
//
//   - require 3+ lines of stripped content
//   - skip documentation comments (`///`, `//!`, `/**`, `"""..."""`
//     attached to a declaration)
//   - skip short marker-only comments (`// TODO:`, `// FIXME:`, etc.)
//   - require the re-parse to have zero ERROR nodes AND contain at
//     least one substantive node (function, class, assignment, if,
//     loop, return, ...) — literal/identifier-only snippets are
//     usually ambient prose that happens to parse
//
// Confidence 0.85 (medium-high): substantive re-parse is a strong
// signal but language-specific prose (`// x = 1 via env var`) can
// occasionally trip it.

#[derive(Debug, Clone, Copy)]
enum CommentStyle {
    /// `//` single-line, `/* … */` block (Rust, TS/JS, Go, Java, C)
    CStyle,
    /// `#` single-line (Python, Shell, TOML)
    Hash,
}

/// Substrings that, when they appear early in a comment, mark it as a
/// note rather than commented-out code. Skipped even if the re-parse
/// would succeed.
const COMMENT_NOTE_MARKERS: &[&str] = &[
    "TODO",
    "FIXME",
    "NOTE",
    "HACK",
    "XXX",
    "WARNING",
    "SAFETY",
    "agentsys-ignore",
    "allow(",
    "SPDX-License",
];

/// A note comment is any comment group where ANY line starts with a
/// well-known marker. Checking per-line matters because
/// `// cleanup\n// TODO: re-enable\n// fn pending() {}` should be
/// suppressed even though the TODO is on line 2.
fn is_note_comment(stripped: &str) -> bool {
    stripped.lines().any(|l| {
        let trimmed = l.trim_start();
        let head: &str = trimmed.get(..trimmed.len().min(40)).unwrap_or("");
        COMMENT_NOTE_MARKERS.iter().any(|m| head.contains(m))
    })
}

/// Strip leading whitespace-one-space after a comment marker, preserving
/// any additional indentation past that. Commented-out Python or any
/// indented block needs its relative indent kept so it re-parses:
///
///   `// body()`           → `body()`
///   `//     do_work()`    → `    do_work()` (keeps 4 spaces)
///   `//body()`            → `body()`
fn strip_one_marker_space(s: &str) -> &str {
    s.strip_prefix(' ').unwrap_or(s)
}

/// Strip the comment markers off a single comment node's text, returning
/// the inner content. For block comments the leading `/*` and trailing
/// `*/` are removed; for line comments the leading `//` / `#` is
/// stripped per line. Documentation comment prefixes (`///`, `//!`,
/// `/**`) return `None` — those are attached to a declaration
/// and should not be flagged.
///
/// Block comments frequently use the "leading star" layout:
///
/// ```ignore
/// /*
///  * first line
///  * second line
///  */
/// ```
///
/// Without normalization the content would be `"\n * first line\n *
/// second line\n"` which won't re-parse. We detect the case by
/// requiring every non-empty line after the first to start with
/// optional whitespace then `*`, and strip that prefix.
fn strip_comment(text: &str, style: CommentStyle) -> Option<String> {
    match style {
        CommentStyle::CStyle => {
            // Block comment
            if let Some(inner) = text.strip_prefix("/*").and_then(|s| s.strip_suffix("*/")) {
                // Outer doc comment `/** … */` (Rust/Java) and inner
                // doc comment `/*! … */` (Rust) are both attached to
                // declarations and must never be flagged. Plain block
                // comments (`/* … */`) pass through.
                if inner.starts_with('*') && !inner.starts_with("**") {
                    return None;
                }
                if inner.starts_with('!') {
                    return None;
                }
                return Some(strip_leading_stars(inner));
            }
            // Line comment: reject doc variants, strip `//`
            let rest = text.strip_prefix("//")?;
            if rest.starts_with('/') || rest.starts_with('!') {
                // `///` or `//!` — doc comment
                return None;
            }
            Some(strip_one_marker_space(rest).to_string())
        }
        CommentStyle::Hash => {
            // Skip shebang lines — those are interpreter directives,
            // not comments at all.
            if text.starts_with("#!") {
                return None;
            }
            let rest = text.strip_prefix('#')?;
            Some(strip_one_marker_space(rest).to_string())
        }
    }
}

/// If every non-empty line after the first in a block-comment body
/// starts with whitespace then `*`, strip that prefix. This handles
/// the common " * foo" continuation style without affecting block
/// comments whose body is already plain indented code.
fn strip_leading_stars(inner: &str) -> String {
    let lines: Vec<&str> = inner.lines().collect();
    if lines.len() < 2 {
        return inner.to_string();
    }
    // First line may be empty (typical `/*\n * …\n */`); decide based
    // on the remaining lines.
    let non_empty_tail: Vec<&str> = lines
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();
    if non_empty_tail.is_empty() {
        return inner.to_string();
    }
    let all_starred = non_empty_tail
        .iter()
        .all(|l| l.trim_start().starts_with('*'));
    if !all_starred {
        return inner.to_string();
    }
    lines
        .iter()
        .map(|l| {
            let t = l.trim_start();
            if let Some(rest) = t.strip_prefix('*') {
                strip_one_marker_space(rest).to_string()
            } else {
                (*l).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a comment group originated from a single-line comment or a
/// block comment. Used to prevent cross-style merges (a block comment
/// must never be glued onto a run of `//` lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentKind {
    Line,
    Block,
}

/// A grouping of consecutive same-style comment nodes. Rows are 0-based
/// tree-sitter positions.
struct CommentGroup {
    start_row: usize,
    end_row: usize,
    content: String,
    kind: CommentKind,
}

/// Collect comment nodes from the tree, grouping contiguous line
/// comments (no blank line between them). Block comments are each
/// their own group and are never merged with neighboring line comments.
fn collect_comment_groups(
    tree: &tree_sitter::Tree,
    source: &[u8],
    style: CommentStyle,
) -> Vec<CommentGroup> {
    let mut out: Vec<CommentGroup> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        for i in (0..node.child_count()).rev() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
        let kind_str = node.kind();
        // Comment node kinds: "comment", "line_comment", "block_comment"
        // are the common ones across tree-sitter grammars.
        let is_comment = matches!(kind_str, "comment" | "line_comment" | "block_comment");
        if !is_comment {
            continue;
        }
        let text = match node.utf8_text(source) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let Some(stripped) = strip_comment(text, style) else {
            continue;
        };
        let start_row = node.start_position().row;
        let end_row = node.end_position().row;

        // Distinguish block vs. line. Generic "comment" kind + a multi-
        // line span also counts as block (`/* … */` on multiple lines
        // in grammars that don't split the two kinds).
        let kind = if matches!(kind_str, "block_comment")
            || (kind_str == "comment" && start_row != end_row)
        {
            CommentKind::Block
        } else {
            CommentKind::Line
        };

        if kind == CommentKind::Block {
            out.push(CommentGroup {
                start_row,
                end_row,
                content: stripped,
                kind,
            });
            continue;
        }

        // Single-line: merge with the previous group only if it is ALSO
        // a line-comment run AND contiguous (no blank-line gap). Never
        // merge into a block-comment group — those are standalone by
        // definition.
        if let Some(last) = out.last_mut() {
            if last.kind == CommentKind::Line && last.end_row + 1 == start_row {
                last.end_row = end_row;
                last.content.push('\n');
                last.content.push_str(&stripped);
                continue;
            }
        }
        out.push(CommentGroup {
            start_row,
            end_row,
            content: stripped,
            kind,
        });
    }
    // Keep result order deterministic.
    out.sort_by_key(|g| g.start_row);
    out
}

/// Check whether a re-parsed tree has at least one substantive node.
/// Literals, identifiers, expression wrappers alone aren't enough —
/// many prose comments happen to parse as a bare identifier.
fn reparse_has_substantive_node(tree: &tree_sitter::Tree) -> bool {
    const SUBSTANTIVE_KINDS: &[&str] = &[
        // Universal statement shapes
        "function_declaration",
        "function_item",
        "function_definition",
        "method_declaration",
        "method_definition",
        "class_declaration",
        "class_definition",
        "class_specifier",
        "struct_item",
        "enum_item",
        "trait_item",
        "impl_item",
        "mod_item",
        "use_declaration",
        "import_statement",
        "import_from_statement",
        "import_declaration",
        "if_statement",
        "if_expression",
        "for_statement",
        "for_expression",
        "for_in_statement",
        "for_of_statement",
        "while_statement",
        "while_expression",
        "do_statement",
        "return_statement",
        "return_expression",
        "try_statement",
        "try_expression",
        "assignment",
        "assignment_expression",
        "let_declaration",
        "variable_declaration",
        "lexical_declaration",
        "const_declaration",
        "expression_statement", // narrow: still requires the outer to be a real stmt
        "call_expression",
        "await_expression",
        "match_expression",
        "switch_statement",
        "throw_statement",
    ];
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            return false;
        }
        if SUBSTANTIVE_KINDS.contains(&node.kind()) {
            return true;
        }
        for i in 0..node.child_count() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
    }
    false
}

/// Returns true if the tree contains any ERROR node anywhere. A clean
/// parse is required to avoid false positives on prose comments that
/// happen to contain a code-like fragment.
fn tree_has_parse_errors(tree: &tree_sitter::Tree) -> bool {
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return true;
        }
        for i in 0..node.child_count() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
    }
    false
}

fn commented_out_code(
    content: &str,
    rel: &str,
    language: &tree_sitter::Language,
    style: CommentStyle,
) -> Vec<SlopFix> {
    let mut out = Vec::new();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(content, None) else {
        return out;
    };
    let groups = collect_comment_groups(&tree, content.as_bytes(), style);
    for group in groups {
        // Skip short groups — 3+ content lines required.
        let line_count = group
            .content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        if line_count < 3 {
            continue;
        }
        // Skip note-style comments early.
        if is_note_comment(&group.content) {
            continue;
        }
        // Re-parse with the same language. Clean + substantive required.
        let mut reparser = tree_sitter::Parser::new();
        if reparser.set_language(language).is_err() {
            continue;
        }
        let Some(reparsed) = reparser.parse(&group.content, None) else {
            continue;
        };
        if tree_has_parse_errors(&reparsed) {
            continue;
        }
        if !reparse_has_substantive_node(&reparsed) {
            continue;
        }
        let start = (group.start_row as u32) + 1;
        let end = (group.end_row as u32) + 1;
        let category = SlopCategory::CommentedOutCode;
        out.push(SlopFix {
            action: SlopAction::DeleteLines {
                path: rel.to_string(),
                lines: [start, end],
            },
            category,
            reason: format!(
                "{line_count}-line comment block re-parses as valid code — likely commented-out code, not prose"
            ),
            confidence: default_confidence(category),
        });
    }
    out
}

// ── Stale suppression annotations (Rust) ─────────────────────────
//
// A `#[allow(dead_code)]` / `#[allow(unused)]` attribute on a symbol
// that the import graph proves IS being used. The suppression was
// correct at some point but the symbol became reachable again and
// the annotation was never removed. Keeping stale suppressions
// around blinds the real dead-code lint.
//
// Algorithm:
//
// 1. Build a `used_names` set from the map: every entry in every
//    file's `imports[].names` (exact-match; Rust identifiers are
//    case-sensitive).
// 2. For each `.rs` file, run a tree-sitter query for `attribute_item`
//    nodes. Accept attributes whose top-level identifier is `allow`
//    (not nested inside e.g. `cfg_attr(...)`) and whose argument list
//    contains one of: `dead_code`, `unused`, `unused_imports`,
//    `unused_variables`, `unused_assignments`, `unused_must_use`.
// 3. For each matched attribute, find the following sibling item
//    node (fn / struct / enum / mod / const / static / trait / impl)
//    and its identifier.
// 4. If the identifier appears in `used_names`, flag the attribute
//    for deletion (DeleteLines over the attribute's line range).
//
// Confidence 0.90: the graph-based evidence is strong but the import
// graph can be incomplete for non-Rust callers (e.g. an FFI boundary
// where the name is referenced from C/Python via `pub extern`). The
// suppression-directive system lets users silence any false positive.

const ALLOW_SUPPRESSION_LINTS: &[&str] = &[
    "dead_code",
    "unused",
    "unused_imports",
    "unused_variables",
    "unused_assignments",
    "unused_must_use",
];

/// Collected usage information from the symbol graph.
///
/// The detector needs both "name is imported somewhere" (fast
/// membership test) and "which module paths reference this name"
/// (used to bias toward same-crate matches and away from
/// cross-module name collisions). Keeping both in one struct so the
/// build walk happens once.
struct UsedSymbols {
    /// Every imported name seen across the map. Rust identifiers are
    /// case-sensitive so no case-folding.
    all_names: HashSet<String>,
    /// Per-name: the set of `imports[].from` module-path strings that
    /// imported it. Lets the caller check `name "helper" is imported
    /// from a path that could plausibly resolve to our file` rather
    /// than blindly trusting a bare-name collision.
    modules_per_name: HashMap<String, HashSet<String>>,
}

fn collect_used_symbols(map: &RepoIntelData) -> UsedSymbols {
    let mut all_names = HashSet::new();
    let mut modules_per_name: HashMap<String, HashSet<String>> = HashMap::new();
    if let Some(syms) = map.symbols.as_ref() {
        for (_path, file_syms) in syms.iter() {
            for imp in &file_syms.imports {
                for name in &imp.names {
                    all_names.insert(name.clone());
                    modules_per_name
                        .entry(name.clone())
                        .or_default()
                        .insert(imp.from.clone());
                }
            }
        }
    }
    UsedSymbols {
        all_names,
        modules_per_name,
    }
}

/// Return true if any `imports[].from` value looks like it could path
/// to the given file. A bare `crate::foo::bar` import gets matched
/// against the file path by taking each path segment and checking
/// whether the file's relative path contains that segment. It's
/// permissive by design — a hit is strong evidence, a miss would
/// wrongly spare a real stale suppression.
///
/// For a file `crates/foo/src/helpers.rs` and an import `from =
/// "crate::helpers"`, the segments [`crate`, `helpers`] both appear
/// in the path, so we return true.
///
/// When no cross-file import data is available for a name (which can
/// happen for inherent methods or crate-private usage the import
/// collector didn't capture) we conservatively return true so the
/// detector falls back to the all-names membership check it had
/// before.
fn import_paths_could_resolve_to_file(modules: &HashSet<String>, rel_file: &str) -> bool {
    if modules.is_empty() {
        return true;
    }
    let file_norm = rel_file.replace('\\', "/");
    for module in modules {
        let segments: Vec<&str> = module
            .split("::")
            .filter(|s| !s.is_empty() && *s != "crate" && *s != "self" && *s != "super")
            .collect();
        if segments.is_empty() {
            // `crate` or `self` alone — any file in the same crate is
            // a valid target. Treat as a match.
            return true;
        }
        if segments
            .iter()
            .any(|seg| file_norm.contains(&format!("/{seg}")) || file_norm.starts_with(seg))
        {
            return true;
        }
    }
    false
}

/// Parse an `#[allow(...)]` attribute via AST structure, not text
/// scanning. Returns the list of lint names only when the attribute's
/// top-level identifier is literally `allow` (not `cfg_attr`, not
/// `deny`, not `warn`). This correctly skips cases like
/// `#[cfg_attr(feature = "x", allow(dead_code))]` where `allow`
/// appears only as a nested identifier.
///
/// Shape in tree-sitter-rust:
///
///   (attribute_item
///     (attribute
///       (identifier)       ; "allow" at top level
///       arguments: (token_tree "(" (identifier)+ ")")))
///
/// Returns `Vec::new()` for inner attributes (`#![allow(...)]`) —
/// those are module-scoped; removing them would change the silence
/// shape file-wide and that's not a safe mechanical edit.
fn extract_allow_lints(attribute_node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    // Inner attributes (#![...]) carry an `inner` marker child or
    // start their text with "#!["; either check works.
    if attribute_node
        .utf8_text(source)
        .map(|t| t.starts_with("#!["))
        .unwrap_or(false)
    {
        return Vec::new();
    }
    // Find the child `attribute` node.
    let mut attr_inner = None;
    let mut c = attribute_node.walk();
    for child in attribute_node.named_children(&mut c) {
        if child.kind() == "attribute" {
            attr_inner = Some(child);
            break;
        }
    }
    let Some(attr_inner) = attr_inner else {
        return Vec::new();
    };
    // The attribute's first named child is the top-level identifier
    // (e.g. `allow`, `cfg_attr`, `derive`). Must be exactly `allow`.
    let Some(ident) = attr_inner.named_child(0) else {
        return Vec::new();
    };
    if ident.kind() != "identifier" {
        return Vec::new();
    }
    let top_name = ident.utf8_text(source).unwrap_or("");
    if top_name != "allow" {
        return Vec::new();
    }
    // Collect identifiers from the `token_tree` / `arguments`. The
    // argument list holds identifier tokens (dead_code, unused, ...),
    // possibly scoped like `clippy::needless_return`.
    let mut out: Vec<String> = Vec::new();
    let mut walker = attr_inner.walk();
    for child in attr_inner.named_children(&mut walker) {
        if child.kind() != "token_tree" {
            continue;
        }
        // Walk raw characters of the token-tree text: tree-sitter
        // tokenises the inside as a flat sequence so simple
        // comma-split is fine here, but we strip clippy:: scope
        // prefixes so `clippy::needless_return` becomes `needless_return`.
        let tt_text = child.utf8_text(source).unwrap_or("");
        // Remove the outer parentheses.
        let inner = tt_text
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or(tt_text);
        for raw in inner.split(',') {
            let name = raw.trim().trim_start_matches("clippy::");
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Find the identifier of the item an attribute is attached to. Rust
/// tree-sitter puts attribute_item as a sibling before the decorated
/// item inside the same enclosing scope. We walk forward siblings
/// skipping whitespace/other attributes/macros until we hit an item
/// with a named identifier, or run out.
///
/// Returning `None` for unrecognized kinds rather than aborting means
/// stacked attributes work: `#[allow(dead_code)] #[derive(Debug)] struct
/// Foo` walks attr → attr → struct_item and finds `Foo`.
fn next_item_identifier<'a>(
    attr: &tree_sitter::Node<'a>,
    source: &[u8],
) -> Option<(String, tree_sitter::Node<'a>)> {
    const NAMED_ITEM_KINDS: &[&str] = &[
        "function_item",
        "function_signature_item",
        "struct_item",
        "enum_item",
        "mod_item",
        "const_item",
        "static_item",
        "trait_item",
        "type_item",
        "union_item",
        "extern_crate_declaration",
    ];
    const SKIP_KINDS: &[&str] = &[
        "attribute_item",
        "line_comment",
        "block_comment",
        "macro_invocation",
        "inner_doc_comment_marker",
        "outer_doc_comment_marker",
    ];
    let mut cur = attr.next_named_sibling();
    while let Some(node) = cur {
        let kind = node.kind();
        if SKIP_KINDS.contains(&kind) {
            cur = node.next_named_sibling();
            continue;
        }
        if NAMED_ITEM_KINDS.contains(&kind) {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(source)
            {
                return Some((name.to_string(), node));
            }
            return None;
        }
        // impl_item / use_declaration / foreign_mod_item / etc. are
        // legitimately decorated but don't carry a single name field
        // we can check against the usage graph. Treat as "can't
        // determine" and move on — underreporting is safer than
        // false positives on an impl block.
        return None;
    }
    None
}

fn stale_suppressions_rust(repo_root: &Path, map: &RepoIntelData) -> Vec<SlopFix> {
    let used = collect_used_symbols(map);
    if used.all_names.is_empty() {
        // No symbol graph → we can't tell used from unused; skip
        // rather than emit false positives.
        return Vec::new();
    }
    let mut out = Vec::new();
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let query_src = "(attribute_item) @attr";
    let Ok(query) = tree_sitter::Query::new(&language, query_src) else {
        return out;
    };
    // Parser and query are both reusable across files; only the tree
    // changes per file. Instantiating them once saves tree-sitter
    // setup cost on large repos.
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return out;
    }

    for path in walk_repo_files(repo_root) {
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let rel = relative(&path, repo_root);
        // Test files routinely carry `#[allow(dead_code)]` on helper
        // fixtures that aren't "used" in the production graph. Skip
        // them to avoid noise — same rule the passthrough-wrapper
        // detector already follows.
        if is_rust_test_file(&rel) {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(tree) = parser.parse(&content, None) else {
            continue;
        };
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());
        let source = content.as_bytes();
        while let Some(m) = matches.next() {
            let Some(cap) = m.captures.first() else {
                continue;
            };
            let attr = cap.node;
            let lints = extract_allow_lints(&attr, source);
            if lints.is_empty() {
                continue;
            }
            // Only emit when EVERY lint in the attribute is a stale
            // suppression candidate. Mixed-lint attributes like
            // `#[allow(non_snake_case, dead_code)]` get left alone —
            // deleting the whole line would silently drop the
            // non-suppression lint (`non_snake_case` here). A future
            // enhancement could emit a ReplaceLines that keeps the
            // non-suppression lints, but for now we prefer under-
            // reporting to a fix that breaks other lints.
            let all_suppression = lints
                .iter()
                .all(|l| ALLOW_SUPPRESSION_LINTS.contains(&l.as_str()));
            if !all_suppression {
                continue;
            }
            let Some((name, _item_node)) = next_item_identifier(&attr, source) else {
                continue;
            };
            if !used.all_names.contains(&name) {
                continue;
            }
            // Name-collision guard: a `#[allow(dead_code)]` on a
            // private `helper` in src/a.rs shouldn't be flagged just
            // because an unrelated file imports a different `helper`
            // from a different module. Require that at least one of
            // the `imports[].from` paths for this name could plausibly
            // resolve to the current file (segment match). When no
            // import path info is available we fall through
            // permissively — see `import_paths_could_resolve_to_file`.
            if let Some(modules) = used.modules_per_name.get(&name)
                && !import_paths_could_resolve_to_file(modules, &rel)
            {
                continue;
            }
            let start = (attr.start_position().row as u32) + 1;
            let end = (attr.end_position().row as u32) + 1;
            let category = SlopCategory::StaleSuppression;
            out.push(SlopFix {
                action: SlopAction::DeleteLines {
                    path: rel.clone(),
                    lines: [start, end],
                },
                category,
                reason: format!(
                    "Rust `#[allow({})]` on `{name}` — symbol is imported elsewhere in the graph so the suppression is stale",
                    lints.join(", ")
                ),
                confidence: default_confidence(category),
            });
        }
    }
    out
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
            confidence: default_confidence(SlopCategory::TrackedArtifact),
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
    fn skips_rust_let_underscore_in_test_files() {
        let dir = make_repo(&[(
            "tests/integration.rs",
            "#[test]\nfn t() {\n    let _ = call();\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch && f.reason.contains("let _")),
            "should not flag `let _` in tests/ dir; got {:?}",
            fixes
        );
    }

    #[test]
    fn skips_rust_dot_ok_in_underscore_test_file() {
        let dir = make_repo(&[("src/foo_test.rs", "fn t() {\n    call().ok();\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains(".ok();")),
            "should not flag `.ok();` in *_test.rs; got {:?}",
            fixes
        );
    }

    #[test]
    fn flags_rust_let_underscore_in_production_file() {
        let dir = make_repo(&[("src/lib.rs", "fn f() {\n    let _ = call();\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes.iter().any(|f| f.reason.contains("let _")),
            "should flag `let _` in production code"
        );
    }

    // ── let _ context-aware filter (intentional discards) ─────────

    #[test]
    fn skips_let_underscore_lazy_pattern_warmup() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn warmup() {\n    let _ = my_pattern();\n    let _ = url_regex();\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("let _")),
            "should not flag *_pattern() / *_regex() lazy-static warmup; got {:?}",
            fixes
        );
    }

    #[test]
    fn skips_let_underscore_thread_spawn() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f() {\n    let _ = thread::spawn(move || { run(); });\n    let _ = tokio::spawn(async {});\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("let _")),
            "should not flag fire-and-forget spawns; got {:?}",
            fixes
        );
    }

    #[test]
    fn skips_let_underscore_best_effort_cleanup() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f() {\n    let _ = fs::remove_file(&p);\n    let _ = std::io::Write::flush(&mut out);\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("let _")),
            "should not flag best-effort cleanup; got {:?}",
            fixes
        );
    }

    #[test]
    fn skips_let_underscore_lock_acquisition() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f() {\n    let _ = mutex.lock();\n    let _ = rw.read().unwrap();\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("let _")),
            "should not flag lock-guard acquisition; got {:?}",
            fixes
        );
    }

    #[test]
    fn skips_let_underscore_bare_identifier_drop() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f() {\n    let outcome = compute();\n    let _ = outcome;\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes.iter().any(|f| f.reason.contains("let _")),
            "should not flag explicit drop of bare identifier; got {:?}",
            fixes
        );
    }

    #[test]
    fn still_flags_genuine_silent_result_drop() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f() {\n    let _ = file.write_all(buf);\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes.iter().any(|f| f.reason.contains("let _")),
            "should still flag genuine silent Result drop; got {:?}",
            fixes
        );
    }

    // ── Orphan-export SymbolKind filter ─────────

    #[test]
    fn orphan_exports_skips_member_kinds() {
        use analyzer_core::types::*;
        use std::collections::HashMap;

        let mut symbols = HashMap::new();
        symbols.insert(
            "src/foo.rs".into(),
            FileSymbols {
                exports: vec![
                    SymbolEntry {
                        name: "MyStruct".into(),
                        kind: SymbolKind::Struct,
                        line: 1,
                    },
                    SymbolEntry {
                        name: "field_a".into(),
                        kind: SymbolKind::Field,
                        line: 3,
                    },
                    SymbolEntry {
                        name: "VariantOne".into(),
                        kind: SymbolKind::EnumVariant,
                        line: 7,
                    },
                    SymbolEntry {
                        name: "computed_prop".into(),
                        kind: SymbolKind::Property,
                        line: 11,
                    },
                ],
                imports: vec![],
                definitions: vec![],
            },
        );

        let mut map = RepoIntelData {
            version: "test".into(),
            generated: chrono::Utc::now(),
            updated: chrono::Utc::now(),
            partial: false,
            git: GitInfo {
                analyzed_up_to: "HEAD".into(),
                total_commits_analyzed: 0,
                first_commit_date: "".into(),
                last_commit_date: "".into(),
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
                style: "".into(),
                uses_scopes: false,
                naming_patterns: None,
                test_patterns: None,
            },
            releases: Releases {
                tags: vec![],
                cadence: "".into(),
            },
            renames: vec![],
            deletions: vec![],
            symbols: Some(symbols),
            import_graph: Some(HashMap::new()),
            project: None,
            doc_refs: None,
            graph: None,
            file_descriptors: None,
            summary: None,
            embeddings_meta: None,
            entry_points: None,
        };
        // Need a non-empty import_graph so the orphan check actually
        // runs (otherwise the function returns early).
        map.import_graph
            .as_mut()
            .unwrap()
            .insert("src/other.rs".into(), vec![]);

        let fixes = orphan_exports(&map);
        // Should ONLY emit the Struct, not the field/variant/property.
        assert_eq!(
            fixes.len(),
            1,
            "expected 1 fix (Struct only); got {fixes:?}"
        );
        assert!(fixes[0].reason.contains("MyStruct"));
        assert!(fixes[0].reason.contains("struct"));
    }

    // ── Always-true conditions ─────

    #[test]
    fn detects_rust_if_true_constant() {
        let dir = make_repo(&[("src/lib.rs", "fn f() {\n    if true { return; }\n}\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition
                    && f.reason.contains("constant")),
            "should flag `if true`; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_typescript_if_x_eq_x() {
        let dir = make_repo(&[("a.ts", "function f(x) { if (x === x) return; }\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition
                    && f.reason.contains("self-comparison")),
            "should flag `if (x === x)`; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_python_if_true() {
        let dir = make_repo(&[("a.py", "def f():\n    if True:\n        return\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition),
            "should flag `if True`; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_go_if_x_eq_x() {
        let dir = make_repo(&[(
            "x.go",
            "package x\nfunc f(a int) {\n    if a == a { return }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition),
            "should flag `if a == a`; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_typescript_contradiction() {
        // x != null && x == null — always false.
        let dir = make_repo(&[(
            "a.ts",
            "function f(x) { if (x != null && x == null) return; }\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition
                    && f.reason.contains("contradictory")),
            "should flag `x != null && x == null`; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_normal_condition() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f(x: i32) {\n    if x > 0 { return; }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition),
            "should not flag normal `if x > 0`; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_function_call_condition() {
        // `if check()` — `check` could return either true or false; not constant.
        let dir = make_repo(&[(
            "src/lib.rs",
            "fn f() {\n    if check() { return; }\n}\nfn check() -> bool { true }\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::AlwaysTrueCondition),
            "should not flag `if check()`; got {:?}",
            fixes
        );
    }

    // ── Confidence + grouping ─────

    #[test]
    fn fix_carries_default_confidence_for_category() {
        let fix = SlopFix {
            action: SlopAction::DeleteFile {
                path: "x.log".into(),
            },
            category: SlopCategory::TrackedArtifact,
            reason: "test".into(),
            confidence: default_confidence(SlopCategory::TrackedArtifact),
        };
        assert!((fix.confidence - 0.97).abs() < 1e-3);
    }

    #[test]
    fn confidence_is_serialized_in_json() {
        let fix = SlopFix {
            action: SlopAction::DeleteFile {
                path: "x.log".into(),
            },
            category: SlopCategory::TrackedArtifact,
            reason: "test".into(),
            confidence: 0.95,
        };
        let json = serde_json::to_string(&fix).unwrap();
        assert!(json.contains("\"confidence\":0.95"));
    }

    #[test]
    fn confidence_default_when_missing_from_input() {
        // A pre-confidence-field artifact deserializes with the
        // category-agnostic default (0.80).
        let json = r#"{"action":"delete-file","path":"x.log","category":"tracked-artifact","reason":"test"}"#;
        let fix: SlopFix = serde_json::from_str(json).unwrap();
        assert!((fix.confidence - 0.80).abs() < 1e-3);
    }

    #[test]
    fn group_by_file_clusters_fixes_per_path_alphabetically() {
        let fixes = vec![
            SlopFix {
                action: SlopAction::DeleteLines {
                    path: "z.rs".into(),
                    lines: [1, 1],
                },
                category: SlopCategory::EmptyCatch,
                reason: "x".into(),
                confidence: 0.95,
            },
            SlopFix {
                action: SlopAction::DeleteLines {
                    path: "a.rs".into(),
                    lines: [1, 1],
                },
                category: SlopCategory::EmptyCatch,
                reason: "x".into(),
                confidence: 0.95,
            },
            SlopFix {
                action: SlopAction::DeleteLines {
                    path: "a.rs".into(),
                    lines: [10, 10],
                },
                category: SlopCategory::OrphanExport,
                reason: "x".into(),
                confidence: 0.75,
            },
        ];
        let grouped = group_by_file(&fixes);
        assert_eq!(grouped.len(), 2);
        // Alphabetically sorted by path.
        assert_eq!(grouped[0].path, "a.rs");
        assert_eq!(grouped[0].fixes.len(), 2);
        assert_eq!(grouped[1].path, "z.rs");
        assert_eq!(grouped[1].fixes.len(), 1);
    }

    // ── Passthrough wrapper detector ─────────

    #[test]
    fn detects_rust_single_call_passthrough() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "pub fn get_user(id: u32) -> User {\n    fetch_user(id)\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper
                    && f.reason.contains("get_user")
                    && f.reason.contains("fetch_user")),
            "should flag get_user as passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_rust_passthrough_with_explicit_return() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "pub fn wrap(x: i32, y: i32) -> i32 {\n    return inner(x, y);\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should flag explicit-return passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_rust_wrapper_that_transforms_args() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "pub fn wrap(x: i32) -> i32 {\n    inner(x + 1)\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag wrapper that transforms args; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_rust_wrapper_with_extra_logging() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "pub fn wrap(x: i32) -> i32 {\n    log::info!(\"wrap\");\n    inner(x)\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag wrapper with extra statement; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_rust_self_method_delegation() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "struct S;\nimpl S {\n    pub fn name(&self) -> String { self.name_impl() }\n    fn name_impl(&self) -> String { let mut s = String::new(); s.push('x'); s }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag self.x() encapsulation; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_rust_generic_wrapper() {
        // Generic wrappers often provide concrete types over a more
        // general API — pragmatic, not slop.
        let dir = make_repo(&[(
            "src/lib.rs",
            "pub fn typed_get<T: Default>(id: u32) -> T {\n    generic_get(id)\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag generic wrapper; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_rust_trait_impl_method() {
        let dir = make_repo(&[(
            "src/lib.rs",
            "trait T { fn name(&self) -> String; }\nstruct S;\nimpl T for S {\n    fn name(&self) -> String { compute_name() }\n}\nfn compute_name() -> String { let mut s = String::new(); s.push('x'); s }\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag trait impl method (contract requires it); got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_typescript_arrow_passthrough() {
        let dir = make_repo(&[("a.ts", "const wrap = (x) => inner(x);\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should flag arrow passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_typescript_function_declaration_passthrough() {
        let dir = make_repo(&[("a.ts", "function getUser(id) { return fetchUser(id); }\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should flag function-decl passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_typescript_this_method_delegation() {
        let dir = make_repo(&[(
            "a.ts",
            "class A { name() { return this.compute(); } compute() { return 1; } }\n",
        )]);
        let fixes = ast_findings(dir.path());
        // `class.method()` is parsed as method_definition not function_declaration
        // so this test really just ensures we don't flag the few things we do match.
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag `this.x()` delegation; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_python_passthrough() {
        let dir = make_repo(&[("a.py", "def get_user(id):\n    return fetch_user(id)\n")]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should flag Python passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_python_self_method_delegation() {
        let dir = make_repo(&[(
            "a.py",
            "class A:\n    def name(self, x):\n        return self.compute(x)\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag self.x() delegation; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_go_passthrough() {
        let dir = make_repo(&[(
            "x.go",
            "package x\nfunc GetUser(id int) User {\n    return FetchUser(id)\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should flag Go passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn detects_java_passthrough() {
        let dir = make_repo(&[(
            "A.java",
            "class A {\n    public int wrap(int x) { return inner(x); }\n    int inner(int x) { return x; }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should flag Java passthrough; got {:?}",
            fixes
        );
    }

    #[test]
    fn ignores_java_super_delegation() {
        let dir = make_repo(&[(
            "A.java",
            "class A extends B {\n    public int wrap(int x) { return super.wrap(x); }\n}\n",
        )]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::PassthroughWrapper),
            "should not flag super.x() delegation; got {:?}",
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

    // ── agentsys-ignore suppression directives ─────

    fn empty_map() -> RepoIntelData {
        use chrono::Utc;
        use std::collections::HashMap as StdMap;
        RepoIntelData {
            version: "test".into(),
            generated: Utc::now(),
            updated: Utc::now(),
            partial: false,
            git: analyzer_core::types::GitInfo {
                analyzed_up_to: "HEAD".into(),
                total_commits_analyzed: 0,
                first_commit_date: "".into(),
                last_commit_date: "".into(),
                scope: None,
                shallow: false,
            },
            contributors: analyzer_core::types::Contributors {
                humans: StdMap::new(),
                bots: StdMap::new(),
            },
            file_activity: StdMap::new(),
            coupling: StdMap::new(),
            conventions: analyzer_core::types::ConventionInfo {
                prefixes: StdMap::new(),
                style: "".into(),
                uses_scopes: false,
                naming_patterns: None,
                test_patterns: None,
            },
            releases: analyzer_core::types::Releases {
                tags: vec![],
                cadence: "".into(),
            },
            renames: vec![],
            deletions: vec![],
            symbols: None,
            import_graph: None,
            project: None,
            doc_refs: None,
            graph: None,
            file_descriptors: None,
            summary: None,
            embeddings_meta: None,
            entry_points: None,
        }
    }

    #[test]
    fn suppression_on_same_line_skips_fix() {
        let dir = make_repo(&[(
            "a.ts",
            "function f() {\n    try { call() } catch {} // agentsys-ignore: empty-catch\n}\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "annotation on same line should suppress; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_on_line_above_skips_fix() {
        let dir = make_repo(&[(
            "a.ts",
            "function f() {\n    // agentsys-ignore: empty-catch\n    try { call() } catch {}\n}\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "annotation on line above should suppress; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_python_uses_hash_marker() {
        let dir = make_repo(&[(
            "a.py",
            "def f():\n    # agentsys-ignore: empty-catch\n    try:\n        x = 1\n    except:\n        pass\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        // The except is at line 5; the directive on line 2 is too far
        // (we only check ±1). This should NOT suppress.
        assert!(
            result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "directive too far away should NOT suppress; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_python_directly_above_works() {
        let dir = make_repo(&[(
            "a.py",
            "def f():\n    try:\n        x = 1\n    # agentsys-ignore: empty-catch\n    except:\n        pass\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "directive directly above except should suppress; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_wrong_category_does_not_skip() {
        // Directive names a different category — fix must still fire.
        let dir = make_repo(&[(
            "a.ts",
            "// agentsys-ignore: orphan-export\nfunction f() { try {} catch {} }\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        assert!(
            result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "different-category directive should NOT suppress empty-catch; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_comma_list_covers_multiple_categories() {
        let dir = make_repo(&[(
            "a.ts",
            "function f() {\n    // agentsys-ignore: empty-catch, tautological-test\n    try {} catch {}\n}\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "comma-list should cover empty-catch; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_ignore_all_directive_skips_any_category() {
        let dir = make_repo(&[(
            "a.ts",
            "function f() {\n    // agentsys-ignore-all\n    try {} catch {}\n}\n",
        )]);
        let map = empty_map();
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::EmptyCatch),
            "agentsys-ignore-all should suppress empty-catch; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn suppression_header_directive_skips_file_deletion() {
        // File-deletion fixes (tracked-artifact) are suppressible by an
        // agentsys-ignore directive in the file's first 5 lines.
        let dir = make_repo(&[(
            "debug.log",
            "// agentsys-ignore: tracked-artifact\nthese are intentional log fixtures\n",
        )]);
        let result = slop_fixes(dir.path(), &empty_map());
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::TrackedArtifact),
            "header directive should suppress file-deletion; got {:?}",
            result.fixes
        );
    }

    // ── Commented-out code detector ───────────────

    #[test]
    fn detects_rust_commented_out_function() {
        let src = "\
pub fn real() {}
// fn removed_fn(x: u32) -> u32 {
//     let y = x + 1;
//     y * 2
// }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "expected a commented-out-code fix; got {fixes:?}"
        );
    }

    #[test]
    fn detects_typescript_commented_out_block() {
        let src = "\
export const ok = 1;
/*
function removed(x) {
  return x + 1;
}
*/
";
        let dir = make_repo(&[("src/lib.ts", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "expected a commented-out-code fix; got {fixes:?}"
        );
    }

    #[test]
    fn detects_python_commented_out_block() {
        let src = "\
def real():
    return 1
# def removed(x):
#     y = x + 1
#     return y
";
        let dir = make_repo(&[("mod.py", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "expected a commented-out-code fix; got {fixes:?}"
        );
    }

    #[test]
    fn ignores_short_comment_block() {
        // 2 lines isn't enough — could be a genuine note.
        let src = "\
pub fn ok() {}
// fn removed() {}
// // nothing here really
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "short comment shouldn't flag; got {fixes:?}"
        );
    }

    #[test]
    fn ignores_doc_comment_on_rust_item() {
        // Triple-slash doc comments are attached to items and should
        // never be flagged, even when their content parses as code.
        let src = "\
/// fn example(x: u32) -> u32 {
///     x + 1
/// }
pub fn real() {}
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "doc comments shouldn't flag; got {fixes:?}"
        );
    }

    #[test]
    fn ignores_prose_comment_block() {
        // 3+ lines of prose — shouldn't re-parse as substantive code.
        let src = "\
pub fn ok() {}
// This function computes the next step in the pipeline.
// It is intentionally decoupled from the I/O layer so callers
// can plug in their own transport implementation.
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "prose shouldn't flag; got {fixes:?}"
        );
    }

    #[test]
    fn ignores_todo_marker_comment() {
        // TODO markers are explicitly exempt even when content parses.
        let src = "\
pub fn ok() {}
// TODO: re-enable this once the refactor lands
// fn pending() {
//     do_work();
// }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "TODO marker should suppress; got {fixes:?}"
        );
    }

    #[test]
    fn ignores_todo_marker_on_later_line_of_group() {
        // TODO markers should suppress even when they appear on the
        // second or third line of a merged comment group (before the
        // fix, only the first 40 chars of the concatenated group were
        // checked; a multi-line "cleanup" header could shadow a TODO).
        let src = "\
pub fn ok() {}
// Keeping this around for the next migration.
// TODO: delete once v3 ships
// fn old_impl(x: u32) -> u32 {
//     let y = x + 1;
//     y * 2
// }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "TODO on line 2 should still suppress; got {fixes:?}"
        );
    }

    #[test]
    fn detects_python_indented_commented_out_block() {
        // Python requires the commented-out code to re-parse with its
        // indentation preserved. Before the fix, `trim_start_matches`
        // destroyed leading whitespace, and `def foo():\nbody()` is
        // a syntax error.
        let src = "\
def real():
    return 1

# def removed(x):
#     y = x + 1
#     return y
";
        let dir = make_repo(&[("mod.py", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "indented Python should flag; got {fixes:?}"
        );
    }

    #[test]
    fn ignores_rust_inner_doc_block_comment() {
        // `/*! … */` is a Rust inner-doc block attached to the
        // enclosing item/module. Even if its content parses, we
        // shouldn't flag it.
        let src = "\
/*! fn example(x: u32) -> u32 {
 *      x + 1
 *  }
 */
pub fn real() {}
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            !fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "inner-doc block shouldn't flag; got {fixes:?}"
        );
    }

    #[test]
    fn detects_leading_star_block_comment() {
        // The classic Java/Rust leading-star block layout must strip
        // the ` * ` continuation before re-parsing.
        let src = "\
pub fn ok() {}
/*
 * fn removed(x: u32) -> u32 {
 *     let y = x + 1;
 *     y * 2
 * }
 */
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let fixes = ast_findings(dir.path());
        assert!(
            fixes
                .iter()
                .any(|f| f.category == SlopCategory::CommentedOutCode),
            "leading-star block should flag; got {fixes:?}"
        );
    }

    // ── Stale suppression annotations ─────────────────────

    /// Build a map where `file` imports `imported` from `crate::lib`.
    /// The "lib" segment is chosen so the default src/lib.rs target
    /// file in the stale-suppression tests matches the import-path
    /// resolution check (segment match against "lib").
    fn map_with_import(file: &str, imported: &[&str]) -> RepoIntelData {
        let mut m = empty_map();
        let mut syms = std::collections::HashMap::new();
        syms.insert(
            file.to_string(),
            analyzer_core::types::FileSymbols {
                exports: Vec::new(),
                imports: vec![analyzer_core::types::ImportEntry {
                    from: "crate::lib".to_string(),
                    names: imported.iter().map(|s| s.to_string()).collect(),
                }],
                definitions: Vec::new(),
            },
        );
        m.symbols = Some(syms);
        m
    }

    #[test]
    fn detects_stale_allow_dead_code_on_used_function() {
        let src = "\
#[allow(dead_code)]
pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        // Simulate another file importing `helper`.
        let map = map_with_import("src/consumer.rs", &["helper"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            result.fixes.iter().any(
                |f| f.category == SlopCategory::StaleSuppression && f.reason.contains("helper")
            ),
            "should flag stale allow(dead_code) on an imported symbol; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_allow_dead_code_on_genuinely_dead_symbol() {
        let src = "\
#[allow(dead_code)]
pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        // Another file imports a DIFFERENT name.
        let map = map_with_import("src/consumer.rs", &["other_name"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should NOT flag allow(dead_code) when symbol truly unused; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_allow_dead_code_when_symbol_graph_absent() {
        // Conservative: if map has no symbols section, we can't tell
        // used from unused, so emit nothing rather than risk false
        // positives.
        let src = "\
#[allow(dead_code)]
pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let result = slop_fixes(dir.path(), &empty_map());
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should not flag without symbol graph; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_non_suppression_allow_attributes() {
        // `#[allow(non_snake_case)]` is a style lint, not a usage
        // suppression. Don't touch it even if the symbol is imported.
        let src = "\
#[allow(non_snake_case)]
pub fn Helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let map = map_with_import("src/consumer.rs", &["Helper"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should not flag allow(non_snake_case); got {:?}",
            result.fixes
        );
    }

    #[test]
    fn skips_stale_suppression_in_test_files() {
        // `tests/foo.rs` and `src/*test*.rs` routinely have dead_code
        // helpers. Skip them entirely.
        let src = "\
#[allow(dead_code)]
pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("tests/fixture.rs", src)]);
        let map = map_with_import("src/consumer.rs", &["helper"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should skip test files; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_mixed_lint_allow_with_non_suppression() {
        // `#[allow(non_snake_case, dead_code)]` on a used symbol
        // must NOT be flagged — deleting the line would silently
        // drop the `non_snake_case` lint suppression. Mixed-lint
        // attributes are left alone until we can rewrite them
        // surgically.
        let src = "\
#[allow(non_snake_case, dead_code)]
pub fn Helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let map = map_with_import("src/consumer.rs", &["Helper"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should not flag mixed-lint attribute; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_unrelated_name_collision() {
        // `helper` in src/module_a.rs has #[allow(dead_code)].
        // Another file imports `helper` from `crate::module_b`. Name
        // collision does NOT imply module_a's helper is used.
        let src_a = "\
#[allow(dead_code)]
pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/module_a.rs", src_a)]);
        let mut m = empty_map();
        let mut syms = std::collections::HashMap::new();
        syms.insert(
            "src/consumer.rs".to_string(),
            analyzer_core::types::FileSymbols {
                exports: Vec::new(),
                imports: vec![analyzer_core::types::ImportEntry {
                    // Path clearly targets module_b, NOT module_a.
                    from: "crate::module_b".to_string(),
                    names: vec!["helper".to_string()],
                }],
                definitions: Vec::new(),
            },
        );
        m.symbols = Some(syms);
        let result = slop_fixes(dir.path(), &m);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should not flag helper in module_a when import targets module_b; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_cfg_attr_nested_allow() {
        // `#[cfg_attr(feature = "x", allow(dead_code))]` mentions
        // `allow` only nested inside cfg_attr. It is NOT a top-level
        // `#[allow(...)]` and must not be flagged even when the
        // decorated symbol is imported.
        let src = "\
#[cfg_attr(feature = \"x\", allow(dead_code))]
pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let map = map_with_import("src/consumer.rs", &["helper"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should not flag nested allow inside cfg_attr; got {:?}",
            result.fixes
        );
    }

    #[test]
    fn ignores_inner_allow_attribute() {
        // `#![allow(dead_code)]` is module-scoped; removing it would
        // change the silence shape across the whole file. Leave it.
        let src = "\
#![allow(dead_code)]

pub fn helper(x: u32) -> u32 { x + 1 }
";
        let dir = make_repo(&[("src/lib.rs", src)]);
        let map = map_with_import("src/consumer.rs", &["helper"]);
        let result = slop_fixes(dir.path(), &map);
        assert!(
            !result
                .fixes
                .iter()
                .any(|f| f.category == SlopCategory::StaleSuppression),
            "should not flag inner attribute; got {:?}",
            result.fixes
        );
    }
}
