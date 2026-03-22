# agent-analyzer Spec

> Repo intelligence engine for AI agent workflows. Single binary, one queryable artifact, many consumers.

## Problem

AI agents (exploration, planning, review, drift-detect, sync-docs, perf) make decisions about code without understanding the repository as a whole. They grep, they read files, they guess. They don't know:

- Which files always change together (coupling)
- Who actually owns which areas and whether that knowledge is still fresh
- Where bugs cluster and why
- What the project's norms are (commit style, test patterns, naming)
- Which docs reference symbols that no longer exist
- Where a newcomer should start contributing
- What the codebase's symbol graph looks like (exports, imports, dependencies)

Each agent reinvents partial answers to these questions using ad-hoc git commands and file scanning. The results are slow, inconsistent, and incomplete.

## Solution

A single Rust binary (`agent-analyzer`) that produces a unified **repo intelligence artifact** (`repo-intel.json`). This artifact is:

- **Generated once**, updated incrementally
- **Queryable** via CLI subcommands (agents call queries, not raw git)
- **Consumed by many plugins** - each plugin asks the questions it needs
- **Cross-platform and deterministic** - same binary, same output everywhere

The binary replaces ad-hoc git commands, ast-grep subprocesses, and manual file scanning across the ecosystem.

## Architecture

```
agent-analyzer (single binary)
  ├── repo-intel init <path>     → full scan, produces repo-intel.json
  ├── repo-intel update <path>   → incremental update (git delta + AST rescan of changed files)
  ├── repo-intel status <path>   → staleness check
  └── repo-intel query <type>    → query the cached artifact
       ├── hotspots              → most-changed files, weighted by recency
       ├── bugspots              → files with highest bug-fix density
       ├── painspots             → hotspot × complexity × bug-density intersection
       ├── coupling <file>       → files that co-change with target
       ├── ownership <path>      → who owns this area, recency of their activity
       ├── bus-factor             → minimum people covering 80% of knowledge
       ├── norms                 → project conventions (commits, naming, test patterns)
       ├── symbols <file>        → exports, imports, definitions for a file
       ├── dependents <symbol>   → who imports/uses this symbol
       ├── stale-docs            → docs referencing changed/deleted/renamed symbols
       ├── areas                 → codebase areas with ownership, health, complexity
       ├── onboard               → newcomer-oriented summary of the repo
       └── can-i-help            → where an outsider can contribute
```

## Data Model: repo-intel.json

The artifact is a single JSON file containing all extracted intelligence. Sections are populated by different analysis passes but stored together for query efficiency.

```
{
  "version": "2.0",
  "generated": "ISO-8601",
  "updated": "ISO-8601",

  // ─── Git History ───────────────────────────────────

  "git": {
    "analyzedUpTo": "SHA",
    "totalCommits": N,
    "firstCommitDate": "ISO-8601",
    "lastCommitDate": "ISO-8601",
    "shallow": false
  },

  "contributors": {
    "humans": {
      "Alice": {
        "commits": N,
        "firstSeen": "ISO-8601",
        "lastSeen": "ISO-8601",
        "areas": ["src/core/", "tests/"],
        "recentActivity": N          // commits in last 90 days
      }
    },
    "bots": {
      "dependabot[bot]": { "commits": N, "tool": "dependabot" }
    }
  },

  "fileActivity": {
    "src/core.rs": {
      "changes": N,
      "authors": ["Alice", "Bob"],
      "created": "ISO-8601",
      "lastChanged": "ISO-8601",
      "additions": N,
      "deletions": N,
      "bugFixChanges": N,            // commits with "fix" prefix or bug-related keywords
      "refactorChanges": N           // commits with "refactor" prefix
    }
  },

  "coupling": {
    "src/core.rs": {
      "src/core_test.rs": { "cochanges": N, "strength": 0.76 }
    }
  },

  "conventions": {
    "commitStyle": "conventional",   // conventional | mixed | freeform
    "prefixes": { "feat": N, "fix": N, "refactor": N },
    "usesScopes": true,
    "namingPatterns": {               // detected from AST
      "functions": "snake_case",
      "types": "PascalCase",
      "constants": "SCREAMING_SNAKE"
    },
    "testPatterns": {
      "framework": "cargo-test",     // jest | pytest | cargo-test | go-test | ...
      "location": "inline",          // inline | __tests__ | tests/ | spec/
      "naming": "test_*"             // test_* | *_test | *Test | *Spec
    }
  },

  "releases": {
    "tags": [{ "tag": "v1.0", "date": "ISO-8601", "commitsSince": N }],
    "cadence": "weekly"
  },

  "renames": [{ "from": "old.rs", "to": "new.rs", "date": "ISO-8601" }],
  "deletions": [{ "path": "removed.rs", "date": "ISO-8601" }],

  // ─── AST Symbols ──────────────────────────────────

  "symbols": {
    "src/core.rs": {
      "exports": [
        { "name": "validate", "kind": "function", "line": 42 },
        { "name": "Config", "kind": "struct", "line": 10 }
      ],
      "imports": [
        { "from": "src/types.rs", "names": ["GitInfo", "FileActivity"] }
      ],
      "definitions": [
        { "name": "validate", "kind": "function", "line": 42, "complexity": 8 },
        { "name": "Config", "kind": "struct", "line": 10, "complexity": 1 }
      ]
    }
  },

  "importGraph": {
    "src/core.rs": ["src/types.rs", "src/utils.rs"],
    "src/main.rs": ["src/core.rs", "src/config.rs"]
  },

  // ─── Doc-Code Cross-References ─────────────────────

  "docRefs": {
    "README.md": {
      "codeRefs": [
        { "text": "validate()", "symbol": "validate", "file": "src/core.rs", "exists": true },
        { "text": "old_function()", "symbol": "old_function", "file": null, "exists": false }
      ],
      "lastUpdated": "ISO-8601",
      "referencesHotFiles": true     // references files in top 10% hotspots
    }
  },

  // ─── Computed Areas ────────────────────────────────

  "areas": {
    "src/core/": {
      "files": N,
      "symbols": N,
      "owners": [
        { "name": "Alice", "pct": 72.0, "lastActive": "ISO-8601", "stale": false }
      ],
      "hotspotScore": 0.85,          // 0-1, normalized
      "bugFixRate": 0.12,            // fraction of changes that are fixes
      "complexity": {
        "total": N,
        "median": N,
        "max": { "file": "core.rs", "function": "validate", "value": 24 }
      },
      "testCoverage": {
        "hasTests": true,
        "testFiles": ["tests/core_test.rs"],
        "testToCodeRatio": 0.8       // test lines / code lines
      },
      "health": "healthy"            // healthy | needs-attention | at-risk
    }
  }
}
```

### What's NOT in the artifact

- **AI attribution counts** - current detection is unreliable (<15% accuracy on AI-heavy repos). Not included until code stylometry is viable.
- **Commit shape / size distribution** - trivially available from `git log`, not worth storing.
- **Commit message samples** - no query needs them.
- **Directory activity** - subsumed by `areas` which adds ownership, complexity, health.

## Query API

Each query reads the cached `repo-intel.json` and returns structured JSON to stdout. Queries are pure functions over the artifact - no git commands, no file I/O beyond reading the cache.

### hotspots

Most-changed files weighted by recency (recent changes count more).

```
agent-analyzer repo-intel query hotspots [--months=N] [--top=N]
```

Returns:
```json
[
  { "path": "src/core.rs", "score": 0.92, "changes": 45, "recentChanges": 12,
    "bugFixes": 8, "owners": ["Alice"], "coupledWith": ["src/core_test.rs"] }
]
```

**Consumers**: perf (optimization targets), audit-project (review priority), drift-detect (churn detection)

### bugspots

Files with highest bug-fix density (ratio of fix commits to total commits).

```
agent-analyzer repo-intel query bugspots [--top=N]
```

Returns:
```json
[
  { "path": "src/parser.rs", "bugFixRate": 0.35, "totalChanges": 20,
    "bugFixes": 7, "lastBugFix": "ISO-8601" }
]
```

**Consumers**: audit-project (extra scrutiny), planning-agent (risk assessment), onboard (warn newcomers)

### painspots

Intersection of hotspot + high complexity + high bug density. These are the files that hurt the most.

```
agent-analyzer repo-intel query painspots [--top=N]
```

Returns:
```json
[
  { "path": "src/parser.rs", "painScore": 0.94,
    "hotspotScore": 0.85, "complexity": 24, "bugFixRate": 0.35,
    "owners": ["Alice"], "ownerStale": false }
]
```

**Consumers**: perf (where to invest effort), planning-agent (risk), onboard (avoid these areas)

### coupling

Files that co-change with a given file. Reveals hidden dependencies.

```
agent-analyzer repo-intel query coupling <file> [--min-strength=0.3]
```

Returns:
```json
[
  { "path": "src/core_test.rs", "cochanges": 16, "strength": 0.76 },
  { "path": "src/types.rs", "cochanges": 8, "strength": 0.38 }
]
```

**Consumers**: exploration-agent ("you're touching X, also check Y"), planning-agent (scope), audit-project ("you changed A but not its coupled partner B")

### ownership

Who owns a file or directory, with recency.

```
agent-analyzer repo-intel query ownership <path>
```

Returns:
```json
{
  "path": "src/core/",
  "owners": [
    { "name": "Alice", "pct": 72.0, "commits": 36, "lastActive": "2026-03-10", "stale": false },
    { "name": "Bob", "pct": 28.0, "commits": 14, "lastActive": "2025-11-02", "stale": true }
  ],
  "busFactorRisk": true
}
```

**Consumers**: next-task (assign work), audit-project (who should review), can-i-help (mentors)

### bus-factor

Minimum contributors covering 80% of commits, with staleness awareness.

```
agent-analyzer repo-intel query bus-factor
```

Returns:
```json
{
  "busFactor": 1,
  "criticalOwners": [
    { "name": "Alice", "coverage": 0.85, "lastActive": "2026-03-10", "stale": false }
  ],
  "atRiskAreas": ["src/core/", "src/parser/"]
}
```

**Consumers**: onboard (project health), drift-detect (risk), can-i-help (where help is needed most)

### norms

Project conventions detected from git history and AST analysis.

```
agent-analyzer repo-intel query norms
```

Returns:
```json
{
  "commits": {
    "style": "conventional",
    "prefixes": { "feat": 151, "fix": 186, "refactor": 22 },
    "usesScopes": true,
    "exampleMessages": ["feat(core): add validation pipeline", "fix: handle empty input"]
  },
  "code": {
    "language": "rust",
    "functionNaming": "snake_case",
    "typeNaming": "PascalCase",
    "testFramework": "cargo-test",
    "testLocation": "inline",
    "testNaming": "test_*"
  }
}
```

**Consumers**: onboard (learn conventions before contributing), can-i-help (match project style), enhance (validate conformance)

### symbols

Exports, imports, and definitions for a file.

```
agent-analyzer repo-intel query symbols <file>
```

Returns:
```json
{
  "path": "src/core.rs",
  "exports": [
    { "name": "validate", "kind": "function", "line": 42, "complexity": 8 }
  ],
  "imports": [
    { "from": "src/types.rs", "names": ["GitInfo", "FileActivity"] }
  ],
  "importedBy": ["src/main.rs", "src/cli.rs"]
}
```

**Consumers**: exploration-agent (understand file), planning-agent (trace dependencies), sync-docs (cross-ref with docs)

### dependents

Who imports or uses a given symbol.

```
agent-analyzer repo-intel query dependents <symbol> [--file=<file>]
```

Returns:
```json
{
  "symbol": "validate",
  "definedIn": "src/core.rs:42",
  "usedBy": [
    { "file": "src/main.rs", "line": 15, "context": "use crate::core::validate" },
    { "file": "tests/integration.rs", "line": 8, "context": "core::validate(input)" }
  ]
}
```

**Consumers**: planning-agent (impact analysis - "changing this breaks these callers"), audit-project (blast radius)

### stale-docs

Documentation files that reference symbols which have been renamed, deleted, or significantly changed.

```
agent-analyzer repo-intel query stale-docs
```

Returns:
```json
[
  { "doc": "README.md", "line": 42,
    "reference": "old_function()",
    "issue": "symbol-not-found",
    "suggestion": "Symbol was deleted in commit abc123 on 2026-03-01" },
  { "doc": "docs/api.md", "line": 18,
    "reference": "parse_input()",
    "issue": "symbol-renamed",
    "renamedTo": "parse_data()",
    "suggestion": "Renamed in src/parser.rs" },
  { "doc": "README.md", "line": 7,
    "reference": "validate()",
    "issue": "references-hotspot",
    "suggestion": "This doc references a file that changed 12 times since doc was last updated" }
]
```

**Consumers**: sync-docs (primary), drift-detect (doc decay detection)

### areas

High-level view of codebase areas with ownership, health, and complexity.

```
agent-analyzer repo-intel query areas [--top=N]
```

Returns:
```json
[
  { "path": "src/core/", "files": 12, "symbols": 87,
    "health": "healthy",
    "hotspotScore": 0.85, "bugFixRate": 0.12, "complexity": { "median": 5, "max": 24 },
    "owners": [{ "name": "Alice", "pct": 72.0, "stale": false }],
    "testCoverage": { "hasTests": true, "ratio": 0.8 } },
  { "path": "src/parser/", "files": 4, "symbols": 23,
    "health": "at-risk",
    "hotspotScore": 0.92, "bugFixRate": 0.35, "complexity": { "median": 12, "max": 31 },
    "owners": [{ "name": "Bob", "pct": 90.0, "stale": true }],
    "testCoverage": { "hasTests": false, "ratio": 0.0 } }
]
```

**Consumers**: onboard (codebase overview), can-i-help (where help needed), drift-detect (area health)

### onboard

Human-readable summary for someone new to the repo.

```
agent-analyzer repo-intel query onboard
```

Returns:
```json
{
  "language": "rust",
  "framework": null,
  "structure": "workspace with 6 crates",
  "totalFiles": 87,
  "totalSymbols": 432,
  "busFactor": 1,
  "health": "healthy",
  "conventions": { "commitStyle": "conventional", "testFramework": "cargo-test" },
  "keyAreas": [
    { "path": "crates/core/", "purpose": "shared types and utilities", "entryPoint": "src/lib.rs" },
    { "path": "crates/cli/", "purpose": "command-line interface", "entryPoint": "src/main.rs" }
  ],
  "painPoints": [
    { "path": "src/parser.rs", "reason": "high complexity (31), frequent bugs, single owner" }
  ],
  "gettingStarted": {
    "buildCommand": "cargo build",
    "testCommand": "cargo test",
    "entryPoints": ["crates/cli/src/main.rs"]
  }
}
```

**Consumers**: onboarding flow, README generation, project health dashboards

### can-i-help

Guidance for outside contributors.

```
agent-analyzer repo-intel query can-i-help
```

Returns:
```json
{
  "conventions": { "commitStyle": "conventional", "branchNaming": "feat/*, fix/*" },
  "goodFirstAreas": [
    { "path": "src/utils/", "reason": "low complexity, good test coverage, active owner for review",
      "owner": "Alice", "complexity": 3 }
  ],
  "needsHelp": [
    { "path": "src/parser/", "reason": "high bug rate, owner inactive for 4 months",
      "bugFixRate": 0.35, "ownerLastActive": "2025-11-02" }
  ],
  "recentActivity": {
    "activeContributors": 2,
    "commitsLast30Days": 45,
    "openIssues": null
  }
}
```

**Consumers**: OSS onboarding, contributor guidance, mentorship matching

## Consumer Integration

How each ecosystem plugin uses the artifact:

| Consumer | Queries Used | Integration Point |
|----------|-------------|-------------------|
| **exploration-agent** | coupling, symbols, ownership, areas | Before planning - understand blast radius |
| **planning-agent** | coupling, dependents, painspots, ownership | Scope decisions, risk assessment, reviewer selection |
| **audit-project** | painspots, coupling, ownership, bugspots | Review priority, "you changed A but not coupled B" |
| **drift-detect** | areas, bus-factor, stale-docs, hotspots | Area health trends, knowledge loss, doc decay |
| **sync-docs** | stale-docs, symbols, coupling | Find stale references, prioritize doc updates |
| **perf** | hotspots, painspots, coupling, symbols | Optimization targets, code path mapping |
| **enhance** | norms, areas | Convention conformance validation |
| **onboard** (new) | onboard, norms, areas, bus-factor | Newcomer orientation |
| **can-i-help** (new) | can-i-help, norms, bugspots, ownership | OSS contributor guidance |

### Agent workflow integration

The binary resolver in `agent-core/lib/binary/` auto-downloads the binary. Plugins call it via:

```javascript
const binary = require('@agentsys/lib/binary');
binary.ensureBinarySync();

// Generate or update
const json = binary.runAnalyzer(['repo-intel', 'init', cwd]);
// or
const json = binary.runAnalyzer(['repo-intel', 'update', cwd]);

// Query
const hotspots = JSON.parse(binary.runAnalyzer(['repo-intel', 'query', 'hotspots', '--top=10', cwd]));
const coupling = JSON.parse(binary.runAnalyzer(['repo-intel', 'query', 'coupling', 'src/core.rs', cwd]));
```

## Implementation Phases

### Phase 1: Git Intelligence (current - needs fixes)

What exists: git history extraction, file activity, coupling, basic queries.

**Fixes needed before release:**
- [x] Date ordering bug (created/lastChanged swap)
- [x] Bot classification (copilot-swe-agent, Copilot, agent-core-bot)
- [ ] Drop AI attribution from output (unreliable, re-add when stylometry works)
- [ ] Add `bugFixRate` and `refactorChanges` to file activity
- [ ] Add `recentActivity` to contributors (commits in last 90 days)
- [ ] Add `ownerStale` flag (no commits in 90+ days)
- [ ] Add recency weighting to hotspot scores
- [ ] Rename CLI from `git-map` to `repo-intel` (unified command namespace)

**Output**: git history portion of `repo-intel.json`

### Phase 2: AST Symbol Extraction

Replace ast-grep subprocess with embedded Rust parser.

**Implementation:**
- Use `tree-sitter` with language grammars for: Rust, TypeScript/JavaScript, Python, Go, Java
- Extract: exports, imports, definitions (functions, classes, structs, traits, interfaces)
- Compute: cyclomatic complexity per function, import graph
- Detect: naming conventions (snake_case, camelCase, PascalCase), test framework and patterns

**New queries**: `symbols`, `dependents`, `norms` (code section)

**Output**: `symbols`, `importGraph`, `conventions.namingPatterns`, `conventions.testPatterns` sections

### Phase 3: Doc-Code Cross-Reference

Cross-reference documentation with code symbols.

**Implementation:**
- Parse markdown files for code references (backtick spans, code blocks, API mentions)
- Match against symbol table from Phase 2
- Cross-reference with renames and deletions from Phase 1
- Detect staleness: doc references symbol that was renamed/deleted/moved
- Detect hotspot references: doc references a frequently-changed file but hasn't been updated

**New queries**: `stale-docs`

**Output**: `docRefs` section

### Phase 4: Computed Areas

Aggregate per-directory intelligence from Phases 1-3.

**Implementation:**
- Group files by directory prefix
- Compute per-area: ownership (with recency), hotspot score, bug-fix rate, complexity stats, test coverage ratio
- Classify health: healthy (active owner, tested, low bug rate), needs-attention (stale owner OR low tests OR high bugs), at-risk (multiple risk factors)
- Generate `onboard` and `can-i-help` from area data

**New queries**: `areas`, `painspots`, `bugspots`, `onboard`, `can-i-help`, `bus-factor` (enhanced with staleness)

**Output**: `areas` section

### Phase 5: AI Code Stylometry (Research)

Detect AI-generated code from code patterns, not commit metadata.

**This is a research problem, not an engineering task.** Current metadata-based detection catches <15% of AI-authored commits. Viable stylometry requires:

- Training data: known AI vs human code samples
- Feature extraction: comment density, naming uniformity, error handling patterns, import ordering
- Per-tool fingerprinting: Claude vs Copilot vs Cursor style differences
- Confidence scoring: per-file, not per-commit

**Not scheduled.** Revisit when academic research matures or when we accumulate enough labeled training data from agent-sh repos (where ground truth is known).

## Incremental Updates

The artifact supports incremental updates to avoid full rescans:

```
repo-intel.json contains:
  git.analyzedUpTo = SHA of last analyzed commit
  updated = timestamp of last update
```

**Update algorithm:**
1. Check `git.analyzedUpTo` against current HEAD
2. If equal: artifact is current, no work needed
3. If `git cat-file -t analyzedUpTo` fails: force-push detected, full rebuild
4. Otherwise: extract delta (`analyzedUpTo..HEAD`), merge into existing artifact
5. For AST: rescan only files changed in the delta (diff --name-only)
6. For doc-refs: recheck only docs that reference changed files

**Performance target:** <2s incremental update for repos with <100 new commits.

## CLI Structure

```
agent-analyzer repo-intel init [--max-commits=N] <path>
agent-analyzer repo-intel update <path>
agent-analyzer repo-intel status <path>
agent-analyzer repo-intel query <type> [args...] <path>

# Legacy (kept for backwards compat during migration)
agent-analyzer git-map init|update|status|query ...
```

All output to stdout as JSON. Progress and errors to stderr.

## Crate Structure

```
crates/
  analyzer-core/          # Shared: types, git2 wrapper, AI detection, noise filtering, output
  analyzer-git-intel/     # Phase 1: git history extraction, aggregation, queries
  analyzer-ast-intel/     # Phase 2: tree-sitter parsing, symbol extraction, import graph
  analyzer-doc-intel/     # Phase 3: markdown parsing, symbol cross-referencing
  analyzer-areas/         # Phase 4: area computation, health classification, onboard/can-i-help
  analyzer-cli/           # Unified binary, clap dispatch
```

## What's NOT in Scope

- **AI attribution counts** - unreliable until stylometry works, omit from output
- **Commit shape / size distribution** - no consumer needs it
- **Commit message samples** - no consumer needs them
- **Release cadence analysis** - `git tag` is sufficient
- **GitHub API integration** - drift-detect already handles this in JS
- **Language server protocol** - the binary is a batch tool, not a daemon
- **Watch mode** - agents call update explicitly when needed

## Success Criteria

The binary is worth releasing when:

1. At least one consumer (exploration-agent or sync-docs) is wired up and using queries in production
2. The `areas` query produces actionable health assessments on real repos
3. The `coupling` query is consumed by exploration-agent before every planning session
4. The `onboard` query produces a useful newcomer summary that agents can present
5. The `stale-docs` query finds real stale references that sync-docs acts on

**Not a success criterion**: "the binary runs and produces JSON." That's table stakes. Value comes from agents making better decisions because of this data.
