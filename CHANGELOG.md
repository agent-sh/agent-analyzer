# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/agent-sh/agent-analyzer/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/agent-sh/agent-analyzer/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/agent-sh/agent-analyzer/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/agent-sh/agent-analyzer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/agent-sh/agent-analyzer/releases/tag/v0.1.0
