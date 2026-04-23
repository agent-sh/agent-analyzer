//! Read-only queries against the graph data attached to a `RepoIntelData`.
//!
//! All queries assume `map.graph.cochange` is populated. They return `None`
//! (or empty results) when graph data is missing rather than panicking, so
//! the CLI layer can surface a clean "graph not built - run init" message.

use serde::Serialize;

use analyzer_core::types::RepoIntelData;

/// One discovered community with summary metrics.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityEntry {
    pub id: u32,
    pub size: usize,
    pub files: Vec<String>,
}

/// One boundary (high-betweenness) file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundaryEntry {
    pub path: String,
    pub betweenness: f64,
    pub community: Option<u32>,
}

/// Result of an `area-of <file>` lookup.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaOfResult {
    pub file: String,
    pub community: Option<u32>,
    pub size: Option<usize>,
}

/// Composite per-community health roll-up.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityHealth {
    pub id: u32,
    pub size: usize,
    pub total_changes: u64,
    pub recent_changes: u64,
    pub bug_fixes: u64,
    pub bug_fix_rate: f64,
    pub stale_owner_files: usize,
    pub files: Vec<String>,
}

/// List all discovered communities, sorted by size (largest first).
pub fn communities(map: &RepoIntelData) -> Vec<CommunityEntry> {
    let Some(g) = map.graph.as_ref().and_then(|g| g.cochange.as_ref()) else {
        return Vec::new();
    };
    let mut entries: Vec<CommunityEntry> = g
        .communities
        .iter()
        .map(|(id, files)| CommunityEntry {
            id: *id,
            size: files.len(),
            files: files.clone(),
        })
        .collect();
    entries.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
    entries
}

/// Return the top-N files by betweenness centrality (boundary files between
/// communities). These are the architectural seams - the highest-leverage
/// files for refactoring decisions.
pub fn boundaries(map: &RepoIntelData, top: usize) -> Vec<BoundaryEntry> {
    let Some(g) = map.graph.as_ref().and_then(|g| g.cochange.as_ref()) else {
        return Vec::new();
    };
    let mut entries: Vec<BoundaryEntry> = g
        .betweenness
        .iter()
        .filter(|&(_, score)| *score > 0.0)
        .map(|(path, &score)| BoundaryEntry {
            path: path.clone(),
            betweenness: score,
            community: g.file_to_community.get(path).copied(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.betweenness
            .partial_cmp(&a.betweenness)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.path.cmp(&b.path))
    });
    entries.truncate(top);
    entries
}

/// Look up which community a given file belongs to.
pub fn area_of(map: &RepoIntelData, file: &str) -> AreaOfResult {
    let Some(g) = map.graph.as_ref().and_then(|g| g.cochange.as_ref()) else {
        return AreaOfResult {
            file: file.to_string(),
            community: None,
            size: None,
        };
    };
    let community = g.file_to_community.get(file).copied();
    let size = community.and_then(|c| g.communities.get(&c).map(|v| v.len()));
    AreaOfResult {
        file: file.to_string(),
        community,
        size,
    }
}

/// Composite health roll-up for one community: aggregates per-file
/// activity into community-level signals (bug rate, stale owners).
/// Returns `None` if the community id is not present.
pub fn community_health(map: &RepoIntelData, id: u32) -> Option<CommunityHealth> {
    let g = map.graph.as_ref().and_then(|g| g.cochange.as_ref())?;
    let files = g.communities.get(&id)?;

    let mut total_changes: u64 = 0;
    let mut recent_changes: u64 = 0;
    let mut bug_fixes: u64 = 0;
    let mut stale_owner_files: usize = 0;

    let last_commit_date = map.git.last_commit_date.as_str();
    let cutoff = parse_cutoff(last_commit_date);

    for path in files {
        let Some(activity) = map.file_activity.get(path) else {
            continue;
        };
        total_changes += activity.changes;
        recent_changes += activity.recent_changes;
        bug_fixes += activity.bug_fix_changes;

        // A file's primary author is the first in `authors` (insertion order
        // matches commit order in the aggregator). Mark stale if the author's
        // last_seen is past the 90-day cutoff.
        if let (Some(author), Some(c)) = (activity.authors.first(), cutoff.as_ref()) {
            if let Some(humans) = map.contributors.humans.get(author) {
                if humans.last_seen.as_str() < c.as_str() {
                    stale_owner_files += 1;
                }
            }
        }
    }

    let bug_fix_rate = if total_changes > 0 {
        bug_fixes as f64 / total_changes as f64
    } else {
        0.0
    };

    // `files` is already sorted at construction time (cochange::build sorts
    // each community's file list before returning), so just clone.
    let sorted_files = files.clone();

    Some(CommunityHealth {
        id,
        size: files.len(),
        total_changes,
        recent_changes,
        bug_fixes,
        bug_fix_rate,
        stale_owner_files,
        files: sorted_files,
    })
}

/// 90-day cutoff string relative to `last_commit_date`. Returns `None` for
/// repos with no parseable last-commit date.
fn parse_cutoff(last_commit_date: &str) -> Option<String> {
    use chrono::DateTime;
    let last = DateTime::parse_from_rfc3339(last_commit_date).ok()?;
    Some((last - chrono::Duration::days(90)).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cochange;
    use analyzer_core::types::{CouplingEntry, FileActivity, GitInfo};
    use chrono::Utc;
    use std::collections::HashMap;

    fn two_triangle_map() -> RepoIntelData {
        let files = [
            ("a.rs", 6_u64),
            ("b.rs", 6),
            ("c.rs", 6),
            ("d.rs", 6),
            ("e.rs", 6),
            ("f.rs", 6),
        ];
        let coupling = [
            ("a.rs", "b.rs", 5_u64),
            ("a.rs", "c.rs", 5),
            ("b.rs", "c.rs", 5),
            ("d.rs", "e.rs", 5),
            ("d.rs", "f.rs", 5),
            ("e.rs", "f.rs", 5),
            ("a.rs", "d.rs", 3),
        ];
        let now = Utc::now();
        let mut data = RepoIntelData {
            version: "1.0".into(),
            generated: now,
            updated: now,
            partial: false,
            git: GitInfo {
                analyzed_up_to: String::new(),
                total_commits_analyzed: 0,
                first_commit_date: String::new(),
                last_commit_date: String::new(),
                scope: None,
                shallow: false,
            },
            contributors: analyzer_core::types::Contributors {
                humans: HashMap::new(),
                bots: HashMap::new(),
            },
            file_activity: HashMap::new(),
            coupling: HashMap::new(),
            conventions: analyzer_core::types::ConventionInfo {
                prefixes: HashMap::new(),
                style: "unknown".into(),
                uses_scopes: false,
                naming_patterns: None,
                test_patterns: None,
            },
            releases: analyzer_core::types::Releases {
                tags: vec![],
                cadence: "unknown".into(),
            },
            renames: vec![],
            deletions: vec![],
            symbols: None,
            import_graph: None,
            project: None,
            doc_refs: None,
            graph: None,
        };

        for (path, changes) in files {
            data.file_activity.insert(
                path.to_string(),
                FileActivity {
                    changes,
                    recent_changes: 0,
                    authors: vec![],
                    created: String::new(),
                    last_changed: String::new(),
                    additions: 0,
                    deletions: 0,
                    bug_fix_changes: 0,
                    refactor_changes: 0,
                    last_bug_fix: String::new(),
                    generated: false,
                },
            );
        }
        for (a, b, cochanges) in coupling {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            data.coupling
                .entry(lo.to_string())
                .or_default()
                .insert(hi.to_string(), CouplingEntry { cochanges });
        }
        let g = cochange::build(&data).expect("graph builds");
        data.graph = Some(analyzer_core::types::GraphData {
            cochange: Some(g),
            ..Default::default()
        });
        data
    }

    #[test]
    fn communities_lists_two() {
        let m = two_triangle_map();
        let c = communities(&m);
        assert_eq!(c.len(), 2, "expected exactly two communities");
        assert!(c.iter().all(|e| e.size == 3));
    }

    #[test]
    fn area_of_returns_correct_community() {
        let m = two_triangle_map();
        let r = area_of(&m, "a.rs");
        assert!(r.community.is_some());
        assert_eq!(r.size, Some(3));
    }

    #[test]
    fn area_of_unknown_file_returns_none() {
        let m = two_triangle_map();
        let r = area_of(&m, "not-in-graph.rs");
        assert!(r.community.is_none());
    }

    #[test]
    fn boundaries_surface_bridge_endpoints() {
        let m = two_triangle_map();
        let b = boundaries(&m, 5);
        let paths: Vec<&str> = b.iter().map(|e| e.path.as_str()).collect();
        // a.rs and d.rs are the bridge endpoints - expect them both present.
        assert!(paths.contains(&"a.rs"), "boundaries should include a.rs");
        assert!(paths.contains(&"d.rs"), "boundaries should include d.rs");
    }

    #[test]
    fn community_health_returns_size() {
        let m = two_triangle_map();
        let any_id = *m
            .graph
            .as_ref()
            .unwrap()
            .cochange
            .as_ref()
            .unwrap()
            .communities
            .keys()
            .next()
            .unwrap();
        let h = community_health(&m, any_id).expect("health for known id");
        assert_eq!(h.size, 3);
    }

    #[test]
    fn community_health_unknown_id_returns_none() {
        let m = two_triangle_map();
        assert!(community_health(&m, 9999).is_none());
    }
}
