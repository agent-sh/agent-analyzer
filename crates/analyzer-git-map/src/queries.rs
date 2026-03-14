use std::collections::HashMap;

use serde::Serialize;

use analyzer_core::types::{FileActivity, GitMapData};

/// A hotspot entry - a file with high recent activity.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotEntry {
    pub path: String,
    pub changes: u64,
    pub authors: Vec<String>,
    pub ai_ratio: f64,
    pub bug_fixes: u64,
}

/// A coldspot entry - a file with no recent activity.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColdspotEntry {
    pub path: String,
    pub last_changed: String,
}

/// Coupling result for a file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CouplingResult {
    pub path: String,
    pub strength: f64,
    pub cochanges: u64,
    pub human_cochanges: u64,
}

/// Ownership result for a directory or file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipResult {
    pub primary: String,
    pub pct: f64,
    pub contributors: Vec<ContributorEntry>,
    pub ai_ratio: f64,
}

/// A contributor entry with stats.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContributorEntry {
    pub name: String,
    pub commits: u64,
    pub pct: f64,
    pub ai_assisted_pct: f64,
}

/// AI ratio result.
#[derive(Debug, Clone, Serialize)]
pub struct AiRatioResult {
    pub ratio: f64,
    pub attributed: u64,
    pub total: u64,
    pub tools: HashMap<String, u64>,
}

/// Release info result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub cadence: String,
    pub last_release: Option<String>,
    pub unreleased: u64,
    pub tags: Vec<analyzer_core::types::ReleaseTag>,
}

/// Health result for the repository.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthResult {
    pub active: bool,
    pub bus_factor: usize,
    pub commit_frequency: f64,
    pub ai_ratio: f64,
}

/// Commit shape result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitShapeResult {
    pub typical_size: String,
    pub files_per_commit_median: u64,
    pub files_per_commit_p90: u64,
    pub merge_commit_ratio: f64,
}

/// Convention result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConventionResult {
    pub style: String,
    pub prefixes: HashMap<String, u64>,
    pub uses_scopes: bool,
    pub samples: Vec<String>,
}

/// Get hotspot files - files with the most changes, sorted by change count.
pub fn hotspots(map: &GitMapData, _months: Option<u32>, limit: usize) -> Vec<HotspotEntry> {
    let mut entries: Vec<HotspotEntry> = map
        .file_activity
        .iter()
        .map(|(path, activity)| {
            let ai_ratio = if activity.changes > 0 {
                activity.ai_changes as f64 / activity.changes as f64
            } else {
                0.0
            };
            HotspotEntry {
                path: path.clone(),
                changes: activity.changes,
                authors: activity.authors.clone(),
                ai_ratio,
                bug_fixes: activity.bug_fix_changes,
            }
        })
        .collect();

    entries.sort_by(|a, b| b.changes.cmp(&a.changes));
    entries.truncate(limit);
    entries
}

/// Get coldspot files - files with no recent changes.
pub fn coldspots(map: &GitMapData, _months: Option<u32>) -> Vec<ColdspotEntry> {
    let mut entries: Vec<ColdspotEntry> = map
        .file_activity
        .iter()
        .map(|(path, activity)| ColdspotEntry {
            path: path.clone(),
            last_changed: activity.last_changed.clone(),
        })
        .collect();

    // Sort by last_changed ascending (oldest first)
    entries.sort_by(|a, b| a.last_changed.cmp(&b.last_changed));
    entries
}

/// Get files coupled with a given file.
pub fn coupling(map: &GitMapData, file: &str, human_only: bool) -> Vec<CouplingResult> {
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

/// Get ownership information for a directory or file path.
pub fn ownership(map: &GitMapData, dir_or_file: &str) -> OwnershipResult {
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

    let mut contributors: Vec<ContributorEntry> = author_changes
        .iter()
        .map(|(name, &changes)| {
            let pct = if total_changes > 0 {
                (changes as f64 / total_changes as f64) * 100.0
            } else {
                0.0
            };
            let ai_assisted_pct = map
                .contributors
                .humans
                .get(name)
                .map(|c| {
                    if c.commits > 0 {
                        (c.ai_assisted_commits as f64 / c.commits as f64) * 100.0
                    } else {
                        0.0
                    }
                })
                .unwrap_or(0.0);

            ContributorEntry {
                name: name.clone(),
                commits: changes,
                pct,
                ai_assisted_pct,
            }
        })
        .collect();

    contributors.sort_by(|a, b| b.commits.cmp(&a.commits));

    let primary = contributors
        .first()
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let primary_pct = contributors.first().map(|c| c.pct).unwrap_or(0.0);
    let ai_ratio = if total_changes > 0 {
        ai_changes as f64 / total_changes as f64
    } else {
        0.0
    };

    OwnershipResult {
        primary,
        pct: primary_pct,
        contributors,
        ai_ratio,
    }
}

/// Calculate bus factor - the minimum number of people covering 80% of commits.
pub fn bus_factor(map: &GitMapData, adjust_for_ai: bool) -> usize {
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
        return 0;
    }

    let threshold = total * 0.8;
    let mut accumulated = 0.0;
    let mut count = 0;

    for (_, commits) in &human_commits {
        accumulated += commits;
        count += 1;
        if accumulated >= threshold {
            break;
        }
    }

    count
}

/// Get contributors filtered by recent activity.
pub fn contributors(map: &GitMapData, _months: Option<u32>) -> Vec<ContributorEntry> {
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

/// Get AI ratio for the entire repo or a path filter.
pub fn ai_ratio(map: &GitMapData, path_filter: Option<&str>) -> AiRatioResult {
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

/// Get release information.
pub fn release_info(map: &GitMapData) -> ReleaseInfo {
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

/// Get repository health summary.
pub fn health(map: &GitMapData) -> HealthResult {
    let bf = bus_factor(map, false);
    let total = map.ai_attribution.attributed + map.ai_attribution.none;
    let ai_r = if total > 0 {
        map.ai_attribution.attributed as f64 / total as f64
    } else {
        0.0
    };

    // Simple activity check: has commits
    let active = map.git.total_commits_analyzed > 0;

    HealthResult {
        active,
        bus_factor: bf,
        commit_frequency: map.git.total_commits_analyzed as f64,
        ai_ratio: ai_r,
    }
}

/// Get file history for a specific path.
pub fn file_history<'a>(map: &'a GitMapData, path: &str) -> Option<&'a FileActivity> {
    map.file_activity.get(path)
}

/// Get commit shape statistics.
pub fn commit_shape(map: &GitMapData) -> CommitShapeResult {
    let dist = &map.commit_shape.size_distribution;
    let typical = if dist.tiny >= dist.small
        && dist.tiny >= dist.medium
        && dist.tiny >= dist.large
        && dist.tiny >= dist.huge
    {
        "tiny"
    } else if dist.small >= dist.medium && dist.small >= dist.large && dist.small >= dist.huge {
        "small"
    } else if dist.medium >= dist.large && dist.medium >= dist.huge {
        "medium"
    } else if dist.large >= dist.huge {
        "large"
    } else {
        "huge"
    };

    let total = map.git.total_commits_analyzed + map.commit_shape.merge_commits;
    let merge_ratio = if total > 0 {
        map.commit_shape.merge_commits as f64 / total as f64
    } else {
        0.0
    };

    CommitShapeResult {
        typical_size: typical.to_string(),
        files_per_commit_median: map.commit_shape.files_per_commit.median,
        files_per_commit_p90: map.commit_shape.files_per_commit.p90,
        merge_commit_ratio: merge_ratio,
    }
}

/// Get convention statistics.
pub fn conventions(map: &GitMapData) -> ConventionResult {
    ConventionResult {
        style: map.conventions.style.clone(),
        prefixes: map.conventions.prefixes.clone(),
        uses_scopes: map.conventions.uses_scopes,
        samples: map.conventions.samples.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::{create_empty_map, merge_delta};
    use analyzer_core::types::{CommitDelta, CommitInfo, FileChange};

    fn make_test_map() -> GitMapData {
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

    #[test]
    fn test_hotspots() {
        let map = make_test_map();
        let spots = hotspots(&map, None, 2);
        assert_eq!(spots.len(), 2);
        // engine.rs and engine_test.rs both have 3 changes; either can be first
        assert_eq!(spots[0].changes, 3);
        assert!(spots[0].path == "src/engine.rs" || spots[0].path == "src/engine_test.rs");
    }

    #[test]
    fn test_coldspots() {
        let map = make_test_map();
        let cold = coldspots(&map, None);
        assert!(!cold.is_empty());
    }

    #[test]
    fn test_bus_factor() {
        let map = make_test_map();
        let bf = bus_factor(&map, false);
        // Alice has 3 commits, Bob has 1 - 80% of 4 = 3.2, so need Alice + Bob = 2
        assert_eq!(bf, 2);
    }

    #[test]
    fn test_ownership() {
        let map = make_test_map();
        let own = ownership(&map, "src/engine.rs");
        assert_eq!(own.primary, "alice");
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
    fn test_commit_shape() {
        let map = make_test_map();
        let shape = commit_shape(&map);
        assert!(!shape.typical_size.is_empty());
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
        // engine.rs and engine_test.rs should be coupled (3 cochanges)
        let test_coupling = coupled.iter().find(|c| c.path == "src/engine_test.rs");
        assert!(test_coupling.is_some());
        assert_eq!(test_coupling.unwrap().cochanges, 3);
    }
}
