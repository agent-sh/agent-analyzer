use std::collections::HashMap;

use chrono::Utc;

use analyzer_core::ai_detect::{detect_ai, is_bot};
use analyzer_core::types::{
    AiAttribution, CommitDelta, CommitShape, CommitSize, Contributors, ConventionInfo, DirActivity,
    FileActivity, FilesPerCommit, GitInfo, GitMapData, Releases, SizeDistribution,
    extract_conventional_prefix,
};
use analyzer_core::walk::is_noise;

/// Create an empty git-map data structure.
pub fn create_empty_map() -> GitMapData {
    let now = Utc::now();
    GitMapData {
        version: "1.0".to_string(),
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
        contributors: Contributors {
            humans: HashMap::new(),
            bots: HashMap::new(),
        },
        file_activity: HashMap::new(),
        dir_activity: HashMap::new(),
        coupling: HashMap::new(),
        commit_shape: CommitShape {
            size_distribution: SizeDistribution {
                tiny: 0,
                small: 0,
                medium: 0,
                large: 0,
                huge: 0,
            },
            files_per_commit: FilesPerCommit { median: 0, p90: 0 },
            merge_commits: 0,
        },
        conventions: ConventionInfo {
            prefixes: HashMap::new(),
            style: "unknown".to_string(),
            uses_scopes: false,
            samples: vec![],
        },
        ai_attribution: AiAttribution {
            attributed: 0,
            heuristic: 0,
            none: 0,
            tools: HashMap::new(),
        },
        releases: Releases {
            tags: vec![],
            cadence: "unknown".to_string(),
        },
        renames: vec![],
        deletions: vec![],
    }
}

/// Merge a commit delta into an existing git-map.
///
/// Implements the merge algorithm from the git-map spec section 5.
pub fn merge_delta(map: &mut GitMapData, delta: &CommitDelta) {
    let mut files_per_commit_values: Vec<u64> = Vec::new();

    for commit in &delta.commits {
        // Determine if author is a bot
        let is_bot_author = is_bot(&commit.author_name);
        let ai_signal = detect_ai(commit);

        // Update contributor counts
        if is_bot_author {
            let entry = map
                .contributors
                .bots
                .entry(commit.author_name.clone())
                .or_insert_with(|| analyzer_core::types::BotContributor {
                    commits: 0,
                    first_seen: commit.date.clone(),
                    last_seen: commit.date.clone(),
                });
            entry.commits += 1;
            entry.last_seen.clone_from(&commit.date);
        } else {
            let entry = map
                .contributors
                .humans
                .entry(commit.author_name.clone())
                .or_insert_with(|| analyzer_core::types::HumanContributor {
                    commits: 0,
                    first_seen: commit.date.clone(),
                    last_seen: commit.date.clone(),
                    ai_assisted_commits: 0,
                });
            entry.commits += 1;
            entry.last_seen.clone_from(&commit.date);

            if ai_signal.detected {
                entry.ai_assisted_commits += 1;
            }
        }

        // Update AI attribution counts
        if ai_signal.detected {
            map.ai_attribution.attributed += 1;
            let tool = ai_signal.tool.unwrap_or_else(|| "unknown".to_string());
            *map.ai_attribution.tools.entry(tool).or_insert(0) += 1;
        } else {
            map.ai_attribution.none += 1;
        }

        // Update commit shape - size distribution
        let total_lines: u64 = commit.files.iter().map(|f| f.additions + f.deletions).sum();
        let size = CommitSize::classify(total_lines);
        match size {
            CommitSize::Tiny => map.commit_shape.size_distribution.tiny += 1,
            CommitSize::Small => map.commit_shape.size_distribution.small += 1,
            CommitSize::Medium => map.commit_shape.size_distribution.medium += 1,
            CommitSize::Large => map.commit_shape.size_distribution.large += 1,
            CommitSize::Huge => map.commit_shape.size_distribution.huge += 1,
        }

        // Track files per commit for median/p90 calculation
        files_per_commit_values.push(commit.files.len() as u64);

        // Update conventions
        let prefix = extract_conventional_prefix(&commit.subject);
        if let Some(ref p) = prefix {
            *map.conventions.prefixes.entry(p.clone()).or_insert(0) += 1;
        }

        // Check for scope usage
        if commit.subject.contains('(') && commit.subject.contains(')') {
            map.conventions.uses_scopes = true;
        }

        // Collect sample commit messages (keep up to 5)
        if map.conventions.samples.len() < 5 {
            map.conventions.samples.push(commit.subject.clone());
        }

        // Update per-file activity
        let non_noise_files: Vec<&analyzer_core::types::FileChange> =
            commit.files.iter().filter(|f| !is_noise(&f.path)).collect();

        for file in &commit.files {
            if is_noise(&file.path) {
                continue;
            }

            let entry = map
                .file_activity
                .entry(file.path.clone())
                .or_insert_with(|| FileActivity {
                    changes: 0,
                    authors: vec![],
                    created: commit.date.clone(),
                    last_changed: commit.date.clone(),
                    additions: 0,
                    deletions: 0,
                    ai_changes: 0,
                    ai_additions: 0,
                    ai_deletions: 0,
                    bug_fix_changes: 0,
                });

            entry.changes += 1;
            entry.last_changed.clone_from(&commit.date);
            entry.additions += file.additions;
            entry.deletions += file.deletions;

            if !entry.authors.contains(&commit.author_name) {
                entry.authors.push(commit.author_name.clone());
            }

            if ai_signal.detected {
                entry.ai_changes += 1;
                entry.ai_additions += file.additions;
                entry.ai_deletions += file.deletions;
            }

            if prefix.as_deref() == Some("fix") {
                entry.bug_fix_changes += 1;
            }
        }

        // Update coupling (co-occurrence within same commit)
        let file_paths: Vec<&str> = non_noise_files.iter().map(|f| f.path.as_str()).collect();
        for i in 0..file_paths.len() {
            for j in (i + 1)..file_paths.len() {
                let (a, b) = if file_paths[i] < file_paths[j] {
                    (file_paths[i], file_paths[j])
                } else {
                    (file_paths[j], file_paths[i])
                };

                let pairs = map.coupling.entry(a.to_string()).or_default();
                let entry = pairs.entry(b.to_string()).or_insert_with(|| {
                    analyzer_core::types::CouplingEntry {
                        cochanges: 0,
                        human_cochanges: 0,
                        ai_cochanges: 0,
                    }
                });
                entry.cochanges += 1;
                if ai_signal.detected {
                    entry.ai_cochanges += 1;
                } else {
                    entry.human_cochanges += 1;
                }
            }
        }

        // Update directory activity
        for file in &commit.files {
            let dir = file_dir(&file.path);
            let entry = map.dir_activity.entry(dir).or_insert_with(|| DirActivity {
                changes: 0,
                authors: vec![],
                ai_changes: 0,
            });
            entry.changes += 1;
            if !entry.authors.contains(&commit.author_name) {
                entry.authors.push(commit.author_name.clone());
            }
            if ai_signal.detected {
                entry.ai_changes += 1;
            }
        }

        // Update first/last commit dates
        if map.git.first_commit_date.is_empty() || commit.date < map.git.first_commit_date {
            map.git.first_commit_date.clone_from(&commit.date);
        }
        if map.git.last_commit_date.is_empty() || commit.date > map.git.last_commit_date {
            map.git.last_commit_date.clone_from(&commit.date);
        }
    }

    // Merge renames and deletions
    map.renames.extend(delta.renames.iter().cloned());
    map.deletions.extend(delta.deletions.iter().cloned());

    // Prune low-signal coupling (below threshold of 3 cochanges)
    let coupling_keys: Vec<String> = map.coupling.keys().cloned().collect();
    for file_a in coupling_keys {
        let pairs = map.coupling.get_mut(&file_a).unwrap();
        pairs.retain(|_, counts| counts.cochanges >= 3);
        if pairs.is_empty() {
            map.coupling.remove(&file_a);
        }
    }

    // Update git metadata
    map.git.analyzed_up_to.clone_from(&delta.head);
    map.git.total_commits_analyzed += delta.commits.len() as u64;

    // Update files per commit stats
    if !files_per_commit_values.is_empty() {
        files_per_commit_values.sort();
        let len = files_per_commit_values.len();
        map.commit_shape.files_per_commit.median = files_per_commit_values[len / 2];
        let p90_idx = (len as f64 * 0.9) as usize;
        map.commit_shape.files_per_commit.p90 =
            files_per_commit_values[p90_idx.min(len.saturating_sub(1))];
    }

    // Detect convention style
    let total_prefixed: u64 = map.conventions.prefixes.values().sum();
    let total_commits = map.git.total_commits_analyzed;
    if total_commits > 0 && total_prefixed > 0 {
        let ratio = total_prefixed as f64 / total_commits as f64;
        map.conventions.style = if ratio > 0.5 {
            "conventional".to_string()
        } else {
            "mixed".to_string()
        };
    }

    map.updated = Utc::now();
}

/// Extract the directory from a file path (with trailing slash).
fn file_dir(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(pos) = normalized.rfind('/') {
        format!("{}/", &normalized[..pos])
    } else {
        "./".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{CommitDelta, CommitInfo, FileChange};

    fn make_delta(commits: Vec<CommitInfo>) -> CommitDelta {
        CommitDelta {
            head: "abc123".to_string(),
            commits,
            renames: vec![],
            deletions: vec![],
        }
    }

    fn make_commit(author: &str, subject: &str, files: Vec<FileChange>) -> CommitInfo {
        CommitInfo {
            hash: "abc123".to_string(),
            author_name: author.to_string(),
            author_email: format!("{author}@example.com"),
            date: "2026-03-14T10:00:00Z".to_string(),
            subject: subject.to_string(),
            body: String::new(),
            trailers: vec![],
            files,
        }
    }

    #[test]
    fn test_create_empty_map() {
        let map = create_empty_map();
        assert_eq!(map.version, "1.0");
        assert_eq!(map.git.total_commits_analyzed, 0);
        assert!(map.contributors.humans.is_empty());
        assert!(map.contributors.bots.is_empty());
    }

    #[test]
    fn test_merge_delta_basic() {
        let mut map = create_empty_map();
        let delta = make_delta(vec![make_commit(
            "alice",
            "feat: add feature",
            vec![FileChange {
                path: "src/main.rs".to_string(),
                additions: 10,
                deletions: 2,
            }],
        )]);

        merge_delta(&mut map, &delta);

        assert_eq!(map.git.total_commits_analyzed, 1);
        assert_eq!(map.contributors.humans.len(), 1);
        assert!(map.contributors.humans.contains_key("alice"));
        assert_eq!(map.contributors.humans["alice"].commits, 1);
        assert!(map.file_activity.contains_key("src/main.rs"));
        assert_eq!(map.file_activity["src/main.rs"].changes, 1);
        assert_eq!(map.file_activity["src/main.rs"].additions, 10);
    }

    #[test]
    fn test_merge_delta_bot_contributor() {
        let mut map = create_empty_map();
        let delta = make_delta(vec![make_commit(
            "dependabot[bot]",
            "chore: bump deps",
            vec![FileChange {
                path: "src/lib.rs".to_string(),
                additions: 1,
                deletions: 1,
            }],
        )]);

        merge_delta(&mut map, &delta);

        assert!(map.contributors.humans.is_empty());
        assert_eq!(map.contributors.bots.len(), 1);
        assert!(map.contributors.bots.contains_key("dependabot[bot]"));
    }

    #[test]
    fn test_merge_delta_conventional_prefixes() {
        let mut map = create_empty_map();
        let delta = make_delta(vec![
            make_commit("alice", "feat: add feature", vec![]),
            make_commit("alice", "fix: handle error", vec![]),
            make_commit("alice", "feat: another feature", vec![]),
        ]);

        merge_delta(&mut map, &delta);

        assert_eq!(map.conventions.prefixes["feat"], 2);
        assert_eq!(map.conventions.prefixes["fix"], 1);
    }

    #[test]
    fn test_merge_delta_noise_filtering() {
        let mut map = create_empty_map();
        let delta = make_delta(vec![make_commit(
            "alice",
            "chore: update deps",
            vec![
                FileChange {
                    path: "package-lock.json".to_string(),
                    additions: 1000,
                    deletions: 500,
                },
                FileChange {
                    path: "src/app.js".to_string(),
                    additions: 5,
                    deletions: 2,
                },
            ],
        )]);

        merge_delta(&mut map, &delta);

        // Noise files should not appear in file_activity
        assert!(!map.file_activity.contains_key("package-lock.json"));
        assert!(map.file_activity.contains_key("src/app.js"));
    }

    #[test]
    fn test_file_dir() {
        assert_eq!(file_dir("src/core/engine.rs"), "src/core/");
        assert_eq!(file_dir("README.md"), "./");
        assert_eq!(file_dir("src\\core\\engine.rs"), "src/core/");
    }
}
