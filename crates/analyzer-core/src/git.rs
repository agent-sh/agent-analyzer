use std::path::Path;

use anyhow::{Context, Result};
use git2::{DiffOptions, Oid, Repository, Sort};

use crate::types::{CommitInfo, FileChange};

/// Open a git repository at the given path.
pub fn open_repo(path: &Path) -> Result<Repository> {
    Repository::discover(path)
        .with_context(|| format!("failed to open git repository at {}", path.display()))
}

/// Get the HEAD commit SHA.
pub fn get_head_sha(repo: &Repository) -> Result<String> {
    let head = repo.head().context("failed to get HEAD reference")?;
    let commit = head.peel_to_commit().context("HEAD is not a commit")?;
    Ok(commit.id().to_string())
}

/// Check if the repository is a shallow clone.
pub fn is_shallow(repo: &Repository) -> bool {
    repo.is_shallow()
}

/// Check if a commit SHA exists in the repository history.
pub fn is_commit_in_history(repo: &Repository, sha: &str) -> bool {
    let Ok(oid) = Oid::from_str(sha) else {
        return false;
    };
    repo.find_commit(oid).is_ok()
}

/// Walk commits in the given range (or full history if range is empty).
///
/// Calls `callback` for each non-merge commit in reverse chronological order.
/// If `since_sha` is `Some`, only walks commits after that SHA (exclusive).
/// Skips merge commits (commits with more than one parent).
pub fn walk_commits<F>(repo: &Repository, since_sha: Option<&str>, mut callback: F) -> Result<()>
where
    F: FnMut(CommitInfo),
{
    let mut revwalk = repo.revwalk().context("failed to create revwalk")?;
    revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
    revwalk
        .push_head()
        .context("failed to push HEAD to revwalk")?;

    if let Some(sha) = since_sha {
        let oid = Oid::from_str(sha).with_context(|| format!("invalid SHA: {sha}"))?;
        revwalk
            .hide(oid)
            .with_context(|| format!("failed to hide commit {sha}"))?;
    }

    for oid_result in revwalk {
        let oid = oid_result.context("revwalk iteration failed")?;
        let commit = repo
            .find_commit(oid)
            .with_context(|| format!("failed to find commit {oid}"))?;

        // Skip merge commits
        if commit.parent_count() > 1 {
            continue;
        }

        let author = commit.author();
        let author_name = author.name().unwrap_or("unknown").to_string();
        let author_email = author.email().unwrap_or("unknown").to_string();

        let time = commit.time();
        let offset = chrono::FixedOffset::east_opt(time.offset_minutes() * 60)
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap());
        let datetime = chrono::DateTime::from_timestamp(time.seconds(), 0)
            .unwrap_or_default()
            .with_timezone(&offset);
        let date = datetime.to_rfc3339();

        let message = commit.message().unwrap_or("");
        let (subject, body, trailers) = parse_commit_message(message);

        let files = get_commit_diff_stats(repo, &commit).unwrap_or_default();

        let info = CommitInfo {
            hash: oid.to_string(),
            author_name,
            author_email,
            date,
            subject,
            body,
            trailers,
            files,
        };

        callback(info);
    }

    Ok(())
}

/// Parse a commit message into subject, body, and trailers.
fn parse_commit_message(message: &str) -> (String, String, Vec<String>) {
    let lines: Vec<&str> = message.lines().collect();

    let subject = lines.first().map(|s| s.to_string()).unwrap_or_default();

    let mut body_lines = Vec::new();
    let mut trailers = Vec::new();
    let mut in_body = false;

    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            continue;
        }
        if i == 1 && line.is_empty() {
            in_body = true;
            continue;
        }
        if in_body || i > 1 {
            in_body = true;
            // Check if this line looks like a trailer (Key: Value or Key-Name: Value)
            if line.contains(": ") && !line.starts_with(' ') && !line.starts_with('\t') {
                let parts: Vec<&str> = line.splitn(2, ": ").collect();
                if parts.len() == 2 {
                    let key = parts[0].trim();
                    // Trailer keys are typically capitalized words with optional hyphens
                    if !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '-' || c == ' ')
                    {
                        trailers.push(line.to_string());
                        continue;
                    }
                }
            }
            body_lines.push(*line);
        }
    }

    let body = body_lines.join("\n").trim().to_string();

    (subject, body, trailers)
}

/// Get diff stats (additions/deletions per file) for a commit.
pub fn get_commit_diff_stats(
    repo: &Repository,
    commit: &git2::Commit<'_>,
) -> Result<Vec<FileChange>> {
    let tree = commit.tree().context("failed to get commit tree")?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)?
                .tree()
                .context("failed to get parent tree")?,
        )
    } else {
        None
    };

    let mut opts = DiffOptions::new();
    opts.include_untracked(false);

    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
        .context("failed to compute diff")?;

    let stats = diff.stats().context("failed to get diff stats")?;
    let _ = stats; // We need per-file stats, not aggregate

    let mut files = Vec::new();

    // Use diff deltas to get per-file stats
    for i in 0..diff.deltas().len() {
        let delta = diff.get_delta(i).unwrap();
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        if path.is_empty() {
            continue;
        }

        // Count additions and deletions by iterating hunks
        let mut additions: u64 = 0;
        let mut deletions: u64 = 0;

        let patch = git2::Patch::from_diff(&diff, i).ok().flatten();
        if let Some(patch) = patch {
            let (_, adds, dels) = patch.line_stats().unwrap_or((0, 0, 0));
            additions = adds as u64;
            deletions = dels as u64;
        }

        files.push(FileChange {
            path,
            additions,
            deletions,
        });
    }

    Ok(files)
}

/// Get renames detected in a commit's diff.
pub fn get_commit_renames(
    repo: &Repository,
    commit: &git2::Commit<'_>,
) -> Result<Vec<(String, String)>> {
    let tree = commit.tree().context("failed to get commit tree")?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)?
                .tree()
                .context("failed to get parent tree")?,
        )
    } else {
        return Ok(vec![]);
    };

    let mut opts = DiffOptions::new();
    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
        .context("failed to compute diff")?;

    let mut find_opts = git2::DiffFindOptions::new();
    find_opts.renames(true);
    let mut diff = diff;
    diff.find_similar(Some(&mut find_opts))
        .context("failed to find renames")?;

    let mut renames = Vec::new();
    for i in 0..diff.deltas().len() {
        let delta = diff.get_delta(i).unwrap();
        if delta.status() == git2::Delta::Renamed {
            let old_path = delta
                .old_file()
                .path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let new_path = delta
                .new_file()
                .path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if !old_path.is_empty() && !new_path.is_empty() {
                renames.push((old_path, new_path));
            }
        }
    }

    Ok(renames)
}

/// Get files deleted in a commit's diff.
pub fn get_commit_deletions(repo: &Repository, commit: &git2::Commit<'_>) -> Result<Vec<String>> {
    let tree = commit.tree().context("failed to get commit tree")?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)?
                .tree()
                .context("failed to get parent tree")?,
        )
    } else {
        return Ok(vec![]);
    };

    let mut opts = DiffOptions::new();
    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
        .context("failed to compute diff")?;

    let mut deleted = Vec::new();
    for i in 0..diff.deltas().len() {
        let delta = diff.get_delta(i).unwrap();
        if delta.status() == git2::Delta::Deleted {
            if let Some(path) = delta.old_file().path() {
                deleted.push(path.to_string_lossy().to_string());
            }
        }
    }

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_commit_message_simple() {
        let (subject, body, trailers) = parse_commit_message("feat: add feature");
        assert_eq!(subject, "feat: add feature");
        assert!(body.is_empty());
        assert!(trailers.is_empty());
    }

    #[test]
    fn test_parse_commit_message_with_body() {
        let msg = "feat: add feature\n\nThis is the body.\nMore details here.";
        let (subject, body, trailers) = parse_commit_message(msg);
        assert_eq!(subject, "feat: add feature");
        assert_eq!(body, "This is the body.\nMore details here.");
        assert!(trailers.is_empty());
    }

    #[test]
    fn test_parse_commit_message_with_trailers() {
        let msg =
            "feat: add feature\n\nSome body\n\nCo-Authored-By: Claude <noreply@anthropic.com>";
        let (subject, body, trailers) = parse_commit_message(msg);
        assert_eq!(subject, "feat: add feature");
        assert!(body.contains("Some body"));
        assert_eq!(trailers.len(), 1);
        assert!(trailers[0].contains("noreply@anthropic.com"));
    }
}
