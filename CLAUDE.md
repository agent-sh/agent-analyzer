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
      types.rs          # GitMapData, Contributors, FileActivity, CouplingEntry, AiSignal, CommitDelta, etc.
      git.rs            # git2 wrapper: open_repo, walk_commits, get_commit_diff_stats, renames, deletions
      ai_detect.rs      # AI commit detection using embedded signature registry
      ai_signatures.json # Updateable AI tool signatures (trailers, emails, patterns, bots)
      walk.rs           # File walking with noise filtering (lockfiles, dist, build, vendor)
      output.rs         # JSON serialization (pretty + compact)
  analyzer-git-map/     # Git history analysis
    src/
      extractor.rs      # extract_full(), extract_delta() using git2
      aggregator.rs     # create_empty_map(), merge_delta() - full spec implementation
      queries.rs        # hotspots, coldspots, coupling, ownership, bus_factor, ai_ratio, etc.
      incremental.rs    # check_status(), needs_rebuild(), get_since_sha()
  analyzer-repo-map/    # Stub (Phase 2)
  analyzer-collectors/  # Stub (Phase 3)
  analyzer-sync-check/  # Stub (Phase 4)
  analyzer-cli/         # Unified CLI binary
    src/
      main.rs           # clap dispatch
      commands/
        git_map.rs      # init, update, status, query subcommands
        repo_map.rs     # stub
        collect.rs      # stub
        sync_check.rs   # stub
.github/workflows/
  ci.yml                # cargo test + clippy + fmt
  release.yml           # 5-target cross-platform build, GitHub release
```

### Key Types (analyzer-core::types)

```rust
GitMapData              // Full JSON output artifact
  git: GitInfo          // analyzedUpTo, totalCommitsAnalyzed, dates, scope, shallow
  contributors: Contributors  // humans (HashMap<String, HumanContributor>), bots
  file_activity: HashMap<String, FileActivity>  // per-file: changes, authors, ai metrics
  dir_activity: HashMap<String, DirActivity>    // per-directory aggregates
  coupling: HashMap<String, HashMap<String, CouplingEntry>>  // co-change pairs
  commit_shape: CommitShape   // size distribution, files per commit, merge count
  conventions: ConventionInfo // conventional commit prefixes, style, samples
  ai_attribution: AiAttribution // attributed/heuristic/none counts, per-tool breakdown
  releases: Releases          // tags, cadence
  renames: Vec<RenameEntry>   // file rename tracking
  deletions: Vec<DeletionEntry> // file deletion tracking

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

### Query API (analyzer-git-map::queries)

All queries operate on the cached `GitMapData` - no git commands needed.

| Function | Returns | Notes |
|----------|---------|-------|
| `hotspots(map, months, limit)` | `Vec<HotspotEntry>` | Sorted by change count |
| `coldspots(map, months)` | `Vec<ColdspotEntry>` | Sorted by last_changed ascending |
| `coupling(map, file, human_only)` | `Vec<CouplingResult>` | Bidirectional lookup |
| `ownership(map, path)` | `OwnershipResult` | Primary owner + contributors |
| `bus_factor(map, adjust_for_ai)` | `usize` | People covering 80% of commits |
| `contributors(map, months)` | `Vec<ContributorEntry>` | Sorted by commit count |
| `ai_ratio(map, path_filter)` | `AiRatioResult` | Repo-wide or per-path |
| `release_info(map)` | `ReleaseInfo` | Cadence, last release, unreleased |
| `health(map)` | `HealthResult` | Active, bus_factor, frequency, ai_ratio |
| `file_history(map, path)` | `Option<&FileActivity>` | Single file lookup |
| `commit_shape(map)` | `CommitShapeResult` | Typical size, files per commit |
| `conventions(map)` | `ConventionResult` | Style, prefixes, scopes |

### Noise Filtering (analyzer-core::walk)

Excluded from coupling and hotspot analysis:
- `package-lock.json`, `yarn.lock`, `Cargo.lock`, `go.sum`, `pnpm-lock.yaml`
- `.min.js`, `.min.css`
- `dist/`, `build/`, `vendor/`

### CLI Interface

```
agent-analyzer --version
agent-analyzer git-map init [--since=<date>] [--max-commits=N] <path>
agent-analyzer git-map update <path>
agent-analyzer git-map status <path>
agent-analyzer git-map query hotspots [--top=N] <path>
agent-analyzer git-map query coupling <file> <path>
agent-analyzer git-map query ownership <file> <path>
agent-analyzer git-map query bus-factor [--adjust-for-ai] <path>
agent-analyzer repo-map ...    # "not yet implemented"
agent-analyzer collect ...     # "not yet implemented"
agent-analyzer sync-check ...  # "not yet implemented"
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
cargo test                            # Run all tests (43 tests)
cargo build --release                 # Build release binary
cargo clippy -- -D warnings           # Lint (treat warnings as errors)
cargo fmt --check                     # Format check
cargo run --bin agent-analyzer -- --version  # Run CLI
```

## Current State

- v0.1.0 - Phase 1 complete (core + git-map + CLI)
- 43 passing tests (24 analyzer-core, 19 analyzer-git-map)
- Stub crates: analyzer-repo-map, analyzer-collectors, analyzer-sync-check
- CI: cargo test + clippy + fmt on push/PR
- Release: 5-target cross-platform builds on tag push

## Phased Roadmap

| Phase | Crate | Status | Description |
|-------|-------|--------|-------------|
| 1 | analyzer-core, analyzer-git-map, analyzer-cli | Done | Git history analysis |
| 2 | analyzer-repo-map | Stub | AST-based symbol mapping (embed ast-grep) |
| 3 | analyzer-collectors | Stub | Project data gathering (docs, codebase, github) |
| 4 | analyzer-sync-check | Stub | Doc-code sync analysis |

## Integration

This binary is consumed by JS plugins via the binary resolver in `agent-core/lib/binary/`:
- JS calls `binary.ensureBinary()` which auto-downloads from GitHub releases
- Binary location: `~/.agent-sh/bin/agent-analyzer[.exe]`
- Distribution: lazy download on first use, no manual install

Consumers:
- `git-map` plugin (JS wrapper for `/git-map` command)
- `repo-map` plugin (Phase 2 - will replace `ast-grep` subprocess)
- `agent-core/lib/collectors/` (Phase 3 - will replace JS implementations)
- `sync-docs` plugin (Phase 4 - will replace JS analysis)

## References

- Part of the [agent-sh](https://github.com/agent-sh) ecosystem
- Spec: `agent-knowledge/git-map-spec.md`
- AI detection: `agent-knowledge/ai-commit-detection-forensics.md`
- Git analysis research: `agent-knowledge/git-history-analysis-developer-tools.md`
- https://agentskills.io
