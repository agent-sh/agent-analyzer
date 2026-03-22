//! License detection from repository files.

use std::path::Path;

static LICENSE_FILES: &[&str] = &[
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "LICENCE",
    "LICENCE.md",
    "COPYING",
    "license",
    "license.md",
];

struct LicensePattern {
    name: &'static str,
    markers: &'static [&'static str],
}

static LICENSE_PATTERNS: &[LicensePattern] = &[
    LicensePattern {
        name: "MIT",
        markers: &[
            "MIT License",
            "Permission is hereby granted, free of charge",
        ],
    },
    LicensePattern {
        name: "Apache-2.0",
        markers: &["Apache License", "Version 2.0"],
    },
    LicensePattern {
        name: "GPL-3.0",
        markers: &["GNU GENERAL PUBLIC LICENSE", "Version 3"],
    },
    LicensePattern {
        name: "GPL-2.0",
        markers: &["GNU GENERAL PUBLIC LICENSE", "Version 2"],
    },
    LicensePattern {
        name: "BSD-3-Clause",
        markers: &[
            "BSD 3-Clause",
            "Redistribution and use in source and binary forms",
        ],
    },
    LicensePattern {
        name: "BSD-2-Clause",
        markers: &["BSD 2-Clause"],
    },
    LicensePattern {
        name: "ISC",
        markers: &["ISC License", "Permission to use, copy, modify"],
    },
    LicensePattern {
        name: "MPL-2.0",
        markers: &["Mozilla Public License Version 2.0"],
    },
    LicensePattern {
        name: "Unlicense",
        markers: &["This is free and unencumbered software"],
    },
];

/// Detect license type from LICENSE file content.
pub fn detect_license(repo_path: &Path) -> Option<String> {
    // First check package manifests for SPDX identifiers
    if let Some(spdx) = detect_from_manifest(repo_path) {
        return Some(spdx);
    }

    // Then check LICENSE files
    for name in LICENSE_FILES {
        let path = repo_path.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let upper = content.to_uppercase();
            for pattern in LICENSE_PATTERNS {
                if pattern
                    .markers
                    .iter()
                    .all(|m| upper.contains(&m.to_uppercase()))
                {
                    return Some(pattern.name.to_string());
                }
            }
            // Found a license file but couldn't identify type
            return Some("unknown".to_string());
        }
    }

    None
}

fn detect_from_manifest(repo_path: &Path) -> Option<String> {
    // Check Cargo.toml
    if let Ok(content) = std::fs::read_to_string(repo_path.join("Cargo.toml")) {
        let mut in_workspace_package = false;
        for line in content.lines() {
            let trimmed = line.trim();

            // Track [workspace.package] section for workspace-level license
            if trimmed.starts_with('[') {
                in_workspace_package = trimmed == "[workspace.package]";
            }

            // Match `license = "MIT"` but not `license.workspace = true`
            if trimmed.starts_with("license") && trimmed.contains('=') {
                // Skip workspace delegation (license.workspace = true)
                if trimmed.starts_with("license.workspace") || trimmed.starts_with("license.path") {
                    continue;
                }
                let value = trimmed
                    .split('=')
                    .nth(1)?
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if !value.is_empty() && !value.contains('{') && value != "true" && value != "false"
                {
                    return Some(value.to_string());
                }
            }
        }

        // If this is a workspace member with `license.workspace = true`,
        // check if we found it in [workspace.package] section above
        if !in_workspace_package {
            // Re-scan for [workspace.package] license
            let mut in_ws = false;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed == "[workspace.package]" {
                    in_ws = true;
                    continue;
                }
                if in_ws && trimmed.starts_with('[') {
                    break;
                }
                if in_ws && trimmed.starts_with("license") && trimmed.contains('=') {
                    if trimmed.starts_with("license.") {
                        continue;
                    }
                    let value = trimmed
                        .split('=')
                        .nth(1)?
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'');
                    if !value.is_empty() && value != "true" && value != "false" {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }

    // Check package.json
    if let Ok(content) = std::fs::read_to_string(repo_path.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(license) = json.get("license").and_then(|v| v.as_str()) {
                return Some(license.to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_mit_license() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("LICENSE"),
            "MIT License\n\nPermission is hereby granted, free of charge...",
        )
        .unwrap();
        assert_eq!(detect_license(dir.path()), Some("MIT".to_string()));
    }

    #[test]
    fn test_detect_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nlicense = \"Apache-2.0\"\n",
        )
        .unwrap();
        assert_eq!(detect_license(dir.path()), Some("Apache-2.0".to_string()));
    }

    #[test]
    fn test_detect_from_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "test", "license": "ISC"}"#,
        )
        .unwrap();
        assert_eq!(detect_license(dir.path()), Some("ISC".to_string()));
    }

    #[test]
    fn test_detect_workspace_license() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nlicense.workspace = true\n\n[workspace.package]\nlicense = \"MIT\"\n",
        )
        .unwrap();
        assert_eq!(detect_license(dir.path()), Some("MIT".to_string()));
    }

    #[test]
    fn test_no_license() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_license(dir.path()).is_none());
    }
}
