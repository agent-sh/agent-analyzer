//! `slop-targets` query — ranked targets for the deslop agent's
//! Sonnet- and Opus-tier scans.
//!
//! Two outputs in one query:
//!
//! * **Sonnet tier (file-level)** — files where slop is *likely*
//!   present but requires reading-with-judgment to decide what to do.
//!   Each row carries a composite score and a dominant `suspect`
//!   label so the agent can pick the right reviewer prompt.
//! * **Opus tier (cross-file / area-level)** — modules or call chains
//!   where the slop is structural (over-abstraction, single-impl
//!   chains, cliché-name clusters). These need architectural taste
//!   and are escalated to the most capable model, with a fall-back
//!   to a human if Opus stays uncertain.
//!
//! All signals are derived from data already in the artifact
//! (FileActivity, symbols, import graph, cochange communities). No
//! embeddings required — the embedder lifts these signals further but
//! is not a prerequisite.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use analyzer_core::types::{FileActivity, RepoIntelData, SymbolKind};
use analyzer_embed::sidecar::Sidecar;

/// Which model tier should pick this target up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlopTier {
    Sonnet,
    Opus,
}

/// Stable identifier for the kind of slop suspected in a target. Lets
/// the deslop agent pick a tailored reviewer prompt instead of a
/// generic "scan for slop".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SlopSuspect {
    /// Hotspot + bugspot combo — code likely accreted defensive cargo
    /// cult patterns from rushed fixes.
    DefensiveCargoCult,
    /// Comment density well above project median — over-verbose docs
    /// likely.
    OverVerboseDocs,
    /// File is in the top size percentile — could likely be split or
    /// shortened.
    CouldBeShorter,
    /// Single commit added a large block of code (often AI dump).
    AiDumpResidue,
    /// Bot-heavy authorship without recent human review.
    BotAuthored,
    /// Cross-file: directory has multiple cliché names (helper /
    /// utility / manager) suggesting unfocused responsibility split.
    ClicheNames,
    /// Cross-file: chain of files where each imports exactly one
    /// downstream and is imported by exactly one upstream — wrapper
    /// tower over-abstraction.
    WrapperTower,
    /// Cross-file: an exported trait/interface has exactly one
    /// implementor across the codebase.
    SingleImpl,
    /// Cross-file: a cochange community with collectively high
    /// bug-fix density.
    HighBugCommunity,
}

/// One row in the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SlopTarget {
    /// Single-file target (Sonnet tier).
    File {
        path: String,
        tier: SlopTier,
        score: f32,
        suspect: SlopSuspect,
        why: String,
    },
    /// Multi-file area or module (Opus tier).
    Area {
        paths: Vec<String>,
        tier: SlopTier,
        score: f32,
        suspect: SlopSuspect,
        why: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlopTargetsResult {
    pub targets: Vec<SlopTarget>,
}

/// Run all detectors and return the unified ranked list.
///
/// `sidecar` is optional: when present, NLP-derived rows (stylistic
/// outliers, semantic duplicates) are layered onto the AST/graph rows.
/// When absent, the function returns only the cheap signals — same
/// shape, fewer rows.
pub fn slop_targets(
    map: &RepoIntelData,
    sidecar: Option<&Sidecar>,
    top_per_tier: usize,
) -> SlopTargetsResult {
    let mut sonnet = sonnet_file_targets(map, top_per_tier);
    let mut opus = opus_area_targets(map, top_per_tier);
    let mut nlp = sidecar
        .map(|s| crate::slop_nlp::nlp_targets(s, top_per_tier))
        .unwrap_or_default();
    let mut all = Vec::with_capacity(sonnet.len() + opus.len() + nlp.len());
    all.append(&mut sonnet);
    all.append(&mut opus);
    all.append(&mut nlp);
    SlopTargetsResult { targets: all }
}

// ── Sonnet tier (file-level scoring) ─────────────────────────────

fn sonnet_file_targets(map: &RepoIntelData, top: usize) -> Vec<SlopTarget> {
    if map.file_activity.is_empty() {
        return Vec::new();
    }

    // Pre-compute distributions for percentile-based anomaly detection.
    let activities: Vec<(&String, &FileActivity)> = map
        .file_activity
        .iter()
        .filter(|(_, a)| !a.generated)
        .collect();
    if activities.is_empty() {
        return Vec::new();
    }

    let total_changes_p95 = percentile(
        activities.iter().map(|(_, a)| a.changes as f32).collect(),
        0.95,
    );
    let bugfix_p95 = percentile(
        activities
            .iter()
            .map(|(_, a)| a.bug_fix_changes as f32)
            .collect(),
        0.95,
    );

    // Bot-author ratio per file: fraction of distinct authors that
    // match the bot list. We don't have per-file bot/human commit
    // counts in the schema (only at the contributors level), so the
    // proxy is: among distinct authors of the file, how many are bots.
    let bot_names: HashSet<&str> = map.contributors.bots.keys().map(|s| s.as_str()).collect();

    let mut scored: Vec<(f32, SlopSuspect, String, String)> = Vec::new();
    for (path, activity) in &activities {
        let path = path.as_str();

        let hot_score = if total_changes_p95 > 0.0 {
            (activity.changes as f32 / total_changes_p95).min(2.0)
        } else {
            0.0
        };
        let bug_score = if bugfix_p95 > 0.0 {
            (activity.bug_fix_changes as f32 / bugfix_p95).min(2.0)
        } else {
            0.0
        };
        let size_score = if activity.additions + activity.deletions > 0 {
            // Total churn as a proxy for file weight.
            ((activity.additions as f32) / 500.0).min(2.0)
        } else {
            0.0
        };
        let bot_ratio = if activity.authors.is_empty() {
            0.0
        } else {
            let bot_count = activity
                .authors
                .iter()
                .filter(|a| bot_names.contains(a.as_str()))
                .count();
            bot_count as f32 / activity.authors.len() as f32
        };

        let composite = hot_score * 1.0 + bug_score * 1.5 + size_score * 0.7 + bot_ratio * 0.8;
        if composite < 1.5 {
            continue;
        }

        // Dominant suspect = the largest contributing factor (excluding
        // size_score, which by itself is a softer signal).
        let suspect = if bug_score >= hot_score && bug_score >= bot_ratio {
            SlopSuspect::DefensiveCargoCult
        } else if bot_ratio >= hot_score {
            SlopSuspect::BotAuthored
        } else if size_score >= 1.0 {
            SlopSuspect::CouldBeShorter
        } else {
            SlopSuspect::DefensiveCargoCult
        };

        let why = format!(
            "changes {} ({:.0}% of p95), bug-fixes {} ({:.0}% of p95), bot-author ratio {:.2}",
            activity.changes,
            (activity.changes as f32 / total_changes_p95.max(1.0)) * 100.0,
            activity.bug_fix_changes,
            (activity.bug_fix_changes as f32 / bugfix_p95.max(1.0)) * 100.0,
            bot_ratio
        );
        scored.push((composite, suspect, path.to_string(), why));
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(top)
        .map(|(score, suspect, path, why)| SlopTarget::File {
            path,
            tier: SlopTier::Sonnet,
            score,
            suspect,
            why,
        })
        .collect()
}

fn percentile(mut values: Vec<f32>, p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((values.len() as f32 - 1.0) * p).round() as usize;
    values[idx.min(values.len() - 1)]
}

// ── Opus tier (cross-file / area) ────────────────────────────────

fn opus_area_targets(map: &RepoIntelData, top: usize) -> Vec<SlopTarget> {
    let mut out = Vec::new();
    out.extend(cliche_name_clusters(map));
    out.extend(wrapper_towers(map));
    out.extend(single_impl_traits(map));
    out.extend(high_bug_communities(map));

    out.sort_by(|a, b| {
        let sa = score_of(a);
        let sb = score_of(b);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(top);
    out
}

fn score_of(t: &SlopTarget) -> f32 {
    match t {
        SlopTarget::File { score, .. } | SlopTarget::Area { score, .. } => *score,
    }
}

/// Detect directories with multiple cliché-named exports (helper,
/// utility, manager, handler, processor, etc).
fn cliche_name_clusters(map: &RepoIntelData) -> Vec<SlopTarget> {
    let symbols = match map.symbols.as_ref() {
        Some(s) => s,
        None => return Vec::new(),
    };
    // Substrings flagged as cliché in identifier names. Matched
    // case-insensitively. Kept conservative — common-but-meaningful
    // names (Service, Controller, Provider, Factory, Builder) are
    // intentionally excluded because they're load-bearing in real
    // architectures (MVC controllers, Service Locator, Factory
    // Method, Builder pattern). Add them via project-level config if
    // a particular codebase wants the stricter rule.
    const CLICHE: &[&str] = &[
        // Vague responsibility ("does stuff for things")
        "helper",
        "helpers",
        "utility",
        "utilities",
        "util",
        "utils",
        "manager",
        "handler",
        "processor",
        "wrapper",
        // Vague type-like names
        "data",
        "info",
        "stuff",
        "thing",
        "things",
        // Module / file naming smells
        "misc",
        "common",
        "shared",
        "generic",
        "abstract",
        "base",
        // Throwaway / placeholder names
        "temp",
        "tmp",
        "dummy",
        "foo",
        "bar",
        "baz",
        "qux",
        "scratch",
        // Java/C# pattern-suffix soup ("FooImpl wraps Foo")
        "impl",
        "dao",
        "dto",
        "vo",
        "pojo",
    ];

    let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
    for (file_path, file_symbols) in symbols {
        let dir = parent_dir(file_path);
        for export in &file_symbols.exports {
            if matches!(
                export.kind,
                SymbolKind::Function | SymbolKind::Class | SymbolKind::Struct
            ) {
                let lower = export.name.to_ascii_lowercase();
                if CLICHE.iter().any(|c| lower.contains(c)) {
                    by_dir
                        .entry(dir.clone())
                        .or_default()
                        .push(format!("{file_path}::{}", export.name));
                }
            }
        }
    }

    let mut out = Vec::new();
    for (dir, hits) in by_dir {
        if hits.len() < 3 {
            continue;
        }
        let score = (hits.len() as f32).min(8.0) + 1.0;
        out.push(SlopTarget::Area {
            paths: vec![dir.clone()],
            tier: SlopTier::Opus,
            score,
            suspect: SlopSuspect::ClicheNames,
            why: format!(
                "{} cliché-named exports in {}: {}",
                hits.len(),
                if dir.is_empty() {
                    "(root)"
                } else {
                    dir.as_str()
                },
                hits.join(", ")
            ),
        });
    }
    out
}

fn parent_dir(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    match normalized.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

/// Detect chains of files where each file is imported by exactly one
/// upstream and imports exactly one downstream — proxy/wrapper towers.
fn wrapper_towers(map: &RepoIntelData) -> Vec<SlopTarget> {
    let import_graph = match map.import_graph.as_ref() {
        Some(g) => g,
        None => return Vec::new(),
    };

    // Build out-degree (imports per file) and in-degree (importers per file).
    let mut importers: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut imports: HashMap<&str, Vec<&str>> = HashMap::new();
    for (importer, targets) in import_graph {
        let importer = importer.as_str();
        for t in targets {
            let t = t.as_str();
            importers.entry(t).or_default().push(importer);
            imports.entry(importer).or_default().push(t);
        }
    }

    // A "wrapper node": exactly one importer + exactly one imported.
    let is_wrapper = |f: &str| {
        importers.get(f).map(|v| v.len()).unwrap_or(0) == 1
            && imports.get(f).map(|v| v.len()).unwrap_or(0) == 1
    };

    // Build chains by following the unique downstream edge from any
    // wrapper node, until we hit a non-wrapper or revisit. Dedupe by
    // recording chain membership in a visited set.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut chains: Vec<Vec<&str>> = Vec::new();

    let all_nodes: HashSet<&str> = importers
        .keys()
        .copied()
        .chain(imports.keys().copied())
        .collect();
    for &node in &all_nodes {
        if !is_wrapper(node) || visited.contains(node) {
            continue;
        }
        // Walk both directions to capture full chain.
        let mut chain: Vec<&str> = vec![node];
        visited.insert(node);
        // Forward
        let mut cur = node;
        while let Some(next) = imports.get(cur).and_then(|v| v.first().copied())
            && is_wrapper(next)
            && !visited.contains(next)
        {
            chain.push(next);
            visited.insert(next);
            cur = next;
        }
        // Backward
        let mut cur = node;
        while let Some(prev) = importers.get(cur).and_then(|v| v.first().copied())
            && is_wrapper(prev)
            && !visited.contains(prev)
        {
            chain.insert(0, prev);
            visited.insert(prev);
            cur = prev;
        }
        if chain.len() >= 3 {
            chains.push(chain);
        }
    }

    chains
        .into_iter()
        .map(|chain| {
            let len = chain.len();
            SlopTarget::Area {
                paths: chain.iter().map(|s| s.to_string()).collect(),
                tier: SlopTier::Opus,
                score: (len as f32).min(10.0),
                suspect: SlopSuspect::WrapperTower,
                why: format!(
                    "{}-deep wrapper chain (each node has one importer + one import): {}",
                    len,
                    chain.join(" → ")
                ),
            }
        })
        .collect()
}

/// Heuristic: a file exports a Trait or Interface and exactly one
/// other file imports it. Likely a single-impl abstraction.
fn single_impl_traits(map: &RepoIntelData) -> Vec<SlopTarget> {
    let symbols = match map.symbols.as_ref() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let import_graph = match map.import_graph.as_ref() {
        Some(g) => g,
        None => return Vec::new(),
    };

    let mut importers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (importer, targets) in import_graph {
        for t in targets {
            importers.entry(t.as_str()).or_default().push(importer);
        }
    }

    let mut out = Vec::new();
    for (file_path, file_symbols) in symbols {
        let trait_or_iface_exports: Vec<&str> = file_symbols
            .exports
            .iter()
            .filter(|e| matches!(e.kind, SymbolKind::Trait | SymbolKind::Interface))
            .map(|e| e.name.as_str())
            .collect();
        if trait_or_iface_exports.is_empty() {
            continue;
        }
        let inv = importers
            .get(file_path.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if inv.len() != 1 {
            continue;
        }
        out.push(SlopTarget::Area {
            paths: vec![file_path.clone(), inv[0].to_string()],
            tier: SlopTier::Opus,
            score: 6.0 + (trait_or_iface_exports.len() as f32).min(4.0),
            suspect: SlopSuspect::SingleImpl,
            why: format!(
                "trait/interface(s) {:?} in {} have a single importer ({})",
                trait_or_iface_exports, file_path, inv[0]
            ),
        });
    }
    out
}

/// Cochange communities where the collective bug-fix density sits in
/// the top decile. Surfaces an "area to investigate" rather than a
/// single file.
fn high_bug_communities(map: &RepoIntelData) -> Vec<SlopTarget> {
    let cochange = match map.graph.as_ref().and_then(|g| g.cochange.as_ref()) {
        Some(c) => c,
        None => return Vec::new(),
    };
    if cochange.communities.is_empty() {
        return Vec::new();
    }

    // Compute per-community total bug-fix changes.
    let mut totals: Vec<(u32, &Vec<String>, u64)> = cochange
        .communities
        .iter()
        .filter_map(|(id, files)| {
            if files.len() < 3 {
                return None;
            }
            let total: u64 = files
                .iter()
                .filter_map(|f| map.file_activity.get(f).map(|a| a.bug_fix_changes))
                .sum();
            Some((*id, files, total))
        })
        .collect();

    if totals.is_empty() {
        return Vec::new();
    }
    totals.sort_by_key(|b| std::cmp::Reverse(b.2));
    let cutoff_count = ((totals.len() as f32) * 0.10).ceil() as usize;
    let take = cutoff_count.max(1);
    totals
        .into_iter()
        .take(take)
        .filter(|(_, _, total)| *total >= 5)
        .map(|(id, files, total)| SlopTarget::Area {
            paths: files.clone(),
            tier: SlopTier::Opus,
            score: (total as f32 / 10.0).min(10.0),
            suspect: SlopSuspect::HighBugCommunity,
            why: format!(
                "cochange community #{id} has {} files with {} total bug-fix changes",
                files.len(),
                total
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{
        BotContributor, Contributors, ConventionInfo, FileActivity, FileSymbols, GitInfo,
        HumanContributor, Releases, RepoIntelData, SymbolEntry,
    };
    use chrono::Utc;

    fn empty_map() -> RepoIntelData {
        RepoIntelData {
            version: "test".into(),
            generated: Utc::now(),
            updated: Utc::now(),
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
            symbols: None,
            import_graph: None,
            project: None,
            doc_refs: None,
            graph: None,
            file_descriptors: None,
            summary: None,
            embeddings_meta: None,
        }
    }

    fn activity(changes: u64, bug_fixes: u64, authors: Vec<&str>) -> FileActivity {
        FileActivity {
            changes,
            recent_changes: 0,
            authors: authors.into_iter().map(String::from).collect(),
            created: "".into(),
            last_changed: "".into(),
            additions: changes * 50,
            deletions: 0,
            bug_fix_changes: bug_fixes,
            refactor_changes: 0,
            last_bug_fix: "".into(),
            generated: false,
        }
    }

    #[test]
    fn empty_map_yields_no_targets() {
        let map = empty_map();
        let res = slop_targets(&map, None, 10);
        assert!(res.targets.is_empty());
    }

    #[test]
    fn high_bug_file_scored_as_defensive_cargo_cult() {
        let mut map = empty_map();
        // One file has many bug fixes; another is calm.
        map.file_activity
            .insert("src/hot.rs".into(), activity(40, 20, vec!["alice"]));
        map.file_activity
            .insert("src/calm.rs".into(), activity(2, 0, vec!["alice"]));

        let res = slop_targets(&map, None, 10);
        let target = res.targets.iter().find(|t| {
            matches!(
                t,
                SlopTarget::File { path, .. } if path == "src/hot.rs"
            )
        });
        assert!(target.is_some(), "expected hot.rs to surface");
        if let Some(SlopTarget::File { suspect, tier, .. }) = target {
            assert_eq!(*tier, SlopTier::Sonnet);
            assert_eq!(*suspect, SlopSuspect::DefensiveCargoCult);
        }
    }

    #[test]
    fn bot_authored_file_gets_bot_suspect() {
        let mut map = empty_map();
        map.contributors.bots.insert(
            "dependabot[bot]".into(),
            BotContributor {
                commits: 100,
                recent_commits: 50,
                first_seen: "".into(),
                last_seen: "".into(),
            },
        );
        map.contributors.humans.insert(
            "alice".into(),
            HumanContributor {
                commits: 1,
                recent_commits: 1,
                first_seen: "".into(),
                last_seen: "".into(),
            },
        );
        // Pad with calm files so percentile calc is meaningful.
        for i in 0..10 {
            map.file_activity
                .insert(format!("calm-{i}.rs"), activity(1, 0, vec!["alice"]));
        }
        // Hot file authored only by the bot.
        map.file_activity.insert(
            "deps/lockfile.json".into(),
            activity(20, 0, vec!["dependabot[bot]"]),
        );

        let res = slop_targets(&map, None, 10);
        let bot_target = res.targets.iter().find(|t| {
            matches!(
                t,
                SlopTarget::File { path, suspect, .. }
                    if path == "deps/lockfile.json"
                        && *suspect == SlopSuspect::BotAuthored
            )
        });
        assert!(
            bot_target.is_some(),
            "expected bot suspect; targets = {:?}",
            res.targets
        );
    }

    #[test]
    fn cliche_name_cluster_surfaces_as_opus() {
        let mut map = empty_map();
        let mut symbols = HashMap::new();
        symbols.insert(
            "src/util/helper.rs".into(),
            FileSymbols {
                exports: vec![
                    SymbolEntry {
                        name: "DataHelper".into(),
                        kind: SymbolKind::Class,
                        line: 1,
                    },
                    SymbolEntry {
                        name: "InfoUtility".into(),
                        kind: SymbolKind::Class,
                        line: 5,
                    },
                    SymbolEntry {
                        name: "RecordManager".into(),
                        kind: SymbolKind::Class,
                        line: 10,
                    },
                ],
                imports: vec![],
                definitions: vec![],
            },
        );
        map.symbols = Some(symbols);

        let res = slop_targets(&map, None, 10);
        let area = res.targets.iter().find(|t| {
            matches!(
                t,
                SlopTarget::Area { tier, suspect, .. }
                    if *tier == SlopTier::Opus && *suspect == SlopSuspect::ClicheNames
            )
        });
        assert!(
            area.is_some(),
            "expected cliché cluster; got {:?}",
            res.targets
        );
    }

    #[test]
    fn wrapper_chain_of_three_surfaces_as_opus() {
        let mut map = empty_map();
        // a.rs -> b.rs -> c.rs -> d.rs (each in/out degree 1)
        let mut g = HashMap::new();
        g.insert("a.rs".into(), vec!["b.rs".into()]);
        g.insert("b.rs".into(), vec!["c.rs".into()]);
        g.insert("c.rs".into(), vec!["d.rs".into()]);
        // Outside caller of a.rs so a is a wrapper too (in-degree 1).
        g.insert("entry.rs".into(), vec!["a.rs".into()]);
        // d.rs has outgoing to a leaf so its out-degree counts.
        g.insert("d.rs".into(), vec!["leaf.rs".into()]);
        map.import_graph = Some(g);

        let res = slop_targets(&map, None, 10);
        let chain = res.targets.iter().find(|t| {
            matches!(
                t,
                SlopTarget::Area { suspect, .. } if *suspect == SlopSuspect::WrapperTower
            )
        });
        assert!(
            chain.is_some(),
            "expected wrapper tower; got {:?}",
            res.targets
        );
    }

    #[test]
    fn single_impl_trait_surfaces_as_opus() {
        let mut map = empty_map();
        let mut symbols = HashMap::new();
        symbols.insert(
            "src/store.rs".into(),
            FileSymbols {
                exports: vec![SymbolEntry {
                    name: "Store".into(),
                    kind: SymbolKind::Trait,
                    line: 1,
                }],
                imports: vec![],
                definitions: vec![],
            },
        );
        map.symbols = Some(symbols);
        let mut g = HashMap::new();
        g.insert("src/main.rs".into(), vec!["src/store.rs".into()]);
        map.import_graph = Some(g);

        let res = slop_targets(&map, None, 10);
        let single = res.targets.iter().find(|t| {
            matches!(
                t,
                SlopTarget::Area { suspect, .. } if *suspect == SlopSuspect::SingleImpl
            )
        });
        assert!(
            single.is_some(),
            "expected SingleImpl; got {:?}",
            res.targets
        );
    }

    #[test]
    fn percentile_handles_empty_and_singleton() {
        assert_eq!(percentile(vec![], 0.95), 0.0);
        assert_eq!(percentile(vec![5.0], 0.95), 5.0);
    }

    #[test]
    fn percentile_picks_top_value_for_p95() {
        let p = percentile(
            vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0],
            0.95,
        );
        assert!(p >= 9.0, "expected p95 in upper range, got {p}");
    }

    #[test]
    fn parent_dir_strips_filename() {
        assert_eq!(parent_dir("src/foo/bar.rs"), "src/foo");
        assert_eq!(parent_dir("foo.rs"), "");
        assert_eq!(parent_dir("a\\b\\c.rs"), "a/b");
    }

    #[test]
    fn target_serializes_with_kind_tag() {
        let t = SlopTarget::File {
            path: "x.rs".into(),
            tier: SlopTier::Sonnet,
            score: 5.0,
            suspect: SlopSuspect::CouldBeShorter,
            why: "test".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"kind\":\"file\""));
        assert!(json.contains("\"tier\":\"sonnet\""));
        assert!(json.contains("\"suspect\":\"could-be-shorter\""));
    }
}
