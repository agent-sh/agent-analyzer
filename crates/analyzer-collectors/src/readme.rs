//! README file detection and parsing.

use std::path::Path;

use analyzer_core::types::ReadmeInfo;

static README_NAMES: &[&str] = &[
    "README.md",
    "README.rst",
    "README.txt",
    "README",
    "readme.md",
    "Readme.md",
];

/// Find and parse README file, extracting headings.
pub fn collect_readme(repo_path: &Path) -> Option<ReadmeInfo> {
    for name in README_NAMES {
        let path = repo_path.join(name);
        if path.exists() {
            let content = std::fs::read_to_string(&path).ok()?;
            let sections = extract_headings(&content, name);
            return Some(ReadmeInfo {
                exists: true,
                path: name.to_string(),
                sections,
            });
        }
    }
    None
}

fn extract_headings(content: &str, filename: &str) -> Vec<String> {
    if filename.ends_with(".md") {
        // Markdown: lines starting with #
        content
            .lines()
            .filter(|line| line.starts_with('#'))
            .map(|line| line.trim_start_matches('#').trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if filename.ends_with(".rst") {
        // RST: lines followed by ===, ---, ~~~ underlines
        let lines: Vec<&str> = content.lines().collect();
        let mut headings = Vec::new();
        for i in 0..lines.len().saturating_sub(1) {
            let next = lines[i + 1];
            if !lines[i].is_empty()
                && next.len() >= lines[i].len()
                && (next.chars().all(|c| c == '=')
                    || next.chars().all(|c| c == '-')
                    || next.chars().all(|c| c == '~'))
            {
                headings.push(lines[i].trim().to_string());
            }
        }
        headings
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_markdown_headings() {
        let content = "# Title\n\nSome text\n\n## Section 1\n\n### Subsection\n\n## Section 2\n";
        let headings = extract_headings(content, "README.md");
        assert_eq!(
            headings,
            vec!["Title", "Section 1", "Subsection", "Section 2"]
        );
    }

    #[test]
    fn test_collect_readme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "# My Project\n\n## Install\n\n## Usage\n",
        )
        .unwrap();
        let info = collect_readme(dir.path()).unwrap();
        assert!(info.exists);
        assert_eq!(info.path, "README.md");
        assert_eq!(info.sections, vec!["My Project", "Install", "Usage"]);
    }

    #[test]
    fn test_no_readme() {
        let dir = tempfile::tempdir().unwrap();
        assert!(collect_readme(dir.path()).is_none());
    }
}
