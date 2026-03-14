# agent-analyzer

[![CI](https://github.com/agent-sh/agent-analyzer/actions/workflows/ci.yml/badge.svg)](https://github.com/agent-sh/agent-analyzer/actions/workflows/ci.yml)
[![Release](https://github.com/agent-sh/agent-analyzer/actions/workflows/release.yml/badge.svg)](https://github.com/agent-sh/agent-analyzer/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Static analysis binary for the [agent-sh](https://github.com/agent-sh) ecosystem. Extracts temporal, social, and behavioral signals from git history - who changed what, when, how often, and whether AI tools were involved.

Produces a single cached JSON artifact that answers questions like "which files change together?", "who owns this module?", and "what percentage of commits are AI-generated?" - without touching git again after the initial scan.

## Why this project

- Use this when you need git-based code intelligence (hotspots, coupling, ownership) without shelling out to `git log` on every query
- Use this when you want to detect and quantify AI-generated commits across a repository
- Use this when you need incremental updates - only process new commits since the last scan
- Use this when building developer tools that need structured repository history as JSON

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

If you use the [git-map](https://github.com/agent-sh/git-map) plugin, the binary is downloaded automatically on first use. No manual install needed.

## Quick start

```bash
# Scan a repository's full git history
agent-analyzer git-map init ./my-repo > git-map.json

# Query the cached result - no git access needed
agent-analyzer git-map query hotspots ./my-repo --map-file git-map.json --top 5
```

Output (JSON to stdout):

```json
[
  { "path": "src/engine.rs", "changes": 142, "authors": 5, "lastChanged": "2026-03-10" },
  { "path": "src/api/handler.rs", "changes": 98, "authors": 3, "lastChanged": "2026-03-14" }
]
```

Progress and errors go to stderr, so piping and redirection work as expected.

## Core concepts

**Two-phase architecture**: First, `init` or `update` walks git history and produces a `GitMapData` JSON artifact. Then, `query` subcommands operate entirely on that cached artifact - no repository access required.

**Incremental by default**: After the initial scan, `update` only processes commits after `analyzedUpTo`. If a force-push is detected (the recorded SHA is no longer in history), it falls back to a full rebuild automatically.

**AI-aware from the ground up**: Every commit is checked against a multi-signal detection pipeline. The signature registry (`ai_signatures.json`) is data, not code - add new AI tools by editing a JSON file.

## Features

- **Git history extraction** - commit metadata, per-file diff stats, rename tracking, deletion tracking via libgit2 (no subprocess calls)
- **AI commit detection** - identifies commits from Claude, Cursor, Copilot, Aider, Replit, Windsurf, Devin, and bots like Dependabot and Renovate using trailers, author patterns, and message signatures
- **Hotspot analysis** - find the most frequently changed files, optionally filtered by time window
- **Coupling analysis** - discover files that change together (co-change frequency with configurable thresholds)
- **Ownership queries** - primary author, contributor breakdown, and bus factor per file or directory
- **Bus factor** - how many people cover 80% of commits, with optional AI-adjustment
- **Convention detection** - conventional commit style, prefix frequency, scope patterns
- **Release tracking** - tag-based release cadence, unreleased commit count
- **Health scoring** - composite metric combining activity, bus factor, frequency, and AI ratio
- **Noise filtering** - automatically excludes lockfiles, minified assets, `dist/`, `build/`, `vendor/` from analysis

## Usage

### Full scan

```bash
agent-analyzer git-map init /path/to/repo > git-map.json
```

### Incremental update

```bash
agent-analyzer git-map update /path/to/repo --map-file git-map.json > git-map-updated.json
```

### Check status

```bash
agent-analyzer git-map status /path/to/repo --map-file git-map.json
```

Returns `current`, `stale`, or `rebuild_needed`.

### Queries

All queries read from the cached JSON - no git access.

```bash
# Most-changed files
agent-analyzer git-map query hotspots . --map-file git-map.json --top 10

# Files that change together with a given file
agent-analyzer git-map query coupling src/engine.rs . --map-file git-map.json

# Who owns a file or directory
agent-analyzer git-map query ownership src/core/ . --map-file git-map.json

# Bus factor (people covering 80% of commits)
agent-analyzer git-map query bus-factor . --map-file git-map.json --adjust-for-ai
```

## Architecture

Rust workspace with 6 crates:

```
analyzer-core             shared types, git2 wrapper, AI detection, file walking, JSON output
    |
    +-- analyzer-git-map      git history extraction, aggregation, queries, incremental
    +-- analyzer-repo-map     AST-based symbol mapping (Phase 2)
    +-- analyzer-collectors   project data gathering (Phase 3)
    +-- analyzer-sync-check   doc-code sync analysis (Phase 4)
    |
analyzer-cli              unified binary, clap dispatch
```

### AI detection pipeline

Checks are ordered by confidence (highest first):

1. Trailer emails (`Co-Authored-By` containing known AI service emails)
2. Author emails (known AI tool domains)
3. Bot authors (exact match: `dependabot[bot]`, `renovate[bot]`)
4. Author name patterns (regex: `\(aider\)$`, `\[bot\]$`)
5. Message body patterns (`Generated with Claude Code`, `^aider: `)
6. Trailer names (`Co-Authored-By` name field: Claude, Cursor, Copilot)

Signatures are loaded from an embedded JSON registry (`ai_signatures.json`). To add a new AI tool, update that file - no code changes needed.

## Limitations

- **Merge commits are skipped** - only non-merge commits are analyzed, which matches how most tools attribute changes
- **Shallow clones** - work but produce incomplete history; the output includes a `shallow: true` flag
- **Large monorepos** - initial scan scales linearly with commit count; use `--max-commits` to bound the scan
- **Stub crates** - `repo-map`, `collectors`, and `sync-check` subcommands print "not yet implemented" (Phases 2-4)

## Development

```bash
cargo test                            # 43 tests across all crates
cargo clippy -- -D warnings           # lint
cargo fmt --check                     # format check
cargo build --release                 # optimized binary (LTO + stripped)
```

## Integration

This binary is consumed by JS plugins in the agent-sh ecosystem via a binary resolver in [agent-core](https://github.com/agent-sh/agent-core):

- JS calls `binary.ensureBinary()` which auto-downloads from GitHub releases
- Binary location: `~/.agent-sh/bin/agent-analyzer[.exe]`
- No manual install - lazy download on first use

Current consumers:
- [git-map](https://github.com/agent-sh/git-map) plugin (JS wrapper for `/git-map` command)

Planned consumers (Phases 2-4):
- `repo-map` plugin (replace ast-grep subprocess)
- `agent-core/lib/collectors/` (replace JS implementations)
- `sync-docs` plugin (replace JS analysis)

## Contributing

1. Fork and create a feature branch
2. Write tests for new functionality
3. Ensure `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` all pass
4. Open a PR - direct pushes to main are not allowed

To add a new AI tool signature, edit `crates/analyzer-core/src/ai_signatures.json` and add a test case.

## License

MIT
