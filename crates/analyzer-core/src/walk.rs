use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use regex::Regex;

/// Patterns for noise files that should be excluded from coupling and hotspot analysis.
static NOISE_PATTERNS: &[&str] = &[
    r"package-lock\.json$",
    r"yarn\.lock$",
    r"Cargo\.lock$",
    r"go\.sum$",
    r"pnpm-lock\.yaml$",
    r"\.min\.(js|css)$",
    r"dist/",
    r"build/",
    r"vendor/",
];

/// Check if a file path is a noise file (lockfiles, minified, dist, build, vendor).
pub fn is_noise(path: &str) -> bool {
    // Use forward slashes for matching
    let normalized = path.replace('\\', "/");
    NOISE_PATTERNS.iter().any(|pattern| {
        Regex::new(pattern)
            .map(|re| re.is_match(&normalized))
            .unwrap_or(false)
    })
}

/// Walk files in a directory, respecting .gitignore rules.
///
/// Calls `callback` for each non-directory, non-hidden file found.
/// Uses the `ignore` crate to automatically respect .gitignore.
pub fn walk_files<F>(path: &Path, mut callback: F) -> Result<()>
where
    F: FnMut(&Path),
{
    let walker = WalkBuilder::new(path)
        .hidden(true) // skip hidden files
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker {
        let entry = entry?;
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            callback(entry.path());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_noise_lockfiles() {
        assert!(is_noise("package-lock.json"));
        assert!(is_noise("yarn.lock"));
        assert!(is_noise("Cargo.lock"));
        assert!(is_noise("go.sum"));
        assert!(is_noise("pnpm-lock.yaml"));
    }

    #[test]
    fn test_is_noise_minified() {
        assert!(is_noise("app.min.js"));
        assert!(is_noise("styles.min.css"));
    }

    #[test]
    fn test_is_noise_directories() {
        assert!(is_noise("dist/bundle.js"));
        assert!(is_noise("build/output.js"));
        assert!(is_noise("vendor/lib.js"));
    }

    #[test]
    fn test_is_not_noise() {
        assert!(!is_noise("src/main.rs"));
        assert!(!is_noise("README.md"));
        assert!(!is_noise("Cargo.toml"));
        assert!(!is_noise("app.js"));
    }
}
