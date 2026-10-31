# agent-analyzer

[![CI](https://github.com/agent-sh/agent-analyzer/actions/workflows/ci.yml/badge.svg)](https://github.com/agent-sh/agent-analyzer/actions/workflows/ci.yml)
[![Release](https://github.com/agent-sh/agent-analyzer/actions/workflows/release.yml/badge.svg)](https://github.com/agent-sh/agent-analyzer/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Static analysis binary for the [agent-sh](https://github.com/agent-sh) ecosystem. Extracts temporal, social, and behavioral signals from git history plus AST symbols, project metadata, doc-code sync, and graph-derived community structure.

Produces a single cached JSON artifact that answers questions like "which files change together?", "who owns this module?", "where does this concept live?", and "what does this repo actually do?" - without touching git again after the initial scan.

## Why this project

- Use this when you need git-based code intelligence (hotspots, coupling, ownership, bus factor) without shelling out to `git log` on every query
- Use this when an agent (or you) needs a fast first-foothold in an unfamiliar repo (`find <concept>`, `entry-points`, `summary`)
- Use this when you need incremental updates - only process new commits since the last scan
- Use this when building developer tools that need structured repository history as JSON
- Use this when you want a Rust crate that stores LLM-augmented signals (per-file descriptors, narrative summary) but never makes the LLM calls itself - the data flows in via `set-descriptors` / `set-summary` from whatever orchestrator you choose

## Installation

### Pre-built binaries

Download from [GitHub Releases](https://github.com/agent-sh/agent-analyzer/releases) for your platform:

| Platform | Target |
|----------|--------|
| Linux x64 | `x86_64-unknown-linux-gnu` |
| Linux x64 (static) | `x86_64-unknown-linux-musl` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS ARM64 | `aarch64-apple-darwin` |
| Windows x64 | `x86_64-pc-windows-msvc` |

### From source

```bash
cargo install --path crates/analyzer-cli
```

### Via agent-sh plugins

If you use the [deslop](https://github.com/agent-sh/deslop) plugin, the binary is downloaded automatically on first use. No manual install needed.

## Quick start

```bash
# Scan a repository's full git history
agent-analyzer repo-intel init ./my-repo > repo-intel.json

# Query the cached result - no git access needed
agent-analyzer repo-intel query hotspots ./my-repo --map-file repo-intel.json --top 5
```

Output (JSON to stdout):

```json
[
  { "path": "src/engine.rs", "changes": 142, "recentChanges": 31, "score": 1.43, "authors": ["alice", "bob"], "bugFixes": 12 },
  { "path": "src/api/handler.rs", "changes": 98, "recentChanges": 20, "score": 1.39, "authors": ["carol"], "bugFixes": 7 }
]
```

Progress and errors go to stderr, so piping and redirection work as expected.

## Core concepts

**Two-phase architecture**: First, `init` or `update` walks git history and produces a `RepoIntelData` JSON artifact. Then, `query` subcommands operate entirely on that cached artifact - no repository access required.

**Incremental by default**: After the initial scan, `update` only processes commits after `analyzedUpTo`. If a force-push is detected (the recorded SHA is no longer in history), it falls back to a full rebuild automatically.

**Offline-only**: The binary never makes network or LLM calls. Per-file descriptors and the 3-depth narrative summary that `find` and `summary` consume are written in by external orchestrators via `set-descriptors` / `set-summary`. Embeddings come from the separate `agent-analyzer-embed` binary and are stored with `set-embeddings`.

**Bot-aware, not AI-attribution**: A `bot_detect` module isolates automation accounts (`dependabot`, `renovate`, `github-actions`) from human contributors. AI authorship attribution was removed in v0.5.0 because it conflated bots with model-assisted authoring and produced misleading ratios.

## Features

- **Git history extraction** - commit metadata, per-file diff stats, rename tracking, deletion tracking via libgit2 (no subprocess calls)
- **Bot detection** - isolates Dependabot, Renovate, GitHub Actions, and other automation from human-authored commits
- **Bug-fix classification** - recognises `fix:` prefix plus plain-English fix subjects ("Fix race", "Resolves #42", "hotfix"), keyword variants, and issue-closure phrases; suppresses attribution for auto-generated files (`.pb.go`, `.d.ts`, `*/generated/*`) so schema-fix commits don't pollute bugspots
- **Hotspot analysis** - find the most frequently changed files, recency-weighted
- **Coupling analysis** - discover files that change together (co-change frequency with configurable thresholds)
- **Co-change communities** - Louvain modularity-maximisation on the file-file Jaccard graph; betweenness centrality identifies bridge files
- **Ownership queries** - primary author, contributor breakdown, and bus factor per file or directory; bots are excluded from bus-factor counts
- **Convention detection** - conventional commit style, prefix frequency, scope patterns
- **Release tracking** - tag-based release cadence, unreleased commit count
- **Health scoring** - composite metric combining activity, bus factor, and bug-fix rate
- **Noise filtering** - automatically excludes lockfiles, minified assets, `dist/`, `build/`, `vendor/` from analysis
- **AST symbol map** - per-file exports/imports/definitions plus reverse-dependency lookup via tree-sitter (Rust, TS/JS, Python, Go, Java)
- **Concept search** - `find <concept>` ranks files by deterministic signals (basename/path/symbol/import/doc-header) plus optional LLM-generated descriptor matching for semantic recall
- **Repo summary** - cached 3-depth narrative description (sentence / paragraph / page), populated by external orchestrator
- **Entry-point listing** - every place execution can start: binaries (`Cargo.toml [[bin]]`, `package.json bin`, `pyproject scripts`), AST `main` functions, npm `scripts`, with Cargo workspace awareness
- **Slop detection** - `slop-fixes` returns self-contained fix actions (tracked artifacts, stale CI configs, duplicate tooling, orphan exports, empty catch blocks, tautological tests, passthrough wrappers, commented-out code) and `slop-targets` ranks files for the [deslop](https://github.com/agent-sh/deslop) agent's deeper scans
- **Local embeddings** - the separate `agent-analyzer-embed` binary embeds files or functions with fastembed (`--variant small` = BGE-small, `big` = EmbeddingGemma-300M) and re-embeds only changed files on `update`

## Usage

### Full scan

```bash
agent-analyzer repo-intel init /path/to/repo > repo-intel.json
```

### Incremental update

```bash
agent-analyzer repo-intel update /path/to/repo --map-file repo-intel.json > repo-intel-updated.json
```

### Check status

```bash
agent-analyzer repo-intel status /path/to/repo --map-file repo-intel.json
```

Returns `valid`, `stale`, or `invalid` (force-push detected).

### Queries

All queries read from the cached JSON - no git access.

```bash
# Most-changed files
agent-analyzer repo-intel query hotspots . --map-file repo-intel.json --top 10

# Files that change together with a given file
agent-analyzer repo-intel query coupling src/engine.rs . --map-file repo-intel.json

# Who owns a file or directory
agent-analyzer repo-intel query ownership src/core/ . --map-file repo-intel.json

# Bus factor (people covering 80% of commits, bots excluded)
agent-analyzer repo-intel query bus-factor . --map-file repo-intel.json

# Newcomer orientation summary
agent-analyzer repo-intel query onboard . --map-file repo-intel.json

# Outside contributor guidance
agent-analyzer repo-intel query can-i-help . --map-file repo-intel.json

# Concept-to-file search (uses descriptors when present)
agent-analyzer repo-intel query find "worker pool" . --map-file repo-intel.json --top 5

# Where execution starts (binaries, AST main fns, npm scripts)
agent-analyzer repo-intel query entry-points . --map-file repo-intel.json

# Cached 3-depth narrative summary (populated by external orchestrator)
agent-analyzer repo-intel query summary . --map-file repo-intel.json --depth 1
```

### Storing LLM-augmented signals

The binary stays offline-only. To add per-file descriptors or a 3-depth summary, an external orchestrator pipes JSON into the `set-*` subcommands:

```bash
echo '{"src/auth.rs": "Login route handler — validates credentials against bcrypt hash, issues JWT."}' \
  | agent-analyzer repo-intel set-descriptors --map-file repo-intel.json --input -

echo '{"depth1": "...", "depth3": "...", "depth10": "...", "inputHash": "sha256:..."}' \
  | agent-analyzer repo-intel set-summary --map-file repo-intel.json --input -
```

Embeddings go through the same path. `agent-analyzer-embed` writes a JSON document to stdout, and `set-embeddings` stores the vectors in a `<map-file-stem>.embeddings.bin` sidecar:

```bash
agent-analyzer-embed scan . --variant small \
  | agent-analyzer repo-intel set-embeddings --map-file repo-intel.json --input -
```

## Architecture

Rust workspace with 8 crates:

```
analyzer-core             shared types, git2 wrapper, bot/bug-fix/generated detection,
                          file walking, JSON output
    |
    +-- analyzer-git-map      git history extraction, aggregation, queries, incremental
    +-- analyzer-repo-map     AST-based symbol mapping + concept-to-file search (Phase 2 + Phase 6)
    +-- analyzer-collectors   project data gathering + entry-point detection (Phase 3)
    +-- analyzer-sync-check   doc-code sync analysis (Phase 4)
    +-- analyzer-graph        co-change communities + betweenness centrality (Phase 5), slop detection
    +-- analyzer-embed        local embeddings, `agent-analyzer-embed` binary
    |
analyzer-cli              unified binary, clap dispatch
```

### Bot detection pipeline

The `bot_detect` module identifies automation accounts using two signals:

1. Exact match against a curated list (`dependabot[bot]`, `renovate[bot]`, `github-actions[bot]`, `devin-ai-integration[bot]`, `copilot-swe-agent[bot]`, `Copilot`, `agent-core-bot`)
2. `[bot]` suffix on the author name

Bots are excluded from human-contributor counts, bus-factor calculations, and `single_author` risk scoring in diff-risk - so an account that touched a file once via Dependabot doesn't dilute the bus factor signal.

### Bug-fix attribution

The `bug_fix_detect` module classifies a commit subject as a bug fix when **any** of:

1. Conventional Commit prefix is one of `fix`, `bugfix`, `hotfix`, `patch`, `revert`
2. Subject contains a fix-related whole-word keyword (`fix`/`fixed`/`fixes`/`fixing`, `bug`, `regression`, `race`, `deadlock`, `leak`, `crash`, `oops`, `typo`, ...)
3. Subject contains an issue-closure phrase (`fixes #123`, `closes #42`, `resolves owner/repo#900`)

Whole-word matching guards against `prefix`/`suffix`/`unfixable` false positives.

### Generated-file suppression

The `generated_detect` module flags `*.pb.go`, `*.d.ts`, `*/generated/*`, `*/codegen/*`, `*.snap` and similar paths as auto-generated. Bug-fix attribution is skipped for these files at aggregation time so a `fix(schema): ...` commit credits the source `.proto` but not the generated bindings - keeps `bugspots` results focused on actually-broken code.

## Limitations

- **Merge commits are skipped** - only non-merge commits are analyzed, which matches how most tools attribute changes
- **Shallow clones** - work but produce incomplete history; the output includes a `shallow: true` flag
- **Large monorepos** - initial scan scales linearly with commit count; use `--max-commits` to bound the scan

## Development

```bash
cargo test                            # 416 tests across all crates
cargo clippy -- -D warnings           # lint
cargo fmt --check                     # format check
cargo build --release                 # optimized binary (LTO + stripped)
```

## Integration

This binary is consumed by JS plugins in the agent-sh ecosystem via the binary resolver in [agentsys](https://github.com/agent-sh/agentsys) (`lib/binary/`):

- JS calls `binary.ensureBinary()` which auto-downloads from GitHub releases
- Binary location: `~/.agent-sh/bin/agent-analyzer[.exe]`
- No manual install - lazy download on first use

Consumers:
- [deslop](https://github.com/agent-sh/deslop) plugin (runs `repo-intel init`/`update` and the `slop-fixes` / `slop-targets` queries through its vendored copy of the resolver)
- agentsys `lib/repo-intel/` (repo-intel artifact install, update and queries)

## Contributing

1. Fork and create a feature branch
2. Write tests for new functionality
3. Ensure `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` all pass
4. Open a PR - direct pushes to main are not allowed

To add a new known bot account, edit the `KNOWN_BOTS` constant in `crates/analyzer-core/src/bot_detect.rs` and add a test case.

## License

MIT
