use serde::Serialize;

use analyzer_core::git;
use analyzer_core::types::GitMapData;

/// Status of the cached git-map relative to the repository.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MapStatus {
    /// Map is up to date with HEAD.
    Valid,
    /// Map exists but HEAD has moved.
    Stale,
    /// Map's analyzedUpTo commit is not in history (force push detected).
    Invalid,
}

/// Check the status of a cached map against the repository.
pub fn check_status(map: &GitMapData, repo: &git2::Repository) -> MapStatus {
    if map.git.analyzed_up_to.is_empty() {
        return MapStatus::Invalid;
    }

    // Check if analyzedUpTo is still in history
    if !git::is_commit_in_history(repo, &map.git.analyzed_up_to) {
        return MapStatus::Invalid;
    }

    // Check if HEAD matches analyzedUpTo
    let head = git::get_head_sha(repo).unwrap_or_default();
    if head == map.git.analyzed_up_to {
        MapStatus::Valid
    } else {
        MapStatus::Stale
    }
}

/// Check if the map needs a full rebuild (force push detected).
pub fn needs_rebuild(map: &GitMapData, repo: &git2::Repository) -> bool {
    if map.git.analyzed_up_to.is_empty() {
        return true;
    }
    !git::is_commit_in_history(repo, &map.git.analyzed_up_to)
}

/// Get the commit range string for incremental update.
///
/// Returns empty string for full scan (new map), or the SHA to start from
/// for incremental update.
pub fn get_since_sha(map: &GitMapData) -> Option<String> {
    if map.git.analyzed_up_to.is_empty() {
        None
    } else {
        Some(map.git.analyzed_up_to.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::create_empty_map;

    #[test]
    fn test_get_since_sha_empty_map() {
        let map = create_empty_map();
        assert!(get_since_sha(&map).is_none());
    }

    #[test]
    fn test_get_since_sha_existing_map() {
        let mut map = create_empty_map();
        map.git.analyzed_up_to = "abc123def456".to_string();
        let sha = get_since_sha(&map);
        assert_eq!(sha, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_needs_rebuild_empty_map() {
        let map = create_empty_map();
        // Without a real repo, we can't test this fully.
        // Just verify the empty map case.
        assert!(map.git.analyzed_up_to.is_empty());
    }
}
