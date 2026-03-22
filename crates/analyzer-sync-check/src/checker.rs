//! Staleness detection - annotate code refs with issues.

use std::collections::HashSet;

use analyzer_core::types::{CodeRef, DeletionEntry, RenameEntry};

/// Check matched code refs for staleness issues.
/// Annotates each CodeRef with an issue if problems are found.
pub fn check_staleness(
    refs: &mut [CodeRef],
    renames: &[RenameEntry],
    deletions: &[DeletionEntry],
    hotspot_files: &HashSet<String>,
) {
    // Build lookup sets for deleted and renamed files/symbols
    let deleted_paths: HashSet<&str> = deletions.iter().map(|d| d.path.as_str()).collect();
    let rename_map: std::collections::HashMap<&str, &str> = renames
        .iter()
        .map(|r| (r.from.as_str(), r.to.as_str()))
        .collect();

    for code_ref in refs.iter_mut() {
        if !code_ref.exists {
            // Symbol not found - check if it was deleted or renamed
            if let Some(file) = &code_ref.file {
                if deleted_paths.contains(file.as_str()) {
                    code_ref.issue = Some("symbol-deleted".to_string());
                    continue;
                }
                if rename_map.contains_key(file.as_str()) {
                    code_ref.issue = Some("symbol-renamed".to_string());
                    continue;
                }
            }
            code_ref.issue = Some("symbol-not-found".to_string());
        } else if let Some(file) = &code_ref.file {
            // Symbol exists - check if it references a hotspot
            if hotspot_files.contains(file.as_str()) {
                code_ref.issue = Some("references-hotspot".to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_not_found() {
        let mut refs = vec![CodeRef {
            text: "old_func()".to_string(),
            symbol: "old_func".to_string(),
            file: None,
            exists: false,
            line: Some(5),
            issue: None,
        }];
        check_staleness(&mut refs, &[], &[], &HashSet::new());
        assert_eq!(refs[0].issue, Some("symbol-not-found".to_string()));
    }

    #[test]
    fn test_references_hotspot() {
        let mut hotspots = HashSet::new();
        hotspots.insert("src/core.rs".to_string());

        let mut refs = vec![CodeRef {
            text: "validate()".to_string(),
            symbol: "validate".to_string(),
            file: Some("src/core.rs".to_string()),
            exists: true,
            line: Some(5),
            issue: None,
        }];
        check_staleness(&mut refs, &[], &[], &hotspots);
        assert_eq!(refs[0].issue, Some("references-hotspot".to_string()));
    }

    #[test]
    fn test_no_issue() {
        let mut refs = vec![CodeRef {
            text: "validate()".to_string(),
            symbol: "validate".to_string(),
            file: Some("src/core.rs".to_string()),
            exists: true,
            line: Some(5),
            issue: None,
        }];
        check_staleness(&mut refs, &[], &[], &HashSet::new());
        assert!(refs[0].issue.is_none());
    }
}
