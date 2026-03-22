# Project Memory: agent-analyzer

> Shared Rust binary for static analysis in the agent-sh ecosystem. Extracts temporal, social, and behavioral signals from git history, AST-based symbol maps, project data, and doc-code sync.

**Repository**: https://github.com/agent-sh/agent-analyzer

## Project Instruction Files

- `CLAUDE.md` is the project memory entrypoint for Claude Code.
- `AGENTS.md` is a byte-for-byte copy of `CLAUDE.md` for tools that read `AGENTS.md` (Codex CLI, OpenCode, Cursor, Cline, Copilot).
- Keep them identical.

## Critical Rules

1. **Rust workspace** - analyzer-core (shared), analyzer-git-map (history), analyzer-repo-map (AST), analyzer-collectors (data), analyzer-sync-check (docs), analyzer-cli (binary)
2. **ai_signatures.json is updateable data** - Add new AI tool signatures there, not in code. Embedded via `include_str!()` at compile time.
3. **Plain text output** - No emojis, no ASCII art. Use `[OK]`, `[ERROR]`, `[WARN]`, `[CRITICAL]` for status markers.
4. **All JSON output to stdout** - Progress and errors go to stderr. Consumers parse stdout.
5. **Release binaries** - Compile with LTO, strip symbols (profile.release in workspace Cargo.toml)
6. **Task is not done until tests pass** - Every feature/fix must have quality tests.
7. **Create PRs for non-trivial changes** - No direct pushes to main.
8. **Always run git hooks** - Never bypass pre-commit or pre-push hooks.
9. **No unnecessary files** - Don't create summary files, plan files, audit files, or temp docs.
10. **Use single dash for em-dashes** - In prose, use ` - ` (single dash with spaces), never ` -- `.
11. **Address all PR review comments** - Even minor ones. If you disagree, respond in the review thread.

## Architecture

### Crate Dependency Graph

```
analyzer-core (shared types, git2 wrapper, AI detection, file walking, JSON output)
    |
    v
+-- analyzer-git-map (git history extraction, aggregation, queries, incremental)
+-- analyzer-repo-map (AST-based symbol mapping - stub)
+-- analyzer-collectors (project data gathering - stub)
+-- analyzer-sync-check (doc-code sync analysis - stub)
    |
    v
analyzer-cli (unified binary, clap dispatch, depends on all above)
```

### Project Layout

```
crates/
  analyzer-core/        # Shared library
    src/
      types.rs          # RepoIntelData, Contributors, FileActivity, CouplingEntry, AiSignal, CommitDelta
      git.rs            # git2 wrapper: open_repo, walk_commits, get_commit_diff_stats, renames, deletions
      ai_detect.rs      # AI commit detection using embedded signature registry
      ai_signatures.json # Updateable AI tool signatures (trailers, emails, patterns, bots)
      walk.rs           # File walking with noise filtering (lockfiles, dist, build, vendor)
      output.rs         # JSON serialization (pretty + compact)
  analyzer-git-map/     # Git history analysis
    src/
      extractor.rs      # extract_full(), extract_delta() using git2
      aggregator.rs     # create_empty_map(), merge_delta() - full spec implementation
      queries.rs        # hotspots, bugspots, ownership, bus_factor, areas, norms, coupling, etc.
      incremental.rs    # check_status(), needs_rebuild(), get_since_sha()
  analyzer-repo-map/    # AST symbol extraction (Phase 2)
    src/
      parser.rs         # Language detection, tree-sitter grammar init (6 languages)
      extractor.rs      # Walk files, parse, extract symbols (exports, imports, definitions, fields)
      complexity.rs     # Cyclomatic complexity via AST branch-point counting
      conventions.rs    # Naming pattern detection (snake_case, PascalCase), test framework detection
      queries.rs        # symbols(), dependents() queries
  analyzer-collectors/  # Project metadata (Phase 3)
    src/
      readme.rs         # README detection and heading extraction
      ci.rs             # CI provider detection (GitHub Actions, GitLab CI, etc.)
      license.rs        # License detection (SPDX from manifests + file pattern matching)
      languages.rs      # Language distribution by file extension
  analyzer-sync-check/  # Doc-code cross-reference (Phase 4)
    src/
      parser.rs         # Markdown parsing with pulldown-cmark, code ref extraction
      matcher.rs        # Symbol matching against AST symbol table, camelCase-to-snake_case
      checker.rs        # Staleness detection (deleted, renamed, hotspot references)
      queries.rs        # stale_docs(), build_doc_refs()
  analyzer-cli/         # Unified CLI binary
    src/
      main.rs           # clap dispatch
      commands/
        repo_intel.rs   # init, update, status, query subcommands
        repo_map.rs     # stub
        collect.rs      # stub
        sync_check.rs   # stub
.github/workflows/
  ci.yml                # cargo test + clippy + fmt
  release.yml           # 5-target cross-platform build, GitHub release
```

### Key Types (analyzer-core::types)

```rust
RepoIntelData           // Full JSON output artifact (repo-intel.json)
  git: GitInfo          // analyzedUpTo, totalCommitsAnalyzed, dates, scope, shallow
  contributors: Contributors  // humans (HashMap<String, HumanContributor>), bots
  file_activity: HashMap<String, FileActivity>  // per-file: changes, recent_changes, authors, ai metrics
  coupling: HashMap<String, HashMap<String, CouplingEntry>>  // co-change pairs
  conventions: ConventionInfo // conventional commit prefixes, style, usesScopes
  ai_attribution: AiAttribution // attributed/heuristic/none counts, per-tool breakdown, confidence
  releases: Releases          // tags, cadence
  renames: Vec<RenameEntry>   // file rename tracking
  deletions: Vec<DeletionEntry> // file deletion tracking

FileActivity            // Per-file metrics
  changes, recent_changes, authors, created, last_changed
  additions, deletions, ai_changes, ai_additions, ai_deletions
  bug_fix_changes, refactor_changes, last_bug_fix

HumanContributor        // commits, recent_commits, first_seen, last_seen, ai_assisted_commits
BotContributor          // commits, recent_commits, first_seen, last_seen
CommitDelta             // Raw extraction output (commits, renames, deletions)
CommitInfo              // Parsed commit (hash, author, date, subject, body, trailers, files)
AiSignal                // Detection result (detected, tool, method)
CommitSize              // Tiny(<10), Small(10-50), Medium(50-200), Large(200-500), Huge(>500)
```

### AI Detection Pipeline (analyzer-core::ai_detect)

Check order (highest confidence first):
1. Trailer emails (Co-Authored-By containing known AI emails)
2. Author emails (known AI tool domains)
3. Bot authors (exact name match: `dependabot[bot]`, `renovate[bot]`, etc.)
4. Author name patterns (regex: `\(aider\)$`, `\[bot\]$`)
5. Message body patterns ("Generated with Claude Code", "^aider: ")
6. Trailer names (Co-Authored-By name field: Claude, Cursor, Copilot, etc.)

Signatures loaded from embedded `ai_signatures.json` - update that file to add new tools.

Note: AI detection confidence is "low" - metadata-based detection catches <15% of AI commits. Phase 5 will add code stylometry for higher accuracy.

### Query API (analyzer-git-map::queries)

All queries operate on the cached `RepoIntelData` - no git commands needed.

| Function | Returns | Notes |
|----------|---------|-------|
| `hotspots(map, _months, limit)` | `Vec<HotspotEntry>` | Recency-weighted score, sorted by score. `months` reserved (90-day window is snapshot-relative) |
| `coldspots(map, _months)` | `Vec<ColdspotEntry>` | Sorted by last_changed ascending. `months` reserved |
| `bugspots(map, limit)` | `Vec<BugspotEntry>` | Bug-fix density (fixes/changes ratio) |
| `coupling(map, file, human_only)` | `Vec<CouplingResult>` | Bidirectional lookup |
| `ownership(map, path)` | `OwnershipResult` | With staleness, bus_factor_risk |
| `bus_factor(map, adjust_for_ai)` | `usize` | People covering 80% of commits |
| `bus_factor_detailed(map, adjust_for_ai)` | `BusFactorResult` | With critical_owners, at_risk_areas |
| `norms(map)` | `NormsResult` | Commit conventions (Phase 2 adds code norms) |
| `areas(map)` | `Vec<AreaEntry>` | Directory-level health (healthy/needs-attention/at-risk) |
| `contributors(map, months)` | `Vec<ContributorEntry>` | Sorted by commit count |
| `ai_ratio(map, path_filter)` | `AiRatioResult` | Repo-wide or per-path |
| `release_info(map)` | `ReleaseInfo` | Cadence, last release, unreleased |
| `health(map)` | `HealthResult` | Active, bus_factor, frequency, ai_ratio |
| `file_history(map, path)` | `Option<&FileActivity>` | Single file lookup |
| `conventions(map)` | `ConventionResult` | Style, prefixes, scopes |
| `test_gaps(map, min_changes, limit)` | `Vec<TestGapEntry>` | Hot files with no co-changing test file |
| `diff_risk(map, files)` | `Vec<DiffRiskEntry>` | Score file list by composite risk |
| `doc_drift(map, limit)` | `Vec<DocDriftEntry>` | Doc files with low code coupling |
| `recent_ai(map, limit)` | `Vec<RecentAiEntry>` | Files with recent AI changes |
| `onboard(map)` | `OnboardResult` | Newcomer-oriented repo summary (structure, key areas, pain points) |
| `can_i_help(map)` | `CanIHelpResult` | Contributor guidance (good-first areas, needs-help areas) |

### Recency and Staleness

- **Recency window**: 90 days relative to repo's `last_commit_date` (snapshot-relative, not wall clock)
- **recent_changes/recent_commits**: Counted within the 90-day window
- **Stale**: A contributor is stale if their `last_seen` is >90 days before `last_commit_date`
- **Hotspot score**: `(recent_changes * 2 + total_changes) / (total_changes + 1)`

### Noise Filtering (analyzer-core::walk)

Excluded from coupling and hotspot analysis:
- `package-lock.json`, `yarn.lock`, `Cargo.lock`, `go.sum`, `pnpm-lock.yaml`
- `.min.js`, `.min.css`
- `dist/`, `build/`, `vendor/`

### CLI Interface

```
agent-analyzer --version
agent-analyzer repo-intel init [--max-commits=N] <path>
agent-analyzer repo-intel update --map-file=<file> <path>
agent-analyzer repo-intel status --map-file=<file> <path>
agent-analyzer repo-intel query hotspots [--top=N] --map-file=<file> <path>
agent-analyzer repo-intel query coldspots [--top=N] --map-file=<file> <path>
agent-analyzer repo-intel query bugspots [--top=N] --map-file=<file> <path>
agent-analyzer repo-intel query coupling <file> --map-file=<file> <path>
agent-analyzer repo-intel query ownership <file> --map-file=<file> <path>
agent-analyzer repo-intel query bus-factor [--adjust-for-ai] --map-file=<file> <path>
agent-analyzer repo-intel query norms --map-file=<file> <path>
agent-analyzer repo-intel query areas --map-file=<file> <path>
agent-analyzer repo-intel query contributors [--top=N] --map-file=<file> <path>
agent-analyzer repo-intel query ai-ratio [--path-filter=<path>] --map-file=<file> <path>
agent-analyzer repo-intel query release-info --map-file=<file> <path>
agent-analyzer repo-intel query health --map-file=<file> <path>
agent-analyzer repo-intel query file-history <file> --map-file=<file> <path>
agent-analyzer repo-intel query conventions --map-file=<file> <path>
agent-analyzer repo-intel query test-gaps [--top=N] [--min-changes=N] --map-file=<file> <path>
agent-analyzer repo-intel query diff-risk --files=<a,b,c> --map-file=<file> <path>
agent-analyzer repo-intel query doc-drift [--top=N] --map-file=<file> <path>
agent-analyzer repo-intel query recent-ai [--top=N] --map-file=<file> <path>
agent-analyzer repo-intel query onboard --map-file=<file> <path>
agent-analyzer repo-intel query can-i-help --map-file=<file> <path>
agent-analyzer repo-map generate <path>
agent-analyzer repo-map symbols <file> --map-file=<file>
agent-analyzer repo-map dependents <symbol> [--file=<file>] --map-file=<file>
agent-analyzer collect run <path>
agent-analyzer sync-check check <path> --map-file=<file>
agent-analyzer sync-check stale-docs <path> [--top=N] --map-file=<file>
```

### Build Targets

5 targets (same as agnix):
- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-gnu` (via cross)
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

## Commands

```bash
cargo check                           # Compile check
cargo test                            # Run all tests (142 tests)
cargo build --release                 # Build release binary
cargo clippy -- -D warnings           # Lint (treat warnings as errors)
cargo fmt --check                     # Format check
cargo run -p analyzer-cli -- --version  # Run CLI
```

## Current State

- Phase 1-4 complete
- 142 passing tests (24 analyzer-core, 53 analyzer-git-map, 30 analyzer-repo-map, 16 analyzer-collectors, 19 analyzer-sync-check)
- CI: cargo test + clippy + fmt on push/PR
- Release: 5-target cross-platform builds on tag push

## Phased Roadmap

| Phase | Crate | Status | Description |
|-------|-------|--------|-------------|
| 1 | analyzer-core, analyzer-git-map, analyzer-cli | Complete | Git intelligence (recency, staleness, bugspots, norms, areas, onboard, can-i-help) |
| 2 | analyzer-repo-map | Complete | AST symbol extraction (tree-sitter, 6 languages: Rust, TS, JS, Python, Go, Java) |
| 3 | analyzer-collectors | Complete | Project metadata (README, CI, license, languages, package manager) |
| 4 | analyzer-sync-check | Complete | Doc-code cross-reference (inline code matching, hotspot detection, staleness) |
| 5 | analyzer-core | Planned | AI code stylometry (replace metadata-based detection) |

## Integration

This binary is consumed by JS plugins via the binary resolver in `agent-core/lib/binary/`:
- JS calls `binary.ensureBinary()` which auto-downloads from GitHub releases
- Binary location: `~/.agent-sh/bin/agent-analyzer[.exe]`
- Distribution: lazy download on first use, no manual install

Consumers:
- `git-map` plugin (JS wrapper using `repo-intel` CLI namespace)
- `repo-map` plugin (uses `repo-map` CLI for AST symbol extraction)
- `agent-core/lib/collectors/` (uses `collect` CLI for project metadata)
- `sync-docs` plugin (uses `sync-check` CLI for doc-code cross-references)

## References

- Part of the [agent-sh](https://github.com/agent-sh) ecosystem
- Spec: `agent-analyzer/SPEC.md`
- AI detection: `agent-knowledge/ai-commit-detection-forensics.md`
- Git analysis research: `agent-knowledge/git-history-analysis-developer-tools.md`
- https://agentskills.io
