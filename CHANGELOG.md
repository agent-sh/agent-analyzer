# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-04-25

### Added

- **`passthrough-wrapper` detector** (#31) - new `SlopCategory::PassthroughWrapper`, confidence 0.85. Detects functions whose entire body is a single call to another function with identical args, across Rust, TypeScript/JavaScript, Python, Go, and Java. AST-based: uses tree-sitter `type_parameters` field for generic detection, `trait` field for Rust trait-impl detection, and a captured-receiver comparison for Go method self-delegation.
- **`always-true-condition` detector** (#32) - new `SlopCategory::AlwaysTrueCondition`, confidence 0.92. Flags tautological checks (`if x == x`, `if x === x`) and contradictions (`x && !x`, `x == null && x != null`, `x === null && x !== null`) across all 5 languages. Paren-aware via `split_top_level_op`.
- **`commented-out-code` detector** (#33) - new `SlopCategory::CommentedOutCode`, confidence 0.85. Detects comment blocks whose content re-parses as valid code in the surrounding language. Tree-sitter re-parse with zero-ERROR + substantive-node requirement. Handles leading-star block-comment layout, preserves Python indentation, rejects `///` / `//!` / `/**` / `/*!` doc comments. Per-line marker check skips TODO / FIXME / NOTE / HACK / XXX / WARNING / SAFETY / agentsys-ignore / SPDX-License.
- **`stale-suppression` detector (Rust only)** (#34) - new `SlopCategory::StaleSuppression`, confidence 0.90. Flags `#[allow(dead_code)]` / `#[allow(unused)]` on symbols the import graph proves are used. `UsedSymbols::modules_per_name` segment-matches against the target file's path to guard against name-collision false positives. Only emits when EVERY lint is a suppression candidate (mixed-lint attributes left alone). Skips inner attributes, nested `cfg_attr(..., allow(...))`, and test files.
- **External entry-points detection** (#29) - new `repo-intel query entry-points`: Cargo `[[bin]]` / `[[test]]` / `[[bench]]` / `[[example]]`, `main()` functions via AST, `package.json bin` scripts, framework-loaded configs (Next / Vite / Astro / Svelte / Docusaurus / Tailwind / PostCSS / Rollup / Webpack / Babel / Jest / Vitest / Playwright), and Python `__main__.py`. `.cjs` and `.mjs` variants included. Reduced orphan-export false positives from 52 to 0 on agnix.
- **`agentsys-ignore` suppression directives** (#30) - per-fix suppression via `// agentsys-ignore[: <category>]` and `// agentsys-ignore-all`. Supports `//`, `#`, and `--` markers. 3-line lookback + same-line inline trailing. File-deletion fixes suppressible by a header directive in the first 5 lines.
- **Confidence scores per `SlopCategory`** (#32) - new `Confidence` type alias (f32). `default_confidence(category)` returns per-category defaults (0.75-0.97). Serde default keeps older JSON artifacts compatible.
- **`group_by_file` output** (#32) - `SlopFixesResult` now carries a `by_file: Vec<FileFixes>` alongside flat `fixes`. Deterministic alphabetic sort.
- **Word-boundary cliché matching** (#30) - `tokenize_identifier` uses case transitions and separator boundaries (camelCase / snake_case / kebab-case / PascalCase). O(N) single-pass.
- **`--files` filter on three queries** (#34) - `slop-fixes`, `entry-points`, and `slop-targets` accept `--files a,b,c` to restrict results. Windows paths normalized. New public `SlopAction::path()` accessor + `slop_targets::retain_targets_touching` helper.

### Changed

- **`SlopAction::path()` signature** (#34) - now returns `&str` (total) instead of `Option<&str>`. Every variant has a path.
- **`category_kebab` hot path** (#30) - returns `&'static str` from a match arm instead of `serde_json::to_value` per call.

### Fixed

- Addressed 19+ reviewer-flagged items across the 6 slop PRs: single-pass partition loops, parser hoisting, AST field checks replacing string scans, shebang handling for Hash-style comments, paren-aware operator splitting, `strip_one_paren_pair` depth handling, `CommentKind` line/block tracking, nested `cfg_attr` allow guard, name-collision guard on stale-suppression, mixed-lint all-suppression requirement.

## [0.6.0] - 2026-04-24

### Added

- **`analyzer-embed` crate + binary** (#28) - new workspace member producing the `agent-analyzer-embed` binary. Provides a standalone embedding server so external agents can generate and store vector embeddings via the `set-embeddings` subcommand. The release workflow now builds and uploads `agent-analyzer-embed` alongside `agent-analyzer` for all 5 platforms (10 release assets total).
- **`set-embeddings` subcommand** (#28) - accepts JSON via stdin to store vector embeddings in the artifact. Complements the existing `set-descriptors` and `set-summary` subcommands.
- **`query slop-fixes` subcommand** (#28) - scans staged/committed diffs for AI slop patterns that were introduced and then immediately reverted or corrected, ranking files by slop-fix frequency.
- **`query slop-targets` subcommand** (#28) - identifies files most likely to contain residual AI slop based on authorship signals and pattern density, used by the deslop agent to prioritise its work queue.
- **Per-language detectors** (#28) - dedicated AST-based detectors for empty error swallowing and tautological assertions across Rust, TypeScript, JavaScript, Python, Go, and Java. Feeds into the slop-targets ranking.
- **Rust mod-decl resolver** (#28) - resolves `mod foo;` declarations to their canonical file path, eliminating orphan-export false positives for Rust workspaces. Reduced false positives by ~95% on the agnix codebase.
- **TS/JS import resolver** (#28) - resolves ES module and CommonJS `require`/`import` statements to file paths, including index-file and extension-less resolution, for the same orphan-export fix.
- **Python import resolver** (#28) - resolves absolute and relative Python imports (`from . import`, `from pkg import mod`) to file paths.

## [0.5.0] - 2026-04-24

### Changed (BREAKING)

- **Drop all AI attribution detection** (#17) - `aiAttribution`, `aiRatio`, and `recentAi` query and CLI subcommands removed. The surface conflated bot commits with human AI authorship and produced misleading ratios. Bot detection is now isolated in its own `bot_detect` module so the human/bot contributor split still works. The diff-risk formula has been adjusted to drop the removed AI-ratio term.

### Added

- **`bug_fix_detect` module** (#18) - broadened bug-fix classification beyond Conventional Commit `fix:` prefix. Now recognises plain-English fix subjects ("Fix race condition", "Resolves #42", "hotfix for prod"), keyword variants, and issue-closure phrases. Whole-word matching guards against `prefix`/`suffix`/`unfixable` false positives.
- **`generated_detect` module** (#19) - suppresses bug-fix attribution for auto-generated files. `fix(schema)` commits no longer pollute bugspots scores through their generated `.pb.go`/`.d.ts` bindings. New `FileActivity.generated` field with `#[serde(default)]` for backward compatibility.
- **`entry-points` query** (#23) - lists every place execution can start: binaries, `main` functions, npm scripts. Workspace-aware Cargo handling included.
- **`find <concept>` query** (#24) - deterministic concept-to-file search. Collapses `grep -r` into ranked output with a one-line "why" per result.
- **`set-descriptors` and `set-summary` subcommands** (#25) - accept JSON via stdin to store LLM-produced file descriptors and repo summaries in the artifact. New `summary` query reads the stored summary. `find` becomes descriptor-aware, using stored semantic descriptors to surface results beyond lexical matches. The Rust crate remains offline-only; descriptors and summaries are populated by external Haiku agents via the repo-intel JS plugin.

## [0.4.0] - 2026-04-23

### Added

- **`analyzer-graph` crate** (#14, #15) - new workspace member providing graph-derived analytics on top of existing `RepoIntelData`. Reads the already-collected `coupling` + `file_activity` data and produces a sparse undirected weighted file-file graph (Jaccard similarity over commit co-occurrence), runs Louvain modularity-maximisation community detection, and computes per-node betweenness centrality via Brandes' algorithm. All thresholds are calibrated from the co-change graph literature (Zimmermann et al., Hassan & Holt) and require no flags.
- **4 new `repo-intel query` subcommands** (#14):
  - `communities` - lists discovered communities sorted by size
  - `boundaries [--top N]` - high-betweenness files (architectural seams between communities)
  - `area-of <file>` - looks up which community a file belongs to
  - `community-health <id>` - composite per-community roll-up (size, total/recent changes, bug-fix rate, AI ratio, stale-owner count)
- **`RepoIntelData.graph: Option<GraphData>`** - new optional field with three reserved sub-graphs (`cochange`, `import`, `author`). `cochange` ships in this release; the other two have data slots reserved for future phases so older readers stay forward-compatible.
- **Phase 5 finalize step** runs automatically after `init` and `update` once all collectors complete. Smoke tests on agnix (~9k surviving edges, 46 communities) finish in well under a second.

### Changed

- **Brandes' betweenness parallelised** via rayon `into_par_iter().fold().reduce()` over source nodes (#14). Each worker keeps its own `Scratch` (pre-allocated stack/predecessors/sigma/distance/delta/queue/bc) so the previous O(V²) per-call allocation pattern is gone. Output stays deterministic - addition is commutative.
- **Louvain `State` rewritten to use `Vec<f64>` instead of `HashMap<u32, f64>`** for `comm_total` and `comm_internal` (#15). Community ids are bounded by `n` for the entire run, so dense-vector indexing is correct and saves the per-access hash. Per-node `weights_to` accumulator is now a single reused `Vec<f64>` with a `dirty_comms` index list, eliminating per-node HashMap allocation. Per-node self-loop weights pre-computed once in `State::new` instead of being re-summed inside the local-moves loop.

### Fixed

- **Self-loop bookkeeping in Louvain `State::new`** (#15) - `comm_internal` is now seeded with each node's self-loop weight rather than zero. Each node initially sits in its own singleton community, and the only edge that can be internal to a 1-node community is a self-loop on that node. Pre-fix the algorithm silently dropped self-loop contributions until the affected node first moved. (No observable effect on co-change graphs, which never have self-loops, but the algorithm is now correct in general.)
- **Rust 1.95 clippy** (`unnecessary_sort_by`) in `analyzer-collectors`, `analyzer-sync-check`, `analyzer-git-map` - replaced descending `sort_by` patterns with `sort_by_key(... Reverse)`. Local toolchain (1.92) didn't flag these but CI's stricter 1.95 did.
- **`petgraph` dependency** moved from per-crate spec to `[workspace.dependencies]` to match the convention used for serde, rayon, chrono.
- **Pre-existing unused import** (`parse_source` in `analyzer-repo-map/complexity.rs`) that was already breaking `cargo clippy --workspace --all-targets -- -D warnings` on main.

## [0.3.2] - 2026-03-22

### Added

- `stale` and `recent_activity` fields to `ContributorEntry` in `contributors` query output (Track D)

### Fixed

- Guard division-by-zero in `bugspots()` when a file has zero total changes

## [0.3.1] - 2026-03-22

### Added

- `painspots` query: ranks files by `hotspot_score * (1 + bug_fix_rate) * (1 + complexity/30)` - identifies files most likely to cause problems
- `complexity_median`, `complexity_max`, and `total_symbols` fields to `AreaEntry` in `areas` query (computed from Phase 2 AST data when available)
- Graceful fallback in `painspots` when Phase 2 AST data is absent (git-only scoring)

## [0.3.0] - 2026-03-22

### Added

- Phase 2-4 implementation: AST symbol extraction, project metadata, and doc-code sync detection
- `symbols` query: exports, imports, and definitions (with complexity) for a specific file
- `dependents` query: reverse dependency lookup - finds all files importing a given symbol
- `stale-docs` query: identifies documentation files diverged from their associated source files
- `project-info` query: project-level metadata (name, description, languages, version, repository)
- 142 tests across all query types

### Changed

- Renamed cache file from `git-map.json` to `repo-intel.json` to reflect the broader artifact scope

## [0.2.0] - 2026-03-16

### Added

- `onboard` query: surfaces good-first areas, contributor signals, and project orientation data
- `can-i-help` query: matches contributor skills to areas needing work (test gaps, stale docs, bugspots)
- Multi-language support in `onboard` query (JS, TS, Python, Rust, Go, Java)
- 77 tests passing (Phase 1 complete)

### Fixed

- Query tuning based on 16-repo validation - improved scoring and signal quality
- `onboard` query expanded for accurate cross-language detection

## [0.1.0] - 2026-03-15

### Added

- Initial release: core `analyzer-git-map` and `analyzer-cli` crates
- Phase 1 git history analysis: 19 query types via `repo-intel` CLI subcommand
- Queries: `hotspots`, `coldspots`, `file-history`, `bugspots`, `test-gaps`, `diff-risk`, `ownership`, `contributors`, `bus-factor`, `coupling`, `norms`, `conventions`, `areas`, `health`, `release-info`, `ai-ratio`, `recent-ai`, `onboard`, `can-i-help`, `doc-drift`
- Cached artifact `repo-intel.json` stored in `.claude/`, `.opencode/`, or `.codex/` state directory
- Incremental update support via `repo-intel update --map-file`
- Query flags: `--min-changes`, `--path-filter`, `--adjust-for-ai`
- 68 tests at launch

[Unreleased]: https://github.com/agent-sh/agent-analyzer/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/agent-sh/agent-analyzer/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/agent-sh/agent-analyzer/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/agent-sh/agent-analyzer/releases/tag/v0.1.0
