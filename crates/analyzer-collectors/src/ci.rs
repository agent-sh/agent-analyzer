//! CI provider detection from config files.

use std::path::Path;

use analyzer_core::types::CiInfo;

struct CiProvider {
    name: &'static str,
    paths: &'static [&'static str],
}

static CI_PROVIDERS: &[CiProvider] = &[
    CiProvider {
        name: "github-actions",
        paths: &[".github/workflows"],
    },
    CiProvider {
        name: "circleci",
        paths: &[".circleci/config.yml", ".circleci/config.yaml"],
    },
    CiProvider {
        name: "gitlab-ci",
        paths: &[".gitlab-ci.yml"],
    },
    CiProvider {
        name: "travis-ci",
        paths: &[".travis.yml"],
    },
    CiProvider {
        name: "jenkins",
        paths: &["Jenkinsfile"],
    },
    CiProvider {
        name: "azure-pipelines",
        paths: &["azure-pipelines.yml"],
    },
    CiProvider {
        name: "bitbucket-pipelines",
        paths: &["bitbucket-pipelines.yml"],
    },
];

/// Detect CI provider from config files in the repo.
pub fn detect_ci(repo_path: &Path) -> Option<CiInfo> {
    for provider in CI_PROVIDERS {
        let mut config_files = Vec::new();

        for path in provider.paths {
            let full_path = repo_path.join(path);
            if full_path.is_dir() {
                // For directories (like .github/workflows), list YAML files
                if let Ok(entries) = std::fs::read_dir(&full_path) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".yml") || name.ends_with(".yaml") {
                            config_files.push(format!("{}/{}", path, name));
                        }
                    }
                }
            } else if full_path.exists() {
                config_files.push(path.to_string());
            }
        }

        if !config_files.is_empty() {
            return Some(CiInfo {
                provider: provider.name.to_string(),
                config_files,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_github_actions() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("ci.yml"), "name: CI").unwrap();
        std::fs::write(workflows.join("release.yml"), "name: Release").unwrap();

        let ci = detect_ci(dir.path()).unwrap();
        assert_eq!(ci.provider, "github-actions");
        assert_eq!(ci.config_files.len(), 2);
    }

    #[test]
    fn test_detect_gitlab_ci() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitlab-ci.yml"), "stages: [build]").unwrap();

        let ci = detect_ci(dir.path()).unwrap();
        assert_eq!(ci.provider, "gitlab-ci");
    }

    #[test]
    fn test_no_ci() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_ci(dir.path()).is_none());
    }
}
