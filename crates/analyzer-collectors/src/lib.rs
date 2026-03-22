//! Phase 3 - Project data gathering.
//!
//! Collects project metadata: README, CI config, license, languages.

mod ci;
mod languages;
mod license;
mod readme;

use std::path::Path;

use anyhow::Result;

use analyzer_core::types::ProjectMetadata;

/// Collect all project metadata from a repository path.
pub fn collect_metadata(repo_path: &Path) -> Result<ProjectMetadata> {
    let readme = readme::collect_readme(repo_path);
    let license = license::detect_license(repo_path);
    let ci = ci::detect_ci(repo_path);
    let package_manager = detect_package_manager(repo_path);
    let languages = languages::detect_languages(repo_path);

    Ok(ProjectMetadata {
        readme,
        license,
        ci,
        package_manager,
        languages,
    })
}

fn detect_package_manager(repo_path: &Path) -> Option<String> {
    if repo_path.join("Cargo.toml").exists() {
        Some("cargo".to_string())
    } else if repo_path.join("package-lock.json").exists() {
        Some("npm".to_string())
    } else if repo_path.join("yarn.lock").exists() {
        Some("yarn".to_string())
    } else if repo_path.join("pnpm-lock.yaml").exists() {
        Some("pnpm".to_string())
    } else if repo_path.join("go.mod").exists() {
        Some("go".to_string())
    } else if repo_path.join("requirements.txt").exists()
        || repo_path.join("pyproject.toml").exists()
        || repo_path.join("setup.py").exists()
    {
        Some("pip".to_string())
    } else if repo_path.join("pom.xml").exists() {
        Some("maven".to_string())
    } else if repo_path.join("build.gradle").exists()
        || repo_path.join("build.gradle.kts").exists()
    {
        Some("gradle".to_string())
    } else if repo_path.join("package.json").exists() {
        Some("npm".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_package_manager_cargo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        assert_eq!(detect_package_manager(dir.path()), Some("cargo".to_string()));
    }

    #[test]
    fn test_detect_package_manager_npm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(detect_package_manager(dir.path()), Some("npm".to_string()));
    }

    #[test]
    fn test_detect_package_manager_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_package_manager(dir.path()), None);
    }
}
