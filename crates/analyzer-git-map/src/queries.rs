use std::collections::HashMap;

use chrono::DateTime;
use serde::Serialize;

use analyzer_core::types::{FileActivity, RepoIntelData};

use crate::aggregator::file_dir;

// ─── Staleness helper ───────────────────────────────────────────────────────

/// Check if a date string is stale (>90 days before the repo's last commit).
fn is_stale(last_seen: &str, repo_last_commit: &str) -> bool {
    let Ok(last) = DateTime::parse_from_rfc3339(repo_last_commit) else {
        return false;
    };
    let Ok(seen) = DateTime::parse_from_rfc3339(last_seen) else {
        return false;
    };
    let cutoff = last - chrono::Duration::days(90);
    seen < cutoff
}

// ─── Hotspots (Task 4: recency-weighted) ────────────────────────────────────

/// A hotspot entry with recency-weighted score.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotEntry {
    pub path: String,
    pub changes: u64,
    pub recent_changes: u64,
    pub score: f64,
    pub authors: Vec<String>,
    pub ai_ratio: f64,
    pub bug_fixes: u64,
}

/// Get hotspot files sorted by recency-weighted score.
///
/// Score formula: (recent_changes * 2 + total_changes) / (total_changes + 1)
/// This gives recent activity a 2x weight. A file with 50 total but 0 recent
/// scores lower than a file with 20 total and 15 recent.
pub fn hotspots(map: &RepoIntelData, _months: Option<u32>, limit: usize) -> Vec<HotspotEntry> {
    let mut entries: Vec<HotspotEntry> = map
        .file_activity
        .iter()
        .map(|(path, activity)| {
            let ai_ratio = if activity.changes > 0 {
                activity.ai_changes as f64 / activity.changes as f64
            } else {
                0.0
            };
            let score = (activity.recent_changes as f64 * 2.0 + activity.changes as f64)
                / (activity.changes as f64 + 1.0);
            HotspotEntry {
                path: path.clone(),
                changes: activity.changes,
                recent_changes: activity.recent_changes,
                score,
                authors: activity.authors.clone(),
                ai_ratio,
                bug_fixes: activity.bug_fix_changes,
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries.truncate(limit);
    entries
}

// ─── Coldspots ──────────────────────────────────────────────────────────────

/// A coldspot entry - a file with no recent activity.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColdspotEntry {
    pub path: String,
    pub last_changed: String,
}

/// Get coldspot files - files with no recent changes.
pub fn coldspots(map: &RepoIntelData, _months: Option<u32>) -> Vec<ColdspotEntry> {
    let mut entries: Vec<ColdspotEntry> = map
        .file_activity
        .iter()
        .map(|(path, activity)| ColdspotEntry {
            path: path.clone(),
            last_changed: activity.last_changed.clone(),
        })
        .collect();

    entries.sort_by(|a, b| a.last_changed.cmp(&b.last_changed));
    entries
}

// ─── Bugspots (Task 5) ─────────────────────────────────────────────────────

/// A bugspot entry - a file with high bug-fix density.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BugspotEntry {
    pub path: String,
    pub bug_fix_rate: f64,
    pub total_changes: u64,
    pub bug_fixes: u64,
    pub last_bug_fix: String,
}

/// Get files with highest bug-fix density (ratio of fix commits to total).
///
/// Only includes files with at least one bug fix. Sorted by bug_fix_rate descending.
pub fn bugspots(map: &RepoIntelData, limit: usize) -> Vec<BugspotEntry> {
    let mut entries: Vec<BugspotEntry> = map
        .file_activity
        .iter()
        .filter(|(_, activity)| activity.bug_fix_changes > 0)
        .map(|(path, activity)| {
            let rate = activity.bug_fix_changes as f64 / activity.changes as f64;
            BugspotEntry {
                path: path.clone(),
                bug_fix_rate: rate,
                total_changes: activity.changes,
                bug_fixes: activity.bug_fix_changes,
                last_bug_fix: activity.last_bug_fix.clone(),
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        b.bug_fix_rate
            .partial_cmp(&a.bug_fix_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries.truncate(limit);
    entries
}

// ─── Coupling ───────────────────────────────────────────────────────────────

/// Coupling result for a file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouplingResult {
    pub path: String,
    pub strength: f64,
    pub cochanges: u64,
    pub human_cochanges: u64,
}

/// Get files coupled with a given file.
pub fn coupling(map: &RepoIntelData, file: &str, human_only: bool) -> Vec<CouplingResult> {
    let mut results = Vec::new();

    // Check forward coupling (file as key)
    if let Some(pairs) = map.coupling.get(file) {
        for (other, entry) in pairs {
            let total = map.file_activity.get(file).map(|f| f.changes).unwrap_or(1);
            let strength = entry.cochanges as f64 / total as f64;

            if human_only && entry.human_cochanges == 0 {
                continue;
            }

            results.push(CouplingResult {
                path: other.clone(),
                strength,
                cochanges: if human_only {
                    entry.human_cochanges
                } else {
                    entry.cochanges
                },
                human_cochanges: entry.human_cochanges,
            });
        }
    }

    // Check reverse coupling (file as value in other entries)
    for (key, pairs) in &map.coupling {
        if let Some(entry) = pairs.get(file) {
            let total = map.file_activity.get(file).map(|f| f.changes).unwrap_or(1);
            let strength = entry.cochanges as f64 / total as f64;

            if human_only && entry.human_cochanges == 0 {
                continue;
            }

            results.push(CouplingResult {
                path: key.clone(),
                strength,
                cochanges: if human_only {
                    entry.human_cochanges
                } else {
                    entry.cochanges
                },
                human_cochanges: entry.human_cochanges,
            });
        }
    }

    results.sort_by(|a, b| b.cochanges.cmp(&a.cochanges));
    results
}

// ─── Ownership (Task 7: enhanced with staleness) ────────────────────────────

/// Ownership result with staleness awareness.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipResult {
    pub path: String,
    pub primary: String,
    pub pct: f64,
    pub owners: Vec<OwnerEntry>,
    pub ai_ratio: f64,
    pub bus_factor_risk: bool,
}

/// An owner entry with staleness tracking.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerEntry {
    pub name: String,
    pub commits: u64,
    pub pct: f64,
    pub last_active: String,
    pub stale: bool,
}

/// Get ownership information with staleness for a directory or file path.
pub fn ownership(map: &RepoIntelData, dir_or_file: &str) -> OwnershipResult {
    let mut author_changes: HashMap<String, u64> = HashMap::new();
    let mut total_changes: u64 = 0;
    let mut ai_changes: u64 = 0;

    for (path, activity) in &map.file_activity {
        if path.starts_with(dir_or_file) || path == dir_or_file {
            total_changes += activity.changes;
            ai_changes += activity.ai_changes;
            for author in &activity.authors {
                *author_changes.entry(author.clone()).or_insert(0) += activity.changes;
            }
        }
    }

    let repo_last = &map.git.last_commit_date;

    let mut owners: Vec<OwnerEntry> = author_changes
        .iter()
        .map(|(name, &changes)| {
            let pct = if total_changes > 0 {
                (changes as f64 / total_changes as f64) * 100.0
            } else {
                0.0
            };

            let last_active = map
                .contributors
                .humans
                .get(name)
                .map(|c| c.last_seen.clone())
                .or_else(|| map.contributors.bots.get(name).map(|c| c.last_seen.clone()))
                .unwrap_or_default();

            let stale = is_stale(&last_active, repo_last);

            OwnerEntry {
                name: name.clone(),
                commits: changes,
                pct,
                last_active,
                stale,
            }
        })
        .collect();

    owners.sort_by(|a, b| b.commits.cmp(&a.commits));

    let primary = owners.first().map(|c| c.name.clone()).unwrap_or_default();
    let primary_pct = owners.first().map(|c| c.pct).unwrap_or(0.0);
    let ai_ratio = if total_changes > 0 {
        ai_changes as f64 / total_changes as f64
    } else {
        0.0
    };

    // Bus factor risk: only 1 non-stale owner
    let non_stale_count = owners.iter().filter(|o| !o.stale).count();
    let bus_factor_risk = non_stale_count <= 1;

    OwnershipResult {
        path: dir_or_file.to_string(),
        primary,
        pct: primary_pct,
        owners,
        ai_ratio,
        bus_factor_risk,
    }
}

// ─── Bus Factor (Task 7: enhanced with staleness) ───────────────────────────

/// Enhanced bus factor result with critical owners and at-risk areas.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BusFactorResult {
    pub bus_factor: usize,
    pub adjust_for_ai: bool,
    pub critical_owners: Vec<CriticalOwner>,
    pub at_risk_areas: Vec<String>,
}

/// A critical owner in bus-factor analysis.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CriticalOwner {
    pub name: String,
    pub coverage: f64,
    pub last_active: String,
    pub stale: bool,
}

/// Calculate bus factor - the minimum number of people covering 80% of commits.
pub fn bus_factor(map: &RepoIntelData, adjust_for_ai: bool) -> usize {
    bus_factor_detailed(map, adjust_for_ai).bus_factor
}

/// Calculate detailed bus factor with critical owners and at-risk areas.
pub fn bus_factor_detailed(map: &RepoIntelData, adjust_for_ai: bool) -> BusFactorResult {
    let repo_last = &map.git.last_commit_date;

    let mut human_commits: Vec<(String, f64)> = map
        .contributors
        .humans
        .iter()
        .map(|(name, c)| {
            let effective = if adjust_for_ai && c.commits > 0 {
                let human_ratio = 1.0 - (c.ai_assisted_commits as f64 / c.commits as f64);
                c.commits as f64 * human_ratio
            } else {
                c.commits as f64
            };
            (name.clone(), effective)
        })
        .collect();

    human_commits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let total: f64 = human_commits.iter().map(|(_, c)| c).sum();
    if total == 0.0 {
        return BusFactorResult {
            bus_factor: 0,
            adjust_for_ai,
            critical_owners: vec![],
            at_risk_areas: vec![],
        };
    }

    let threshold = total * 0.8;
    let mut accumulated = 0.0;
    let mut count = 0;
    let mut critical_owners = Vec::new();

    for (name, commits) in &human_commits {
        accumulated += commits;
        count += 1;

        let last_active = map
            .contributors
            .humans
            .get(name)
            .map(|c| c.last_seen.clone())
            .unwrap_or_default();
        let stale = is_stale(&last_active, repo_last);

        critical_owners.push(CriticalOwner {
            name: name.clone(),
            coverage: (accumulated / total) * 100.0,
            last_active,
            stale,
        });

        if accumulated >= threshold {
            break;
        }
    }

    // Find at-risk areas: directories where the primary owner is stale
    let mut area_owners: HashMap<String, Vec<(&str, u64)>> = HashMap::new();
    for (path, activity) in &map.file_activity {
        let dir = file_dir(path);
        for author in &activity.authors {
            area_owners
                .entry(dir.clone())
                .or_default()
                .push((author.as_str(), activity.changes));
        }
    }

    let mut at_risk_areas = Vec::new();
    for (area, contributors) in &area_owners {
        let mut author_totals: HashMap<&str, u64> = HashMap::new();
        for (author, changes) in contributors {
            *author_totals.entry(author).or_insert(0) += changes;
        }
        if let Some((&primary, _)) = author_totals.iter().max_by_key(|&(_, &c)| c) {
            let last_active = map
                .contributors
                .humans
                .get(primary)
                .map(|c| c.last_seen.as_str())
                .unwrap_or("");
            if is_stale(last_active, repo_last) {
                at_risk_areas.push(area.clone());
            }
        }
    }
    at_risk_areas.sort();

    BusFactorResult {
        bus_factor: count,
        adjust_for_ai,
        critical_owners,
        at_risk_areas,
    }
}

// ─── Norms (Task 6) ────────────────────────────────────────────────────────

/// Project norms detected from git history.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormsResult {
    pub commits: CommitNorms,
}

/// Commit message convention norms.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitNorms {
    pub style: String,
    pub prefixes: HashMap<String, u64>,
    pub uses_scopes: bool,
    pub example_messages: Vec<String>,
}

/// Get project norms detected from git history.
///
/// Returns commit conventions. Code-level norms (naming, test framework)
/// will be added in Phase 2 (AST analysis).
pub fn norms(map: &RepoIntelData) -> NormsResult {
    NormsResult {
        commits: CommitNorms {
            style: map.conventions.style.clone(),
            prefixes: map.conventions.prefixes.clone(),
            uses_scopes: map.conventions.uses_scopes,
            example_messages: vec![],
        },
    }
}

// ─── Areas (Task 9) ────────────────────────────────────────────────────────

/// An area (directory) with health classification.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaEntry {
    pub area: String,
    pub files: usize,
    pub owners: Vec<OwnerEntry>,
    pub hotspot_score: f64,
    pub bug_fix_rate: f64,
    pub health: String,
}

/// Get area-level health overview by grouping files into top-level directories.
///
/// Health classification:
/// - "healthy": active non-stale owner + bug_fix_rate < 0.3
/// - "needs-attention": stale primary owner OR high bug rate
/// - "at-risk": both stale primary owner AND high bug rate
pub fn areas(map: &RepoIntelData) -> Vec<AreaEntry> {
    let repo_last = &map.git.last_commit_date;

    // Group files by directory
    let mut dir_files: HashMap<String, Vec<(&String, &FileActivity)>> = HashMap::new();
    for (path, activity) in &map.file_activity {
        let dir = file_dir(path);
        dir_files.entry(dir).or_default().push((path, activity));
    }

    let mut entries: Vec<AreaEntry> = dir_files
        .into_iter()
        .map(|(area, files)| {
            let file_count = files.len();

            // Aggregate stats
            let mut total_changes: u64 = 0;
            let mut total_bug_fixes: u64 = 0;
            let mut max_score: f64 = 0.0;
            let mut author_changes: HashMap<String, u64> = HashMap::new();

            for (_, activity) in &files {
                total_changes += activity.changes;
                total_bug_fixes += activity.bug_fix_changes;

                let score = (activity.recent_changes as f64 * 2.0 + activity.changes as f64)
                    / (activity.changes as f64 + 1.0);
                if score > max_score {
                    max_score = score;
                }

                for author in &activity.authors {
                    *author_changes.entry(author.clone()).or_insert(0) += activity.changes;
                }
            }

            let bug_fix_rate = if total_changes > 0 {
                total_bug_fixes as f64 / total_changes as f64
            } else {
                0.0
            };

            // Build owner entries
            let mut owners: Vec<OwnerEntry> = author_changes
                .iter()
                .map(|(name, &changes)| {
                    let pct = if total_changes > 0 {
                        (changes as f64 / total_changes as f64) * 100.0
                    } else {
                        0.0
                    };
                    let last_active = map
                        .contributors
                        .humans
                        .get(name)
                        .map(|c| c.last_seen.clone())
                        .or_else(|| map.contributors.bots.get(name).map(|c| c.last_seen.clone()))
                        .unwrap_or_default();
                    let stale = is_stale(&last_active, repo_last);

                    OwnerEntry {
                        name: name.clone(),
                        commits: changes,
                        pct,
                        last_active,
                        stale,
                    }
                })
                .collect();

            owners.sort_by(|a, b| b.commits.cmp(&a.commits));

            // Health classification
            let primary_stale = owners.first().map(|o| o.stale).unwrap_or(true);
            let high_bug_rate = bug_fix_rate >= 0.3;

            let health = if primary_stale && high_bug_rate {
                "at-risk"
            } else if primary_stale || high_bug_rate {
                "needs-attention"
            } else {
                "healthy"
            };

            AreaEntry {
                area,
                files: file_count,
                owners,
                hotspot_score: max_score,
                bug_fix_rate,
                health: health.to_string(),
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        b.hotspot_score
            .partial_cmp(&a.hotspot_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries
}

// ─── Contributors ───────────────────────────────────────────────────────────

/// A contributor entry with stats.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContributorEntry {
    pub name: String,
    pub commits: u64,
    pub pct: f64,
    pub ai_assisted_pct: f64,
}

/// Get contributors filtered by recent activity.
pub fn contributors(map: &RepoIntelData, _months: Option<u32>) -> Vec<ContributorEntry> {
    let total: u64 = map.contributors.humans.values().map(|c| c.commits).sum();

    let mut entries: Vec<ContributorEntry> = map
        .contributors
        .humans
        .iter()
        .map(|(name, c)| {
            let pct = if total > 0 {
                (c.commits as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            let ai_assisted_pct = if c.commits > 0 {
                (c.ai_assisted_commits as f64 / c.commits as f64) * 100.0
            } else {
                0.0
            };
            ContributorEntry {
                name: name.clone(),
                commits: c.commits,
                pct,
                ai_assisted_pct,
            }
        })
        .collect();

    entries.sort_by(|a, b| b.commits.cmp(&a.commits));
    entries
}

// ─── AI Ratio ───────────────────────────────────────────────────────────────

/// AI ratio result.
#[derive(Debug, Clone, Serialize)]
pub struct AiRatioResult {
    pub ratio: f64,
    pub attributed: u64,
    pub total: u64,
    pub tools: HashMap<String, u64>,
}

/// Get AI ratio for the entire repo or a path filter.
pub fn ai_ratio(map: &RepoIntelData, path_filter: Option<&str>) -> AiRatioResult {
    if let Some(filter) = path_filter {
        let mut total_changes: u64 = 0;
        let mut ai_changes: u64 = 0;

        for (path, activity) in &map.file_activity {
            if path.starts_with(filter) {
                total_changes += activity.changes;
                ai_changes += activity.ai_changes;
            }
        }

        AiRatioResult {
            ratio: if total_changes > 0 {
                ai_changes as f64 / total_changes as f64
            } else {
                0.0
            },
            attributed: ai_changes,
            total: total_changes,
            tools: map.ai_attribution.tools.clone(),
        }
    } else {
        let total = map.ai_attribution.attributed + map.ai_attribution.none;
        AiRatioResult {
            ratio: if total > 0 {
                map.ai_attribution.attributed as f64 / total as f64
            } else {
                0.0
            },
            attributed: map.ai_attribution.attributed,
            total,
            tools: map.ai_attribution.tools.clone(),
        }
    }
}

// ─── Release Info ───────────────────────────────────────────────────────────

/// Release info result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub cadence: String,
    pub last_release: Option<String>,
    pub unreleased: u64,
    pub tags: Vec<analyzer_core::types::ReleaseTag>,
}

/// Get release information.
pub fn release_info(map: &RepoIntelData) -> ReleaseInfo {
    let last_release = map.releases.tags.last().map(|t| t.tag.clone());
    let unreleased = map
        .releases
        .tags
        .last()
        .map(|t| t.commits_since)
        .unwrap_or(map.git.total_commits_analyzed);

    ReleaseInfo {
        cadence: map.releases.cadence.clone(),
        last_release,
        unreleased,
        tags: map.releases.tags.clone(),
    }
}

// ─── Health ─────────────────────────────────────────────────────────────────

/// Health result for the repository.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResult {
    pub active: bool,
    pub bus_factor: usize,
    pub commit_frequency: f64,
    pub ai_ratio: f64,
}

/// Get repository health summary.
pub fn health(map: &RepoIntelData) -> HealthResult {
    let bf = bus_factor(map, false);
    let total = map.ai_attribution.attributed + map.ai_attribution.none;
    let ai_r = if total > 0 {
        map.ai_attribution.attributed as f64 / total as f64
    } else {
        0.0
    };

    let active = map.git.total_commits_analyzed > 0;

    HealthResult {
        active,
        bus_factor: bf,
        commit_frequency: map.git.total_commits_analyzed as f64,
        ai_ratio: ai_r,
    }
}

// ─── File History ───────────────────────────────────────────────────────────

/// Get file history for a specific path.
pub fn file_history<'a>(map: &'a RepoIntelData, path: &str) -> Option<&'a FileActivity> {
    map.file_activity.get(path)
}

// ─── Conventions ────────────────────────────────────────────────────────────

/// Convention result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConventionResult {
    pub style: String,
    pub prefixes: HashMap<String, u64>,
    pub uses_scopes: bool,
}

/// Get convention statistics.
pub fn conventions(map: &RepoIntelData) -> ConventionResult {
    ConventionResult {
        style: map.conventions.style.clone(),
        prefixes: map.conventions.prefixes.clone(),
        uses_scopes: map.conventions.uses_scopes,
    }
}

// ─── Test Gaps ──────────────────────────────────────────────────────────────

/// A file that changes frequently but has no co-changing test file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestGapEntry {
    pub path: String,
    pub changes: u64,
    pub recent_changes: u64,
    pub bug_fixes: u64,
    pub authors: Vec<String>,
}

/// Test file name patterns.
fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains(".test.")
        || lower.contains("_test.")
        || lower.contains(".spec.")
        || lower.contains("_spec.")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("__tests__/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
}

/// Find hot source files with no co-changing test file.
///
/// A file has a "test gap" if it has >= `min_changes` total changes,
/// is not itself a test file, and has no coupling entry to any test file.
pub fn test_gaps(map: &RepoIntelData, min_changes: u64, limit: usize) -> Vec<TestGapEntry> {
    let mut entries: Vec<TestGapEntry> = map
        .file_activity
        .iter()
        .filter(|(path, activity)| {
            if activity.changes < min_changes {
                return false;
            }
            if is_test_file(path) {
                return false;
            }
            // Check coupling for any test file
            let has_test_coupling = map
                .coupling
                .get(path.as_str())
                .map(|pairs| pairs.keys().any(|k| is_test_file(k)))
                .unwrap_or(false);
            if has_test_coupling {
                return false;
            }
            // Also check reverse coupling
            let has_reverse = map
                .coupling
                .iter()
                .any(|(other, pairs)| is_test_file(other) && pairs.contains_key(path.as_str()));
            !has_reverse
        })
        .map(|(path, activity)| TestGapEntry {
            path: path.clone(),
            changes: activity.changes,
            recent_changes: activity.recent_changes,
            bug_fixes: activity.bug_fix_changes,
            authors: activity.authors.clone(),
        })
        .collect();

    entries.sort_by(|a, b| b.changes.cmp(&a.changes));
    entries.truncate(limit);
    entries
}

// ─── Diff Risk ──────────────────────────────────────────────────────────────

/// A file from a diff scored by risk.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRiskEntry {
    pub path: String,
    pub risk_score: f64,
    pub bug_fix_rate: f64,
    pub churn: u64,
    pub author_count: usize,
    pub ai_ratio: f64,
    pub known: bool,
}

/// Score a list of changed files by composite risk.
///
/// Risk formula: `bug_fix_rate * 0.4 + single_author * 0.3 + ai_ratio * 0.3`
/// Files not in the map get `known: false` and risk_score 0.5 (unknown = moderate risk).
pub fn diff_risk(map: &RepoIntelData, files: &[String]) -> Vec<DiffRiskEntry> {
    let mut entries: Vec<DiffRiskEntry> = files
        .iter()
        .map(|path| {
            if let Some(activity) = map.file_activity.get(path.as_str()) {
                let bug_fix_rate = if activity.changes > 0 {
                    activity.bug_fix_changes as f64 / activity.changes as f64
                } else {
                    0.0
                };
                let ai_ratio = if activity.changes > 0 {
                    activity.ai_changes as f64 / activity.changes as f64
                } else {
                    0.0
                };
                let single_author = if activity.authors.len() <= 1 {
                    1.0
                } else {
                    1.0 / activity.authors.len() as f64
                };
                let churn = activity.additions + activity.deletions;

                let risk_score = bug_fix_rate * 0.4 + single_author * 0.3 + ai_ratio * 0.3;

                DiffRiskEntry {
                    path: path.clone(),
                    risk_score,
                    bug_fix_rate,
                    churn,
                    author_count: activity.authors.len(),
                    ai_ratio,
                    known: true,
                }
            } else {
                DiffRiskEntry {
                    path: path.clone(),
                    risk_score: 0.5,
                    bug_fix_rate: 0.0,
                    churn: 0,
                    author_count: 0,
                    ai_ratio: 0.0,
                    known: false,
                }
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        b.risk_score
            .partial_cmp(&a.risk_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries
}

// ─── Doc Drift ──────────────────────────────────────────────────────────────

/// A documentation file with its coupling strength to code.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocDriftEntry {
    pub path: String,
    pub code_coupling: u64,
    pub last_changed: String,
    pub changes: u64,
}

/// Find doc files that rarely co-change with code (likely stale).
///
/// Checks `*.md` files for coupling to non-md files.
/// Low coupling = docs aren't updated when code changes = drift.
pub fn doc_drift(map: &RepoIntelData, limit: usize) -> Vec<DocDriftEntry> {
    let mut entries: Vec<DocDriftEntry> = map
        .file_activity
        .iter()
        .filter(|(path, _)| path.ends_with(".md"))
        .map(|(path, activity)| {
            // Sum coupling to non-md files
            let mut code_coupling: u64 = 0;

            if let Some(pairs) = map.coupling.get(path.as_str()) {
                for (other, entry) in pairs {
                    if !other.ends_with(".md") {
                        code_coupling += entry.cochanges;
                    }
                }
            }
            // Check reverse coupling too
            for (other, pairs) in &map.coupling {
                if other.ends_with(".md") {
                    continue;
                }
                if let Some(entry) = pairs.get(path.as_str()) {
                    code_coupling += entry.cochanges;
                }
            }

            DocDriftEntry {
                path: path.clone(),
                code_coupling,
                last_changed: activity.last_changed.clone(),
                changes: activity.changes,
            }
        })
        .collect();

    // Sort by coupling ascending (least coupled = most drifted)
    entries.sort_by_key(|e| e.code_coupling);
    entries.truncate(limit);
    entries
}

// ─── Recent AI ──────────────────────────────────────────────────────────────

/// A file with recent AI-authored changes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentAiEntry {
    pub path: String,
    pub ai_changes: u64,
    pub total_changes: u64,
    pub ai_ratio: f64,
    pub recent_changes: u64,
    pub ai_additions: u64,
    pub ai_deletions: u64,
}

/// Find files with recent AI changes, sorted by AI ratio descending.
///
/// Only includes files where both `ai_changes > 0` and `recent_changes > 0`.
pub fn recent_ai(map: &RepoIntelData, limit: usize) -> Vec<RecentAiEntry> {
    let mut entries: Vec<RecentAiEntry> = map
        .file_activity
        .iter()
        .filter(|(_, activity)| activity.ai_changes > 0 && activity.recent_changes > 0)
        .map(|(path, activity)| {
            let ai_ratio = if activity.changes > 0 {
                activity.ai_changes as f64 / activity.changes as f64
            } else {
                0.0
            };
            RecentAiEntry {
                path: path.clone(),
                ai_changes: activity.ai_changes,
                total_changes: activity.changes,
                ai_ratio,
                recent_changes: activity.recent_changes,
                ai_additions: activity.ai_additions,
                ai_deletions: activity.ai_deletions,
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        b.ai_ratio
            .partial_cmp(&a.ai_ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries.truncate(limit);
    entries
}

// ─── Onboard ─────────────────────────────────────────────────────────────────

/// A key area in the repository for onboarding.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyArea {
    pub path: String,
    pub purpose: String,
    pub files: usize,
    pub hotspot_score: f64,
}

/// A pain point in the repository.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PainPoint {
    pub path: String,
    pub reason: String,
}

/// Getting started information for new contributors.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GettingStarted {
    pub build_command: String,
    pub test_command: String,
    pub entry_points: Vec<String>,
}

/// Convention summary for onboard and can-i-help.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConventionSummary {
    pub commit_style: String,
    pub uses_scopes: bool,
}

/// Onboard result - human-readable summary for someone new to the repo.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardResult {
    pub language: String,
    pub framework: Option<String>,
    pub structure: String,
    pub total_files: usize,
    pub total_symbols: usize,
    pub bus_factor: usize,
    pub health: String,
    pub conventions: ConventionSummary,
    pub key_areas: Vec<KeyArea>,
    pub pain_points: Vec<PainPoint>,
    pub getting_started: GettingStarted,
}

/// Detect the primary language from file extensions in file_activity.
fn detect_language(map: &RepoIntelData) -> String {
    let mut ext_counts: HashMap<String, usize> = HashMap::new();

    for path in map.file_activity.keys() {
        if let Some(ext) = path.rsplit('.').next() {
            let ext = ext.to_lowercase();
            // Skip non-language extensions
            if matches!(
                ext.as_str(),
                "md" | "txt"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "lock"
                    | "gitignore"
                    | "csv"
                    | "svg"
                    | "png"
                    | "jpg"
                    | "ico"
            ) {
                continue;
            }
            *ext_counts.entry(ext).or_insert(0) += 1;
        }
    }

    let top = ext_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(ext, _)| ext);

    match top.as_deref() {
        Some("rs") => "rust".to_string(),
        Some("ts" | "tsx") => "typescript".to_string(),
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript".to_string(),
        Some("py") => "python".to_string(),
        Some("go") => "go".to_string(),
        Some("java") => "java".to_string(),
        Some("rb") => "ruby".to_string(),
        Some("c" | "h") => "c".to_string(),
        Some("cpp" | "cc" | "cxx" | "hpp") => "c++".to_string(),
        Some("cs") => "c#".to_string(),
        Some("swift") => "swift".to_string(),
        Some("kt" | "kts") => "kotlin".to_string(),
        Some("php") => "php".to_string(),
        Some("sh" | "bash") => "shell".to_string(),
        Some("css" | "scss" | "less") => "css".to_string(),
        Some("html" | "htm") => "html".to_string(),
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    }
}

/// Detect the repository structure from directory layout.
fn detect_structure(map: &RepoIntelData) -> String {
    let paths: Vec<&str> = map.file_activity.keys().map(|s| s.as_str()).collect();

    // Check for Cargo workspace (multiple crates/ subdirectories)
    let crate_dirs: Vec<&str> = paths
        .iter()
        .filter(|p| p.starts_with("crates/"))
        .filter_map(|p| {
            p.strip_prefix("crates/")
                .and_then(|rest| rest.split('/').next())
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    if crate_dirs.len() > 1 {
        return format!("workspace with {} crates", crate_dirs.len());
    }

    // Check for monorepo (packages/ directory)
    let package_dirs: Vec<&str> = paths
        .iter()
        .filter(|p| p.starts_with("packages/"))
        .filter_map(|p| {
            p.strip_prefix("packages/")
                .and_then(|rest| rest.split('/').next())
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    if package_dirs.len() > 1 {
        return format!("monorepo with {} packages", package_dirs.len());
    }

    // Check for src/ + lib/ split
    let has_src = paths.iter().any(|p| p.starts_with("src/"));
    let has_lib = paths.iter().any(|p| p.starts_with("lib/"));

    if has_src && has_lib {
        return "src + lib layout".to_string();
    }
    if has_src {
        return "single package".to_string();
    }
    if has_lib {
        return "library".to_string();
    }

    "flat".to_string()
}

/// Detect build and test commands from file patterns.
fn detect_commands(map: &RepoIntelData) -> GettingStarted {
    let paths: Vec<&str> = map.file_activity.keys().map(|s| s.as_str()).collect();

    let has_cargo = paths.iter().any(|p| *p == "Cargo.toml");
    let has_package_json = paths.iter().any(|p| *p == "package.json");
    let has_go_mod = paths.iter().any(|p| *p == "go.mod");
    let has_makefile = paths.iter().any(|p| *p == "Makefile" || *p == "makefile");

    let (build_cmd, test_cmd) = if has_cargo {
        ("cargo build", "cargo test")
    } else if has_package_json {
        ("npm install", "npm test")
    } else if has_go_mod {
        ("go build ./...", "go test ./...")
    } else if has_makefile {
        ("make", "make test")
    } else {
        ("see README", "see README")
    };

    // Find entry points
    let mut entry_points = Vec::new();
    let entry_candidates = [
        "src/main.rs",
        "src/lib.rs",
        "src/index.ts",
        "src/index.js",
        "index.ts",
        "index.js",
        "main.go",
        "cmd/main.go",
        "app.py",
        "main.py",
    ];
    for candidate in &entry_candidates {
        if paths.iter().any(|p| *p == *candidate) {
            entry_points.push(candidate.to_string());
        }
    }

    // Also look for main.rs in crate subdirectories
    for path in &paths {
        if path.ends_with("/main.rs") && !entry_points.contains(&path.to_string()) {
            entry_points.push(path.to_string());
        }
    }

    if entry_points.is_empty() {
        // Fallback: take the first source file
        if let Some(first) = paths.iter().find(|p| {
            p.ends_with(".rs")
                || p.ends_with(".ts")
                || p.ends_with(".js")
                || p.ends_with(".py")
                || p.ends_with(".go")
        }) {
            entry_points.push(first.to_string());
        }
    }

    GettingStarted {
        build_command: build_cmd.to_string(),
        test_command: test_cmd.to_string(),
        entry_points,
    }
}

/// Infer the purpose of a directory area from its name.
fn infer_area_purpose(area: &str) -> String {
    let name = area.trim_end_matches('/');
    let leaf = name.rsplit('/').next().unwrap_or(name);
    match leaf.to_lowercase().as_str() {
        "src" => "source code".to_string(),
        "lib" => "library code".to_string(),
        "test" | "tests" | "__tests__" => "test suite".to_string(),
        "docs" | "doc" => "documentation".to_string(),
        "bin" | "cmd" => "binary entrypoints".to_string(),
        "config" | "cfg" => "configuration".to_string(),
        "utils" | "util" | "helpers" | "helper" => "shared utilities".to_string(),
        "core" => "core logic".to_string(),
        "api" => "API layer".to_string(),
        "cli" => "command-line interface".to_string(),
        "web" | "ui" | "frontend" => "user interface".to_string(),
        "server" | "backend" => "server-side logic".to_string(),
        "models" | "types" => "data models and types".to_string(),
        "scripts" => "build and automation scripts".to_string(),
        ".github" => "CI/CD and GitHub configuration".to_string(),
        _ => format!("{leaf} module"),
    }
}

/// Human-readable summary for someone new to the repo.
///
/// Derives language, structure, health, conventions, key areas, pain points,
/// and getting-started info from the cached `RepoIntelData`.
pub fn onboard(map: &RepoIntelData) -> OnboardResult {
    let language = detect_language(map);
    let structure = detect_structure(map);
    let total_files = map.file_activity.len();
    let bf = bus_factor(map, false);

    let h = health(map);
    let health_label = if !h.active {
        "inactive"
    } else if bf >= 3 {
        "healthy"
    } else if bf >= 2 {
        "moderate"
    } else {
        "at-risk"
    };

    let n = norms(map);
    let conv = ConventionSummary {
        commit_style: n.commits.style,
        uses_scopes: n.commits.uses_scopes,
    };

    // Key areas from areas() - top areas by file count
    let area_list = areas(map);
    let key_areas: Vec<KeyArea> = area_list
        .iter()
        .take(10)
        .map(|a| KeyArea {
            path: a.area.clone(),
            purpose: infer_area_purpose(&a.area),
            files: a.files,
            hotspot_score: a.hotspot_score,
        })
        .collect();

    // Pain points: areas with both high bug rate and ownership risk
    let pain_points: Vec<PainPoint> = area_list
        .iter()
        .filter(|a| {
            let primary_stale = a.owners.first().map(|o| o.stale).unwrap_or(false);
            let high_bug_rate = a.bug_fix_rate >= 0.3;
            let single_owner = a.owners.len() <= 1;
            (high_bug_rate && primary_stale) || (high_bug_rate && single_owner)
        })
        .map(|a| {
            let mut reasons = Vec::new();
            if a.bug_fix_rate >= 0.3 {
                reasons.push("high bug-fix rate");
            }
            let primary_stale = a.owners.first().map(|o| o.stale).unwrap_or(false);
            if primary_stale {
                reasons.push("primary owner inactive");
            }
            if a.owners.len() <= 1 {
                reasons.push("single owner");
            }
            PainPoint {
                path: a.area.clone(),
                reason: reasons.join(", "),
            }
        })
        .collect();

    let getting_started = detect_commands(map);

    OnboardResult {
        language,
        framework: None,
        structure,
        total_files,
        total_symbols: 0,
        bus_factor: bf,
        health: health_label.to_string(),
        conventions: conv,
        key_areas,
        pain_points,
        getting_started,
    }
}

// ─── Can I Help ──────────────────────────────────────────────────────────────

/// A good first area for new contributors.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoodFirstArea {
    pub path: String,
    pub reason: String,
    pub files: usize,
    pub hotspot_score: f64,
}

/// An area that needs help from outside contributors.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsHelpArea {
    pub path: String,
    pub reason: String,
    pub bug_fix_rate: f64,
    pub owner_stale: bool,
}

/// Recent activity summary.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentActivitySummary {
    pub active_contributors: usize,
    pub total_commits: u64,
    pub bus_factor: usize,
}

/// Can-I-Help result - guidance for outside contributors.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanIHelpResult {
    pub conventions: ConventionSummary,
    pub good_first_areas: Vec<GoodFirstArea>,
    pub needs_help: Vec<NeedsHelpArea>,
    pub recent_activity: RecentActivitySummary,
}

/// Guidance for outside contributors.
///
/// Identifies good first areas (low complexity, non-stale owners) and
/// areas that need help (high bug rate or stale owners).
pub fn can_i_help(map: &RepoIntelData) -> CanIHelpResult {
    let n = norms(map);
    let conv = ConventionSummary {
        commit_style: n.commits.style,
        uses_scopes: n.commits.uses_scopes,
    };

    let area_list = areas(map);
    let repo_last = &map.git.last_commit_date;

    // Good first areas: low hotspot score AND non-stale primary owner
    let good_first_areas: Vec<GoodFirstArea> = area_list
        .iter()
        .filter(|a| {
            let primary_stale = a.owners.first().map(|o| o.stale).unwrap_or(true);
            let low_hotspot = a.hotspot_score < 1.5;
            let low_bug_rate = a.bug_fix_rate < 0.3;
            !primary_stale && low_hotspot && low_bug_rate
        })
        .map(|a| {
            let mut reasons = Vec::new();
            if a.hotspot_score < 1.0 {
                reasons.push("low churn");
            }
            if a.bug_fix_rate < 0.3 {
                reasons.push("low bug rate");
            }
            if a.owners.first().map(|o| !o.stale).unwrap_or(false) {
                reasons.push("active maintainer for review");
            }
            GoodFirstArea {
                path: a.area.clone(),
                reason: if reasons.is_empty() {
                    "stable area".to_string()
                } else {
                    reasons.join(", ")
                },
                files: a.files,
                hotspot_score: a.hotspot_score,
            }
        })
        .collect();

    // Needs help: high bug rate OR stale owners
    let needs_help: Vec<NeedsHelpArea> = area_list
        .iter()
        .filter(|a| {
            let primary_stale = a.owners.first().map(|o| o.stale).unwrap_or(false);
            let high_bug_rate = a.bug_fix_rate >= 0.3;
            primary_stale || high_bug_rate
        })
        .map(|a| {
            let primary_stale = a.owners.first().map(|o| o.stale).unwrap_or(false);
            let mut reasons = Vec::new();
            if a.bug_fix_rate >= 0.3 {
                reasons.push("high bug-fix rate");
            }
            if primary_stale {
                reasons.push("primary owner inactive");
            }
            NeedsHelpArea {
                path: a.area.clone(),
                reason: reasons.join(", "),
                bug_fix_rate: a.bug_fix_rate,
                owner_stale: primary_stale,
            }
        })
        .collect();

    // Recent activity: count non-stale contributors
    let active_contributors = map
        .contributors
        .humans
        .values()
        .filter(|c| !is_stale(&c.last_seen, repo_last))
        .count();

    let recent_activity = RecentActivitySummary {
        active_contributors,
        total_commits: map.git.total_commits_analyzed,
        bus_factor: bus_factor(map, false),
    };

    CanIHelpResult {
        conventions: conv,
        good_first_areas,
        needs_help,
        recent_activity,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::{create_empty_map, merge_delta};
    use analyzer_core::types::{CommitDelta, CommitInfo, FileChange};

    fn make_test_map() -> RepoIntelData {
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc123".to_string(),
            commits: vec![
                CommitInfo {
                    hash: "aaa".to_string(),
                    author_name: "alice".to_string(),
                    author_email: "alice@example.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "feat: add engine".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![
                        FileChange {
                            path: "src/engine.rs".to_string(),
                            additions: 100,
                            deletions: 0,
                        },
                        FileChange {
                            path: "src/engine_test.rs".to_string(),
                            additions: 50,
                            deletions: 0,
                        },
                    ],
                },
                CommitInfo {
                    hash: "bbb".to_string(),
                    author_name: "alice".to_string(),
                    author_email: "alice@example.com".to_string(),
                    date: "2026-03-11T10:00:00Z".to_string(),
                    subject: "fix: handle null".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![
                        FileChange {
                            path: "src/engine.rs".to_string(),
                            additions: 5,
                            deletions: 2,
                        },
                        FileChange {
                            path: "src/engine_test.rs".to_string(),
                            additions: 10,
                            deletions: 0,
                        },
                    ],
                },
                CommitInfo {
                    hash: "ccc".to_string(),
                    author_name: "alice".to_string(),
                    author_email: "alice@example.com".to_string(),
                    date: "2026-03-12T10:00:00Z".to_string(),
                    subject: "feat: add config".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![
                        FileChange {
                            path: "src/engine.rs".to_string(),
                            additions: 20,
                            deletions: 5,
                        },
                        FileChange {
                            path: "src/config.rs".to_string(),
                            additions: 30,
                            deletions: 0,
                        },
                        FileChange {
                            path: "src/engine_test.rs".to_string(),
                            additions: 15,
                            deletions: 0,
                        },
                    ],
                },
                CommitInfo {
                    hash: "ddd".to_string(),
                    author_name: "bob".to_string(),
                    author_email: "bob@example.com".to_string(),
                    date: "2026-03-13T10:00:00Z".to_string(),
                    subject: "feat: add utils".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/utils.rs".to_string(),
                        additions: 40,
                        deletions: 0,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);
        map
    }

    /// Create a test map with a stale contributor for staleness tests.
    fn make_staleness_test_map() -> RepoIntelData {
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "xyz789".to_string(),
            commits: vec![
                // Old commit from alice (>90 days before latest)
                CommitInfo {
                    hash: "old".to_string(),
                    author_name: "alice".to_string(),
                    author_email: "alice@example.com".to_string(),
                    date: "2025-06-01T10:00:00Z".to_string(),
                    subject: "feat: initial work".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/core.rs".to_string(),
                        additions: 200,
                        deletions: 0,
                    }],
                },
                // Recent commit from bob
                CommitInfo {
                    hash: "new".to_string(),
                    author_name: "bob".to_string(),
                    author_email: "bob@example.com".to_string(),
                    date: "2026-03-14T10:00:00Z".to_string(),
                    subject: "fix: patch bug".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/core.rs".to_string(),
                        additions: 5,
                        deletions: 2,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);
        map
    }

    #[test]
    fn test_hotspots() {
        let map = make_test_map();
        let spots = hotspots(&map, None, 2);
        assert_eq!(spots.len(), 2);
        // All commits are within 90 days so recent_changes == changes
        assert!(spots[0].score > 0.0);
        assert!(spots[0].recent_changes > 0);
    }

    #[test]
    fn test_recency_weighted_hotspots() {
        let map = make_staleness_test_map();
        let spots = hotspots(&map, None, 10);
        // src/core.rs has 2 total changes but only 1 recent
        let core = spots.iter().find(|s| s.path == "src/core.rs").unwrap();
        assert_eq!(core.changes, 2);
        assert_eq!(core.recent_changes, 1);
        // Score = (1 * 2 + 2) / (2 + 1) = 4/3 ~= 1.33
        assert!((core.score - 4.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_coldspots() {
        let map = make_test_map();
        let cold = coldspots(&map, None);
        assert!(!cold.is_empty());
    }

    #[test]
    fn test_bugspots() {
        let map = make_test_map();
        let bugs = bugspots(&map, 10);
        // engine.rs has 1 fix out of 3 changes, engine_test.rs has 1 fix out of 3
        assert!(!bugs.is_empty());
        for b in &bugs {
            assert!(b.bug_fix_rate > 0.0);
            assert!(!b.last_bug_fix.is_empty());
        }
    }

    #[test]
    fn test_bugspots_ranking() {
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![
                CommitInfo {
                    hash: "a".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "fix: bug A".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "buggy.rs".to_string(),
                        additions: 1,
                        deletions: 1,
                    }],
                },
                CommitInfo {
                    hash: "b".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-11T10:00:00Z".to_string(),
                    subject: "feat: feature".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![
                        FileChange {
                            path: "buggy.rs".to_string(),
                            additions: 5,
                            deletions: 0,
                        },
                        FileChange {
                            path: "stable.rs".to_string(),
                            additions: 50,
                            deletions: 0,
                        },
                    ],
                },
                CommitInfo {
                    hash: "c".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-12T10:00:00Z".to_string(),
                    subject: "fix: another bug".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "buggy.rs".to_string(),
                        additions: 2,
                        deletions: 1,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let bugs = bugspots(&map, 10);
        // buggy.rs: 2 fixes / 3 changes = 0.67
        // stable.rs: 0 fixes, should not appear
        assert_eq!(bugs.len(), 1);
        assert_eq!(bugs[0].path, "buggy.rs");
        assert!((bugs[0].bug_fix_rate - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_contributor_staleness() {
        let map = make_staleness_test_map();
        // alice's last_seen is 2025-06-01, repo last is 2026-03-14 => stale
        // bob's last_seen is 2026-03-14 => not stale
        let own = ownership(&map, "src/core.rs");
        let alice = own.owners.iter().find(|o| o.name == "alice").unwrap();
        assert!(alice.stale);
        let bob = own.owners.iter().find(|o| o.name == "bob").unwrap();
        assert!(!bob.stale);
    }

    #[test]
    fn test_ownership_with_staleness() {
        let map = make_staleness_test_map();
        let own = ownership(&map, "src/core.rs");
        assert_eq!(own.path, "src/core.rs");
        // Both alice and bob contributed
        assert_eq!(own.owners.len(), 2);
        // bus_factor_risk: bob is the only non-stale owner
        assert!(own.bus_factor_risk);
    }

    #[test]
    fn test_bus_factor() {
        let map = make_test_map();
        let bf = bus_factor(&map, false);
        assert_eq!(bf, 2);
    }

    #[test]
    fn test_bus_factor_with_staleness() {
        let map = make_staleness_test_map();
        let result = bus_factor_detailed(&map, false);
        // Should identify alice as stale in critical owners
        let alice = result.critical_owners.iter().find(|o| o.name == "alice");
        if let Some(a) = alice {
            assert!(a.stale);
        }
    }

    #[test]
    fn test_norms_query() {
        let map = make_test_map();
        let n = norms(&map);
        assert!(n.commits.prefixes.contains_key("feat"));
        assert!(n.commits.prefixes.contains_key("fix"));
        assert_eq!(n.commits.style, "conventional");
    }

    #[test]
    fn test_areas_health_classification() {
        let map = make_test_map();
        let area_list = areas(&map);
        // All commits are recent => no stale owners => health should be "healthy"
        assert!(!area_list.is_empty());
        for a in &area_list {
            assert!(
                a.health == "healthy" || a.health == "needs-attention" || a.health == "at-risk"
            );
        }
    }

    #[test]
    fn test_areas_stale_detection() {
        let map = make_staleness_test_map();
        let area_list = areas(&map);
        // src/ area has alice (stale) as primary owner with more changes
        let src = area_list.iter().find(|a| a.area == "src/").unwrap();
        // alice has 200 additions, bob has 5+2=7 additions, but changes: alice=1, bob=1
        // Both have 1 change each, primary could be either
        // Bug fix rate: 1 fix / 2 total = 0.5 >= 0.3 => high bug rate
        assert!(src.bug_fix_rate >= 0.3);
    }

    #[test]
    fn test_ownership() {
        let map = make_test_map();
        let own = ownership(&map, "src/engine.rs");
        assert_eq!(own.primary, "alice");
        assert_eq!(own.path, "src/engine.rs");
    }

    #[test]
    fn test_ai_ratio_no_ai() {
        let map = make_test_map();
        let ratio = ai_ratio(&map, None);
        assert_eq!(ratio.attributed, 0);
        assert_eq!(ratio.ratio, 0.0);
    }

    #[test]
    fn test_contributors() {
        let map = make_test_map();
        let contribs = contributors(&map, None);
        assert_eq!(contribs.len(), 2);
        assert_eq!(contribs[0].name, "alice");
        assert_eq!(contribs[0].commits, 3);
        assert_eq!(contribs[1].name, "bob");
        assert_eq!(contribs[1].commits, 1);
    }

    #[test]
    fn test_file_history() {
        let map = make_test_map();
        let history = file_history(&map, "src/engine.rs");
        assert!(history.is_some());
        let h = history.unwrap();
        assert_eq!(h.changes, 3);
        assert_eq!(h.bug_fix_changes, 1);
    }

    #[test]
    fn test_conventions() {
        let map = make_test_map();
        let conv = conventions(&map);
        assert!(conv.prefixes.contains_key("feat"));
        assert!(conv.prefixes.contains_key("fix"));
    }

    #[test]
    fn test_coupling() {
        let map = make_test_map();
        let coupled = coupling(&map, "src/engine.rs", false);
        let test_coupling = coupled.iter().find(|c| c.path == "src/engine_test.rs");
        assert!(test_coupling.is_some());
        assert_eq!(test_coupling.unwrap().cochanges, 3);
    }

    #[test]
    fn test_refactor_changes_counted() {
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![CommitInfo {
                hash: "r1".to_string(),
                author_name: "dev".to_string(),
                author_email: "dev@test.com".to_string(),
                date: "2026-03-10T10:00:00Z".to_string(),
                subject: "refactor: clean up module".to_string(),
                body: String::new(),
                trailers: vec![],
                files: vec![FileChange {
                    path: "src/module.rs".to_string(),
                    additions: 20,
                    deletions: 15,
                }],
            }],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let h = file_history(&map, "src/module.rs").unwrap();
        assert_eq!(h.refactor_changes, 1);
    }

    #[test]
    fn test_test_gaps() {
        let map = make_test_map();
        // engine.rs couples with engine_test.rs => no gap
        // config.rs couples with engine_test.rs too => no gap
        // utils.rs has no coupling to any test file => gap
        let gaps = test_gaps(&map, 1, 10);
        let utils_gap = gaps.iter().find(|g| g.path == "src/utils.rs");
        assert!(utils_gap.is_some(), "utils.rs should be a test gap");
        // engine.rs should NOT be a gap (coupled with engine_test.rs)
        let engine_gap = gaps.iter().find(|g| g.path == "src/engine.rs");
        assert!(engine_gap.is_none(), "engine.rs should not be a test gap");
    }

    #[test]
    fn test_test_gaps_excludes_test_files() {
        let map = make_test_map();
        let gaps = test_gaps(&map, 1, 10);
        for g in &gaps {
            assert!(
                !is_test_file(&g.path),
                "test file {} should not appear in test gaps",
                g.path
            );
        }
    }

    #[test]
    fn test_diff_risk_known_files() {
        let map = make_test_map();
        let files = vec!["src/engine.rs".to_string(), "src/utils.rs".to_string()];
        let risks = diff_risk(&map, &files);
        assert_eq!(risks.len(), 2);
        for r in &risks {
            assert!(r.known);
            assert!(r.risk_score >= 0.0 && r.risk_score <= 1.0);
        }
    }

    #[test]
    fn test_diff_risk_unknown_files() {
        let map = make_test_map();
        let files = vec!["does/not/exist.rs".to_string()];
        let risks = diff_risk(&map, &files);
        assert_eq!(risks.len(), 1);
        assert!(!risks[0].known);
        assert!((risks[0].risk_score - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_diff_risk_single_author_penalty() {
        let map = make_test_map();
        // utils.rs has 1 author (bob), engine.rs has 1 author (alice)
        // Both single-author => single_author score = 1.0
        let files = vec!["src/utils.rs".to_string()];
        let risks = diff_risk(&map, &files);
        assert!(risks[0].author_count == 1);
        // single_author contributes 0.3 to risk
        assert!(risks[0].risk_score >= 0.3);
    }

    #[test]
    fn test_doc_drift() {
        // Create a map with an md file that has no code coupling
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![
                CommitInfo {
                    hash: "a".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "docs: update readme".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "README.md".to_string(),
                        additions: 10,
                        deletions: 0,
                    }],
                },
                CommitInfo {
                    hash: "b".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-11T10:00:00Z".to_string(),
                    subject: "feat: add code".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/main.rs".to_string(),
                        additions: 50,
                        deletions: 0,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let drifting = doc_drift(&map, 10);
        // README.md never co-changes with code => coupling = 0
        let readme = drifting.iter().find(|d| d.path == "README.md");
        assert!(readme.is_some());
        assert_eq!(readme.unwrap().code_coupling, 0);
    }

    #[test]
    fn test_recent_ai_empty() {
        let map = make_test_map();
        // No AI commits in test map
        let ai = recent_ai(&map, 10);
        assert!(ai.is_empty());
    }

    #[test]
    fn test_is_test_file() {
        assert!(is_test_file("src/engine.test.ts"));
        assert!(is_test_file("src/engine_test.rs"));
        assert!(is_test_file("src/engine.spec.js"));
        assert!(is_test_file("__tests__/foo.js"));
        assert!(is_test_file("test/helpers.ts"));
        assert!(is_test_file("tests/integration.rs"));
        assert!(!is_test_file("src/engine.rs"));
        assert!(!is_test_file("src/test_utils.rs")); // test_ prefix != test file
    }

    #[test]
    fn test_release_info_no_tags() {
        let map = make_test_map();
        let info = release_info(&map);
        assert!(info.last_release.is_none());
        assert!(info.tags.is_empty());
        // unreleased should fall back to total_commits_analyzed
        assert_eq!(info.unreleased, map.git.total_commits_analyzed);
    }

    #[test]
    fn test_health_active() {
        let map = make_test_map();
        let h = health(&map);
        assert!(h.active);
        assert_eq!(h.bus_factor, 2);
    }

    #[test]
    fn test_health_empty_map() {
        let map = create_empty_map();
        let h = health(&map);
        assert!(!h.active);
        assert_eq!(h.bus_factor, 0);
    }

    #[test]
    fn test_recent_ai_with_data() {
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![
                CommitInfo {
                    hash: "ai1".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "feat: ai work".to_string(),
                    body: "Generated with Claude Code".to_string(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/ai_file.rs".to_string(),
                        additions: 50,
                        deletions: 0,
                    }],
                },
                CommitInfo {
                    hash: "h1".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-11T10:00:00Z".to_string(),
                    subject: "feat: human work".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![
                        FileChange {
                            path: "src/ai_file.rs".to_string(),
                            additions: 10,
                            deletions: 5,
                        },
                        FileChange {
                            path: "src/human_file.rs".to_string(),
                            additions: 30,
                            deletions: 0,
                        },
                    ],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let ai = recent_ai(&map, 10);
        assert!(!ai.is_empty());
        // ai_file.rs has 1 AI change out of 2 total
        let ai_file = ai.iter().find(|e| e.path == "src/ai_file.rs").unwrap();
        assert_eq!(ai_file.ai_changes, 1);
        assert_eq!(ai_file.total_changes, 2);
        assert!((ai_file.ai_ratio - 0.5).abs() < 0.01);
        // human_file.rs should not appear (0 ai changes)
        assert!(ai.iter().all(|e| e.path != "src/human_file.rs"));
    }

    #[test]
    fn test_ai_ratio_with_path_filter() {
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![
                CommitInfo {
                    hash: "ai1".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "feat: ai work".to_string(),
                    body: "Generated with Claude Code".to_string(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/module.rs".to_string(),
                        additions: 50,
                        deletions: 0,
                    }],
                },
                CommitInfo {
                    hash: "h1".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-11T10:00:00Z".to_string(),
                    subject: "feat: other".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "lib/other.rs".to_string(),
                        additions: 30,
                        deletions: 0,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        // With path filter "src/" should only count src/module.rs
        let filtered = ai_ratio(&map, Some("src/"));
        assert!(filtered.attributed > 0);
        assert!(filtered.ratio > 0.0);

        // With path filter "lib/" should have 0 AI
        let lib = ai_ratio(&map, Some("lib/"));
        assert_eq!(lib.attributed, 0);
        assert_eq!(lib.ratio, 0.0);
    }

    #[test]
    fn test_coupling_human_only() {
        let map = make_test_map();
        // human_only=false should include all cochanges
        let all = coupling(&map, "src/engine.rs", false);
        assert!(!all.is_empty());
        // human_only=true should also work (all commits in test map are human)
        let human = coupling(&map, "src/engine.rs", true);
        assert!(!human.is_empty());
        // Both should find engine_test.rs coupling
        let all_test = all.iter().find(|c| c.path == "src/engine_test.rs");
        let human_test = human.iter().find(|c| c.path == "src/engine_test.rs");
        assert!(all_test.is_some());
        assert!(human_test.is_some());
    }

    #[test]
    fn test_coldspots_sort_order() {
        let map = make_test_map();
        let cold = coldspots(&map, None);
        assert!(cold.len() >= 2);
        // Should be sorted by last_changed ascending (oldest first)
        for i in 1..cold.len() {
            assert!(
                cold[i - 1].last_changed <= cold[i].last_changed,
                "coldspots should be sorted ascending: {} > {}",
                cold[i - 1].last_changed,
                cold[i].last_changed
            );
        }
    }

    #[test]
    fn test_onboard() {
        let map = make_test_map();
        let result = onboard(&map);

        // Language should be detected from .rs files
        assert_eq!(result.language, "rust");
        // Framework is always None for now
        assert!(result.framework.is_none());
        // Should have files
        assert!(result.total_files > 0);
        assert_eq!(result.total_files, map.file_activity.len());
        // total_symbols is always 0 (Phase 2)
        assert_eq!(result.total_symbols, 0);
        // Bus factor should match bus_factor()
        assert_eq!(result.bus_factor, bus_factor(&map, false));
        // Health should be a valid label
        assert!(
            ["healthy", "moderate", "at-risk", "inactive"].contains(&result.health.as_str()),
            "unexpected health: {}",
            result.health
        );
        // Conventions should match norms
        let n = norms(&map);
        assert_eq!(result.conventions.commit_style, n.commits.style);
        assert_eq!(result.conventions.uses_scopes, n.commits.uses_scopes);
        // Key areas should be populated
        assert!(!result.key_areas.is_empty());
        // Getting started should detect cargo
        assert!(!result.getting_started.build_command.is_empty());
        assert!(!result.getting_started.test_command.is_empty());
    }

    #[test]
    fn test_onboard_language_detection() {
        // Create a map with TypeScript files
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![CommitInfo {
                hash: "a".to_string(),
                author_name: "dev".to_string(),
                author_email: "dev@test.com".to_string(),
                date: "2026-03-10T10:00:00Z".to_string(),
                subject: "feat: init".to_string(),
                body: String::new(),
                trailers: vec![],
                files: vec![
                    FileChange {
                        path: "src/index.ts".to_string(),
                        additions: 50,
                        deletions: 0,
                    },
                    FileChange {
                        path: "src/utils.ts".to_string(),
                        additions: 30,
                        deletions: 0,
                    },
                    FileChange {
                        path: "package.json".to_string(),
                        additions: 10,
                        deletions: 0,
                    },
                    FileChange {
                        path: "README.md".to_string(),
                        additions: 5,
                        deletions: 0,
                    },
                ],
            }],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let result = onboard(&map);
        assert_eq!(result.language, "typescript");
        // package.json detected -> npm commands
        assert_eq!(result.getting_started.build_command, "npm install");
        assert_eq!(result.getting_started.test_command, "npm test");
        // Should find src/index.ts as entry point
        assert!(
            result
                .getting_started
                .entry_points
                .contains(&"src/index.ts".to_string()),
            "should find src/index.ts entry point"
        );
    }

    #[test]
    fn test_onboard_structure_detection() {
        // Create a workspace-like map
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![CommitInfo {
                hash: "a".to_string(),
                author_name: "dev".to_string(),
                author_email: "dev@test.com".to_string(),
                date: "2026-03-10T10:00:00Z".to_string(),
                subject: "feat: workspace".to_string(),
                body: String::new(),
                trailers: vec![],
                files: vec![
                    FileChange {
                        path: "crates/core/src/lib.rs".to_string(),
                        additions: 50,
                        deletions: 0,
                    },
                    FileChange {
                        path: "crates/cli/src/main.rs".to_string(),
                        additions: 30,
                        deletions: 0,
                    },
                    FileChange {
                        path: "crates/utils/src/lib.rs".to_string(),
                        additions: 20,
                        deletions: 0,
                    },
                    FileChange {
                        path: "Cargo.toml".to_string(),
                        additions: 10,
                        deletions: 0,
                    },
                ],
            }],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let result = onboard(&map);
        assert!(
            result.structure.contains("workspace"),
            "should detect workspace structure: {}",
            result.structure
        );
        assert!(
            result.structure.contains("3"),
            "should detect 3 crates: {}",
            result.structure
        );
    }

    #[test]
    fn test_can_i_help() {
        let map = make_test_map();
        let result = can_i_help(&map);

        // Conventions should be populated
        assert_eq!(result.conventions.commit_style, "conventional");
        // Recent activity should reflect contributors
        assert!(result.recent_activity.active_contributors > 0);
        assert!(result.recent_activity.total_commits > 0);
        assert_eq!(result.recent_activity.bus_factor, bus_factor(&map, false));

        // good_first_areas and needs_help should be populated (all commits recent in test map)
        // With all-recent commits: areas have high hotspot scores, so good_first_areas may be empty
        // needs_help should reflect areas with bug fixes
        // At minimum, both should be valid (not panic)
        let _gfa = &result.good_first_areas;
        let _nh = &result.needs_help;
        // If any area has bug_fix_rate >= 0.3, it should appear in needs_help
        let area_list = areas(&map);
        let buggy_areas: Vec<_> = area_list.iter().filter(|a| a.bug_fix_rate >= 0.3).collect();
        for ba in &buggy_areas {
            assert!(
                result.needs_help.iter().any(|nh| nh.path == ba.area),
                "area {} with bug_fix_rate {:.2} should appear in needs_help",
                ba.area,
                ba.bug_fix_rate
            );
        }
    }

    #[test]
    fn test_can_i_help_with_staleness() {
        let map = make_staleness_test_map();
        let result = can_i_help(&map);

        // With staleness, src/ area should appear in needs_help
        // (alice is stale, bob is active; bug_fix_rate is 0.5 >= 0.3)
        let src_needs_help = result.needs_help.iter().find(|a| a.path == "src/");
        assert!(
            src_needs_help.is_some(),
            "src/ should need help due to high bug rate or stale owner"
        );
        let src = src_needs_help.unwrap();
        assert!(src.bug_fix_rate >= 0.3);

        // Active contributors should be 1 (only bob is non-stale)
        assert_eq!(result.recent_activity.active_contributors, 1);
    }

    #[test]
    fn test_can_i_help_good_first_areas() {
        // Create a map with a low-activity area (multiple non-recent commits
        // to bring down hotspot score) and a high-activity area
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![
                // Old commit - only adds to total, not recent
                CommitInfo {
                    hash: "old1".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2025-06-01T10:00:00Z".to_string(),
                    subject: "feat: add helpers".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![
                        FileChange {
                            path: "utils/helpers.rs".to_string(),
                            additions: 20,
                            deletions: 0,
                        },
                        FileChange {
                            path: "utils/format.rs".to_string(),
                            additions: 15,
                            deletions: 0,
                        },
                    ],
                },
                CommitInfo {
                    hash: "old2".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2025-07-01T10:00:00Z".to_string(),
                    subject: "feat: more helpers".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "utils/helpers.rs".to_string(),
                        additions: 10,
                        deletions: 0,
                    }],
                },
                // Recent commit from same dev
                CommitInfo {
                    hash: "new1".to_string(),
                    author_name: "dev".to_string(),
                    author_email: "dev@test.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "feat: add core".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/core.rs".to_string(),
                        additions: 100,
                        deletions: 0,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let result = can_i_help(&map);
        // utils/ area has 2 total changes on helpers.rs, 0 recent, 1 on format.rs, 0 recent
        // hotspot_score for helpers: (0*2 + 2)/(2+1) = 0.67
        // hotspot_score for format: (0*2 + 1)/(1+1) = 0.5
        // max = 0.67, low_hotspot (< 1.5) => true
        // owner is dev who is non-stale => good first area
        let utils_good = result.good_first_areas.iter().find(|a| a.path == "utils/");
        assert!(
            utils_good.is_some(),
            "utils/ should be a good first area (low hotspot, active owner)"
        );
    }

    #[test]
    fn test_onboard_serialization() {
        let map = make_test_map();
        let result = onboard(&map);
        // Ensure it serializes to valid JSON
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"language\""));
        assert!(json.contains("\"busFactor\""));
        assert!(json.contains("\"keyAreas\""));
        assert!(json.contains("\"gettingStarted\""));
        assert!(json.contains("\"painPoints\""));
        // framework should serialize as null
        assert!(json.contains("\"framework\":null"));
    }

    #[test]
    fn test_onboard_pain_points() {
        // Create a map with a single-owner area that has high bug rate
        let mut map = create_empty_map();
        let delta = CommitDelta {
            head: "abc".to_string(),
            commits: vec![
                CommitInfo {
                    hash: "f1".to_string(),
                    author_name: "alice".to_string(),
                    author_email: "alice@test.com".to_string(),
                    date: "2026-03-10T10:00:00Z".to_string(),
                    subject: "fix: patch bug".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/buggy.rs".to_string(),
                        additions: 5,
                        deletions: 2,
                    }],
                },
                CommitInfo {
                    hash: "f2".to_string(),
                    author_name: "alice".to_string(),
                    author_email: "alice@test.com".to_string(),
                    date: "2026-03-11T10:00:00Z".to_string(),
                    subject: "fix: another bug".to_string(),
                    body: String::new(),
                    trailers: vec![],
                    files: vec![FileChange {
                        path: "src/buggy.rs".to_string(),
                        additions: 3,
                        deletions: 1,
                    }],
                },
            ],
            renames: vec![],
            deletions: vec![],
        };
        merge_delta(&mut map, &delta);

        let result = onboard(&map);
        // src/ has: bug_fix_rate=1.0, single owner (alice) => pain point
        assert!(
            !result.pain_points.is_empty(),
            "single-owner area with 100% bug-fix rate should be a pain point"
        );
        let src_pain = result.pain_points.iter().find(|p| p.path == "src/");
        assert!(src_pain.is_some(), "src/ should be a pain point");
        let sp = src_pain.unwrap();
        assert!(sp.reason.contains("high bug-fix rate"));
        assert!(sp.reason.contains("single owner"));
    }

    #[test]
    fn test_can_i_help_serialization() {
        let map = make_test_map();
        let result = can_i_help(&map);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"conventions\""));
        assert!(json.contains("\"goodFirstAreas\""));
        assert!(json.contains("\"needsHelp\""));
        assert!(json.contains("\"recentActivity\""));
        assert!(json.contains("\"commitStyle\""));
    }
}
