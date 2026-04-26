//! Shared deny-list for known-secret file patterns.
//!
//! The repo walkers in analyzer-embed (embedding/indexing) and
//! analyzer-graph (slop detection) both want to skip files that are
//! almost certainly secrets to avoid surfacing them in indexes, logs, or
//! diagnostics. This module is the single source of truth for the
//! patterns they match on.
//!
//! This is defense-in-depth on top of `ignore::WalkBuilder::hidden(true)`
//! and `.standard_filters(true)`. The walker already excludes dotfiles
//! on Unix-like platforms, but:
//!
//! - `.hidden` semantics differ by platform.
//! - Non-hidden variants like `id_rsa`, `server.pem`, or anything under
//!   `.ssh/` / `.aws/` can still slip through.
//!
//! Keep this list conservative: false positives only cost one skipped
//! file; false negatives can leak credentials into an index or report.

use std::path::Path;

/// Returns `true` if the path looks like a file or directory we should
/// refuse to read during repo analysis because it likely contains
/// credentials.
pub fn is_secret_like(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };

    // Dotfile names we care about even if the walker misses them (e.g.
    // on platforms where `.hidden` has different semantics).
    if matches!(name, ".env" | ".npmrc" | ".pypirc" | ".netrc" | ".htpasswd") {
        return true;
    }
    if name.starts_with(".env.") {
        return true;
    }

    // SSH private-key filename conventions.
    if name.starts_with("id_rsa")
        || name.starts_with("id_dsa")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_ed25519")
    {
        return true;
    }

    // File extensions that are almost always cryptographic material.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if let Some(e) = ext.as_deref()
        && matches!(
            e,
            "pem" | "key" | "crt" | "p12" | "pfx" | "jks" | "keystore"
        )
    {
        return true;
    }

    // Secret-bearing directories: anywhere in the path.
    for comp in path.components() {
        if let Some(c) = comp.as_os_str().to_str()
            && matches!(
                c,
                ".git" | ".ssh" | ".gnupg" | ".aws" | ".gcloud" | ".azure"
            )
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dotenv_variants_are_secret_like() {
        assert!(is_secret_like(&PathBuf::from(".env")));
        assert!(is_secret_like(&PathBuf::from(".env.local")));
        assert!(is_secret_like(&PathBuf::from(".env.production")));
    }

    #[test]
    fn ssh_private_keys_are_secret_like() {
        assert!(is_secret_like(&PathBuf::from("id_rsa")));
        assert!(is_secret_like(&PathBuf::from("id_ed25519")));
        assert!(is_secret_like(&PathBuf::from("id_rsa.pub"))); // matches prefix rule
    }

    #[test]
    fn pem_extensions_are_secret_like() {
        assert!(is_secret_like(&PathBuf::from("server.pem")));
        assert!(is_secret_like(&PathBuf::from("keystore.jks")));
        assert!(is_secret_like(&PathBuf::from("cert.CRT"))); // case-insensitive
    }

    #[test]
    fn secret_directories_are_matched_anywhere() {
        assert!(is_secret_like(&PathBuf::from("home/me/.ssh/config")));
        assert!(is_secret_like(&PathBuf::from("project/.aws/credentials")));
    }

    #[test]
    fn ordinary_files_are_not_secret_like() {
        assert!(!is_secret_like(&PathBuf::from("README.md")));
        assert!(!is_secret_like(&PathBuf::from("src/main.rs")));
        assert!(!is_secret_like(&PathBuf::from("config.toml")));
    }
}
