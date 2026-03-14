use std::path::Path;

use anyhow::{Context, Result};

use analyzer_core::git;
use analyzer_core::types::{CommitDelta, CommitInfo, DeletionEntry, RenameEntry};

/// Extract full history from a repository.
pub fn extract_full(repo_path: &Path) -> Result<CommitDelta> {
    let repo = git::open_repo(repo_path)?;
    let head = git::get_head_sha(&repo)?;

    let mut commits: Vec<CommitInfo> = Vec::new();
    let mut renames: Vec<RenameEntry> = Vec::new();
    let mut deletions: Vec<DeletionEntry> = Vec::new();

    git::walk_commits(&repo, None, |commit| {
        // Detect renames and deletions from the commit
        if let Ok(oid) = git2::Oid::from_str(&commit.hash) {
            if let Ok(git_commit) = repo.find_commit(oid) {
                if let Ok(rename_list) = git::get_commit_renames(&repo, &git_commit) {
                    for (from, to) in rename_list {
                        renames.push(RenameEntry {
                            from,
                            to,
                            commit: commit.hash.clone(),
                            date: commit.date.clone(),
                        });
                    }
                }
                if let Ok(deletion_list) = git::get_commit_deletions(&repo, &git_commit) {
                    for path in deletion_list {
                        deletions.push(DeletionEntry {
                            path,
                            commit: commit.hash.clone(),
                            date: commit.date.clone(),
                        });
                    }
                }
            }
        }

        commits.push(commit);
    })
    .context("failed to walk commits")?;

    Ok(CommitDelta {
        head,
        commits,
        renames,
        deletions,
    })
}

/// Extract incremental delta since a given commit SHA.
pub fn extract_delta(repo_path: &Path, since_sha: &str) -> Result<CommitDelta> {
    let repo = git::open_repo(repo_path)?;
    let head = git::get_head_sha(&repo)?;

    let mut commits: Vec<CommitInfo> = Vec::new();
    let mut renames: Vec<RenameEntry> = Vec::new();
    let mut deletions: Vec<DeletionEntry> = Vec::new();

    git::walk_commits(&repo, Some(since_sha), |commit| {
        if let Ok(oid) = git2::Oid::from_str(&commit.hash) {
            if let Ok(git_commit) = repo.find_commit(oid) {
                if let Ok(rename_list) = git::get_commit_renames(&repo, &git_commit) {
                    for (from, to) in rename_list {
                        renames.push(RenameEntry {
                            from,
                            to,
                            commit: commit.hash.clone(),
                            date: commit.date.clone(),
                        });
                    }
                }
                if let Ok(deletion_list) = git::get_commit_deletions(&repo, &git_commit) {
                    for path in deletion_list {
                        deletions.push(DeletionEntry {
                            path,
                            commit: commit.hash.clone(),
                            date: commit.date.clone(),
                        });
                    }
                }
            }
        }

        commits.push(commit);
    })
    .context("failed to walk commits")?;

    Ok(CommitDelta {
        head,
        commits,
        renames,
        deletions,
    })
}
