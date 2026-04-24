//! Detect every place execution can begin in a repository.
//!
//! Combines manifest declarations, Cargo's auto-discovered targets,
//! Python package conventions, framework configs, and AST-derived
//! `main` functions to give agents and contributors a single answer to
//! "where does this code start running?" - replacing 4-6 grep + glob
//! calls per language with one lookup. Also feeds the orphan-export
//! slop detector so files referenced externally aren't flagged as
//! unused.
//!
//! # Scope
//!
//! - Cargo.toml `[[bin]]` (and the implicit `src/main.rs` binary)
//! - Cargo auto-discovered: `tests/*.rs`, `benches/*.rs`,
//!   `examples/*.rs`, `src/bin/*.rs`, `src/bin/<name>/main.rs`
//! - package.json `bin` field (string and object form)
//! - package.json `scripts` field
//! - pyproject.toml `[project.scripts]`
//! - Python `__main__.py` files (package execution entries)
//! - `main`-named function definitions from the AST symbol index
//! - Framework configs: standalone (`next.config.{js,ts,mjs}`,
//!   `vite.config.*`, `astro.config.*`, `nuxt.config.*`,
//!   `svelte.config.*`, `remix.config.*`, `tailwind.config.*`,
//!   `postcss.config.*`, `rollup.config.*`, `webpack.config.*`,
//!   `babel.config.*`, `jest.config.*`, `vitest.config.*`,
//!   `playwright.config.*`) plus Docusaurus convention
//!   (`docusaurus.config.*` anchors `sidebars.*`/`footer.*`/
//!   `navbar.*` siblings).
//!
//! # Out of scope
//!
//! - Framework-registration patterns inside source files (clap
//!   subcommands, axum/actix routes, express/FastAPI routes, queue
//!   consumer registrations)
//! - Python `if __name__ == "__main__":` guards - these are top-level
//!   `If` statements, not function definitions, and the symbol index
//!   only tracks definitions today (`__main__.py` files are detected
//!   via filename instead)
//! - pyproject `[project.scripts]` target file resolution - the script
//!   entry like `name = "module.path:callable"` is recorded but the
//!   target `.py` is not yet resolved to a repo-relative path

use std::collections::HashMap;
use std::path::Path;

use analyzer_core::types::{EntryPoint, EntryPointKind, FileSymbols, SymbolKind};

/// Detect entry points in `repo_path`, optionally augmenting with the
/// AST symbol index from a previously-collected repo-intel artifact.
///
/// Returns entries sorted by `(kind, path, name)` for stable output.
pub fn detect(repo_path: &Path, symbols: Option<&HashMap<String, FileSymbols>>) -> Vec<EntryPoint> {
    // Canonicalize so downstream `strip_prefix` works whether the caller
    // passed `.`, a relative path, or an absolute path. Fall back to the
    // raw path if canonicalization fails (e.g. nonexistent dir in tests).
    let repo_path: std::path::PathBuf =
        std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let repo_path = repo_path.as_path();

    let mut out = Vec::new();

    detect_cargo_bins(repo_path, &mut out);
    detect_cargo_auto_targets(repo_path, &mut out);
    detect_package_json(repo_path, &mut out);
    detect_pyproject(repo_path, &mut out);
    detect_python_main_files(repo_path, &mut out);
    detect_framework_configs(repo_path, &mut out);
    if let Some(syms) = symbols {
        detect_main_symbols(syms, &mut out);
    }

    out.sort_by(|a, b| {
        kind_sort_key(&a.kind)
            .cmp(&kind_sort_key(&b.kind))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.name.cmp(&b.name))
    });
    out.dedup_by(|a, b| a.kind == b.kind && a.path == b.path && a.name == b.name);
    out
}

fn kind_sort_key(k: &EntryPointKind) -> u8 {
    match k {
        EntryPointKind::Binary => 0,
        EntryPointKind::Main => 1,
        EntryPointKind::NpmScript => 2,
        EntryPointKind::FrameworkConfig => 3,
    }
}

/// Walk every `Cargo.toml` in the repo and surface its binaries.
///
/// In a workspace the root manifest typically only declares `[workspace]`
/// and no binaries; the actual `[[bin]]` entries live in member crates.
/// We follow `[workspace].members` (including `crates/*` glob form) and
/// also process the root if it has its own `[package]` section.
fn detect_cargo_bins(repo_path: &Path, out: &mut Vec<EntryPoint>) {
    let root_manifest = repo_path.join("Cargo.toml");
    let Ok(root_text) = std::fs::read_to_string(&root_manifest) else {
        return;
    };
    let Ok(root) = root_text.parse::<toml::Value>() else {
        return;
    };

    // Root manifest is processed directly only if it has [package].
    if root.get("package").is_some() {
        process_cargo_manifest(repo_path, repo_path, &root, out);
    }

    // Workspace members: each member's Cargo.toml is processed with that
    // member's directory as the relative-path base.
    if let Some(members) = root
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members.iter().filter_map(|m| m.as_str()) {
            for entry in expand_workspace_member(repo_path, member) {
                let member_manifest = entry.join("Cargo.toml");
                let Ok(text) = std::fs::read_to_string(&member_manifest) else {
                    continue;
                };
                let Ok(parsed) = text.parse::<toml::Value>() else {
                    continue;
                };
                process_cargo_manifest(repo_path, &entry, &parsed, out);
            }
        }
    }
}

/// Resolve a `[workspace].members` entry to concrete directories.
///
/// Supports two forms covering ~all real Cargo workspaces:
/// 1. concrete relative path (`crates/analyzer-cli`)
/// 2. single-level glob with one trailing `*` segment (`crates/*`)
///
/// Avoids the `glob` crate entirely so we don't have to wrestle with
/// Windows path separators or escape repo paths that themselves contain
/// glob metacharacters (a reviewer-caught hazard).
fn expand_workspace_member(repo_path: &Path, member: &str) -> Vec<std::path::PathBuf> {
    if !member.contains('*') {
        let candidate = repo_path.join(member);
        return if candidate.is_dir() {
            vec![candidate]
        } else {
            Vec::new()
        };
    }

    // Single-trailing-`*` pattern (the Cargo idiom): split on the last
    // `/` before the `*`, treat the prefix as a literal directory, and
    // list its children. Anything more exotic (`*foo`, `crates/*/sub`)
    // is treated as unsupported and yields nothing - real workspaces
    // don't do this.
    let trimmed = member.trim_end_matches('/');
    let Some((prefix, last)) = trimmed.rsplit_once('/') else {
        // member is like `*` at the workspace root - list repo_path's
        // direct subdirectories.
        if trimmed == "*" {
            return list_subdirs(repo_path);
        }
        return Vec::new();
    };
    if last != "*" {
        return Vec::new();
    }
    let parent = repo_path.join(prefix);
    list_subdirs(&parent)
}

fn list_subdirs(parent: &Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Surface binaries declared in one Cargo manifest. `member_dir` is the
/// directory that contains the manifest; emitted paths are made relative
/// to `repo_root` so callers always see repo-rooted paths regardless of
/// whether the manifest is the root or a workspace member.
fn process_cargo_manifest(
    repo_root: &Path,
    member_dir: &Path,
    manifest: &toml::Value,
    out: &mut Vec<EntryPoint>,
) {
    let package_name = manifest
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());

    let to_repo_path = |p: &str| -> String {
        let abs = member_dir.join(p);
        abs.strip_prefix(repo_root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.to_string())
    };

    // Explicit [[bin]] entries.
    let mut explicit_targets: Vec<String> = Vec::new();
    if let Some(bins) = manifest.get("bin").and_then(|b| b.as_array()) {
        for bin in bins {
            let name = bin
                .get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            // Cargo defaults for [[bin]] without an explicit `path`:
            // either `src/bin/<name>.rs` or `src/bin/<name>/main.rs`.
            // Probe both and prefer whichever exists; fall back to the
            // flat form when neither does so the entry still surfaces.
            let path = bin
                .get("path")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    name.as_ref().map(|n| {
                        let flat = format!("src/bin/{n}.rs");
                        let nested = format!("src/bin/{n}/main.rs");
                        if member_dir.join(&flat).exists() {
                            flat
                        } else if member_dir.join(&nested).exists() {
                            nested
                        } else {
                            flat
                        }
                    })
                });
            if let (Some(name), Some(path)) = (name, path) {
                let rel = to_repo_path(&path);
                explicit_targets.push(rel.clone());
                out.push(EntryPoint {
                    path: rel,
                    line: None,
                    kind: EntryPointKind::Binary,
                    name,
                });
            }
        }
    }

    // Implicit src/main.rs - only if the file exists AND no explicit
    // [[bin]] already points at it.
    let implicit_member_rel = "src/main.rs".to_string();
    let implicit_repo_rel = to_repo_path(&implicit_member_rel);
    if !explicit_targets.contains(&implicit_repo_rel)
        && member_dir.join(&implicit_member_rel).exists()
    {
        out.push(EntryPoint {
            path: implicit_repo_rel,
            line: None,
            kind: EntryPointKind::Binary,
            name: package_name.unwrap_or_else(|| "main".to_string()),
        });
    }
}

/// Read `package.json` and surface `bin` (string or object form) plus
/// every `scripts` entry.
fn detect_package_json(repo_path: &Path, out: &mut Vec<EntryPoint>) {
    let manifest_path = repo_path.join("package.json");
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };

    let pkg_name = manifest
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());

    if let Some(bin) = manifest.get("bin") {
        match bin {
            serde_json::Value::String(path) => {
                out.push(EntryPoint {
                    path: path.clone(),
                    line: None,
                    kind: EntryPointKind::Binary,
                    name: pkg_name.clone().unwrap_or_else(|| "bin".to_string()),
                });
            }
            serde_json::Value::Object(map) => {
                for (name, value) in map {
                    if let Some(path) = value.as_str() {
                        out.push(EntryPoint {
                            path: path.to_string(),
                            line: None,
                            kind: EntryPointKind::Binary,
                            name: name.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(scripts) = manifest.get("scripts").and_then(|s| s.as_object()) {
        for name in scripts.keys() {
            out.push(EntryPoint {
                path: "package.json".to_string(),
                line: None,
                kind: EntryPointKind::NpmScript,
                name: name.clone(),
            });
        }
    }
}

/// Read `pyproject.toml` and surface `[project.scripts]` console-script
/// entries.
fn detect_pyproject(repo_path: &Path, out: &mut Vec<EntryPoint>) {
    let manifest_path = repo_path.join("pyproject.toml");
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return;
    };
    let Ok(manifest) = text.parse::<toml::Value>() else {
        return;
    };

    let scripts = manifest
        .get("project")
        .and_then(|p| p.get("scripts"))
        .and_then(|s| s.as_table());
    if let Some(scripts) = scripts {
        for (name, target) in scripts {
            // `target` looks like "module.path:callable"; we treat it as
            // an opaque label and surface it as the binary name. The
            // file path is left as pyproject.toml because the actual
            // entry is generated by the installer, not present in source.
            let label = target
                .as_str()
                .map(|s| format!("{name} ({s})"))
                .unwrap_or_else(|| name.clone());
            out.push(EntryPoint {
                path: "pyproject.toml".to_string(),
                line: None,
                kind: EntryPointKind::Binary,
                name: label,
            });
        }
    }
}

/// Surface every `main`-named function definition from the AST symbol
/// index. Covers `fn main` (Rust), `def main` (Python), `func main()`
/// (Go), and `function main()` (JS/TS).
///
/// Filters by `SymbolKind::Function` so a struct, constant, or module
/// that happens to be named `main` does not get reported as an entry
/// point.
///
/// Does NOT detect Python `if __name__ == "__main__":` guards - those
/// are top-level `If` statements rather than function definitions, and
/// the symbol index only tracks definitions today.
fn detect_main_symbols(symbols: &HashMap<String, FileSymbols>, out: &mut Vec<EntryPoint>) {
    for (path, file_syms) in symbols {
        for def in &file_syms.definitions {
            if def.name == "main" && def.kind == SymbolKind::Function {
                out.push(EntryPoint {
                    path: path.clone(),
                    line: Some(def.line),
                    kind: EntryPointKind::Main,
                    name: def.name.clone(),
                });
            }
        }
    }
}

/// Cargo's auto-discovered targets that aren't declared via `[[bin]]`
/// but still compile to executables: integration tests (`tests/*.rs`
/// at crate root), benches (`benches/*.rs`), examples
/// (`examples/*.rs`), and binaries in the magic `src/bin/` dir
/// (`src/bin/*.rs` and `src/bin/*/main.rs`).
///
/// These files are linked by Cargo, not by `use` from another file —
/// without surfacing them, the orphan-export detector would flag
/// every integration test as "no importers in the project graph".
fn detect_cargo_auto_targets(repo_path: &Path, out: &mut Vec<EntryPoint>) {
    // Each Cargo workspace member (or the root crate) gets its tests/
    // benches/ examples/ src/bin/ dirs scanned. Reuse the workspace
    // expansion already used for `[[bin]]` discovery.
    let mut crate_dirs: Vec<std::path::PathBuf> = Vec::new();
    let root_manifest = repo_path.join("Cargo.toml");
    if let Ok(text) = std::fs::read_to_string(&root_manifest)
        && let Ok(root) = text.parse::<toml::Value>()
    {
        if root.get("package").is_some() {
            crate_dirs.push(repo_path.to_path_buf());
        }
        if let Some(members) = root
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array())
        {
            for member in members.iter().filter_map(|m| m.as_str()) {
                for dir in expand_workspace_member(repo_path, member) {
                    if dir.join("Cargo.toml").exists() {
                        crate_dirs.push(dir);
                    }
                }
            }
        }
    }

    for crate_dir in crate_dirs {
        for sub in &["tests", "benches", "examples"] {
            let dir = crate_dir.join(sub);
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = match path.strip_prefix(repo_path) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(sub)
                    .to_string();
                out.push(EntryPoint {
                    path: rel,
                    line: None,
                    kind: EntryPointKind::Binary,
                    name: format!("{sub}::{name}"),
                });
            }
        }

        // src/bin/<name>.rs (flat) and src/bin/<name>/main.rs (nested)
        let bin_dir = crate_dir.join("src/bin");
        if bin_dir.is_dir() {
            for entry in std::fs::read_dir(&bin_dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    if let Ok(rel) = path.strip_prefix(repo_path) {
                        let name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("bin")
                            .to_string();
                        out.push(EntryPoint {
                            path: rel.to_string_lossy().replace('\\', "/"),
                            line: None,
                            kind: EntryPointKind::Binary,
                            name,
                        });
                    }
                } else if path.is_dir() {
                    let nested = path.join("main.rs");
                    if nested.exists()
                        && let Ok(rel) = nested.strip_prefix(repo_path)
                    {
                        let name = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("bin")
                            .to_string();
                        out.push(EntryPoint {
                            path: rel.to_string_lossy().replace('\\', "/"),
                            line: None,
                            kind: EntryPointKind::Binary,
                            name,
                        });
                    }
                }
            }
        }
    }
}

/// Surface `__main__.py` files. These are the entry point a user
/// invokes via `python -m <package>`, so they appear orphan to the
/// import graph but are essential.
fn detect_python_main_files(repo_path: &Path, out: &mut Vec<EntryPoint>) {
    fn walk(dir: &Path, repo_root: &Path, out: &mut Vec<EntryPoint>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            // Skip common noise dirs to keep the walk cheap.
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_noise_dir(&name_str) {
                continue;
            }
            if path.is_dir() {
                walk(&path, repo_root, out, depth + 1);
            } else if name_str == "__main__.py"
                && let Ok(rel) = path.strip_prefix(repo_root)
            {
                out.push(EntryPoint {
                    path: rel.to_string_lossy().replace('\\', "/"),
                    line: None,
                    kind: EntryPointKind::Main,
                    name: "__main__".to_string(),
                });
            }
        }
    }
    walk(repo_path, repo_path, out, 0);
}

/// Surface framework-loaded config files — files the framework reads
/// by convention rather than via `import`/`require`. Without these,
/// orphan-export would flag every `docusaurus.config.js`,
/// `next.config.js`, etc. as unused.
///
/// Detection is convention-based: when one anchor file
/// (`docusaurus.config.{js,ts}`) exists in a directory, the related
/// sidebars/companion files in the same dir are also surfaced.
/// Standalone framework configs (`next.config.js`, `vite.config.ts`,
/// `astro.config.mjs`, etc.) are surfaced wherever found.
fn is_noise_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "dist" | "build" | "venv" | ".venv" | "__pycache__"
        )
}

fn detect_framework_configs(repo_path: &Path, out: &mut Vec<EntryPoint>) {
    fn walk(dir: &Path, repo_root: &Path, out: &mut Vec<EntryPoint>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<std::path::PathBuf> = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_noise_dir(&name_str) {
                continue;
            }
            entries.push(entry.path());
        }

        // Standalone framework configs anywhere in the tree.
        for path in &entries {
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && is_standalone_framework_config(name)
                && let Ok(rel) = path.strip_prefix(repo_root)
            {
                out.push(EntryPoint {
                    path: rel.to_string_lossy().replace('\\', "/"),
                    line: None,
                    kind: EntryPointKind::FrameworkConfig,
                    name: name.to_string(),
                });
            }
        }

        // Docusaurus convention: when docusaurus.config.{js,ts,mjs}
        // exists in this dir, both the anchor itself AND its sibling
        // sidebars / footer / navbar files are framework-loaded.
        let has_docusaurus = entries.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(is_docusaurus_anchor)
                .unwrap_or(false)
        });
        if has_docusaurus {
            for path in &entries {
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !is_docusaurus_anchor(name) && !is_docusaurus_companion(name) {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(repo_root) {
                    out.push(EntryPoint {
                        path: rel.to_string_lossy().replace('\\', "/"),
                        line: None,
                        kind: EntryPointKind::FrameworkConfig,
                        name: name.to_string(),
                    });
                }
            }
        }

        for path in entries {
            if path.is_dir() {
                walk(&path, repo_root, out, depth + 1);
            }
        }
    }
    walk(repo_path, repo_path, out, 0);
}

fn is_standalone_framework_config(name: &str) -> bool {
    matches!(
        name,
        "next.config.js"
            | "next.config.ts"
            | "next.config.mjs"
            | "next.config.cjs"
            | "vite.config.js"
            | "vite.config.ts"
            | "vite.config.mjs"
            | "vite.config.cjs"
            | "astro.config.js"
            | "astro.config.ts"
            | "astro.config.mjs"
            | "astro.config.cjs"
            | "nuxt.config.js"
            | "nuxt.config.ts"
            | "nuxt.config.mjs"
            | "svelte.config.js"
            | "svelte.config.ts"
            | "svelte.config.mjs"
            | "svelte.config.cjs"
            | "remix.config.js"
            | "remix.config.cjs"
            | "remix.config.mjs"
            | "tailwind.config.js"
            | "tailwind.config.ts"
            | "tailwind.config.cjs"
            | "tailwind.config.mjs"
            | "postcss.config.js"
            | "postcss.config.cjs"
            | "postcss.config.mjs"
            | "rollup.config.js"
            | "rollup.config.mjs"
            | "rollup.config.cjs"
            | "rollup.config.ts"
            | "webpack.config.js"
            | "webpack.config.cjs"
            | "webpack.config.mjs"
            | "webpack.config.ts"
            | "babel.config.js"
            | "babel.config.cjs"
            | "babel.config.mjs"
            | "babel.config.json"
            | "jest.config.js"
            | "jest.config.ts"
            | "jest.config.cjs"
            | "jest.config.mjs"
            | "vitest.config.js"
            | "vitest.config.ts"
            | "vitest.config.cjs"
            | "vitest.config.mjs"
            | "playwright.config.js"
            | "playwright.config.ts"
            | "playwright.config.cjs"
            | "playwright.config.mjs"
    )
}

fn is_docusaurus_anchor(name: &str) -> bool {
    matches!(
        name,
        "docusaurus.config.js"
            | "docusaurus.config.ts"
            | "docusaurus.config.mjs"
            | "docusaurus.config.cjs"
    )
}

fn is_docusaurus_companion(name: &str) -> bool {
    matches!(
        name,
        "sidebars.js"
            | "sidebars.ts"
            | "sidebars.mjs"
            | "sidebars.cjs"
            | "footer.js"
            | "footer.ts"
            | "footer.mjs"
            | "footer.cjs"
            | "navbar.js"
            | "navbar.ts"
            | "navbar.mjs"
            | "navbar.cjs"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{DefinitionEntry, ImportEntry, SymbolKind};
    use std::fs;
    use tempfile::TempDir;

    fn make_repo() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    #[test]
    fn cargo_explicit_bins_detected() {
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[[bin]]
name = "alpha"
path = "src/alpha.rs"

[[bin]]
name = "beta"
"#,
        )
        .unwrap();

        let eps = detect(dir.path(), None);
        let names: Vec<&str> = eps.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "explicit name+path should appear");
        assert!(
            names.contains(&"beta"),
            "explicit name without path should appear"
        );
        let beta = eps.iter().find(|e| e.name == "beta").unwrap();
        assert_eq!(
            beta.path, "src/bin/beta.rs",
            "missing path defaults to src/bin/<name>.rs"
        );
    }

    #[test]
    fn cargo_implicit_main_detected() {
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        let eps = detect(dir.path(), None);
        let main_bin = eps
            .iter()
            .find(|e| e.path == "src/main.rs" && e.kind == EntryPointKind::Binary)
            .expect("implicit main binary should appear");
        assert_eq!(main_bin.name, "demo", "implicit binary uses package name");
    }

    #[test]
    fn cargo_implicit_bin_resolves_nested_main() {
        // Cargo also auto-resolves a [[bin]] without explicit path to
        // `src/bin/<name>/main.rs` when that file exists. The detector
        // should pick the nested form when only it exists.
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[[bin]]
name = "nested"
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src/bin/nested")).unwrap();
        fs::write(dir.path().join("src/bin/nested/main.rs"), "fn main() {}").unwrap();

        let eps = detect(dir.path(), None);
        let nested = eps
            .iter()
            .find(|e| e.name == "nested")
            .expect("nested-form bin should appear");
        assert_eq!(nested.path, "src/bin/nested/main.rs");
    }

    #[test]
    fn workspace_member_glob_handles_glob_metachars_in_repo_path() {
        // Reviewer-caught: a repo path containing glob metacharacters
        // (e.g. `[ci]` or `repo*`) would previously confuse glob::glob
        // when joined with the workspace member pattern. The detector
        // must escape the prefix so only the member portion is treated
        // as a pattern.
        let parent = TempDir::new().unwrap();
        let weird_dir = parent.path().join("repo[ci]");
        fs::create_dir_all(&weird_dir).unwrap();
        fs::write(
            weird_dir.join("Cargo.toml"),
            r#"
[workspace]
resolver = "2"
members = ["crates/*"]
"#,
        )
        .unwrap();
        fs::create_dir_all(weird_dir.join("crates/cli/src")).unwrap();
        fs::write(
            weird_dir.join("crates/cli/Cargo.toml"),
            r#"
[package]
name = "cli"
version = "0.1.0"

[[bin]]
name = "tool"
path = "src/main.rs"
"#,
        )
        .unwrap();
        fs::write(weird_dir.join("crates/cli/src/main.rs"), "fn main() {}").unwrap();

        let eps = detect(&weird_dir, None);
        let tool = eps
            .iter()
            .find(|e| e.name == "tool")
            .expect("workspace bin must be found even when repo path contains [ci]");
        assert_eq!(tool.path, "crates/cli/src/main.rs");
    }

    #[test]
    fn cargo_workspace_member_bins_detected() {
        // Workspaces have no [[bin]] in the root manifest - the actual
        // binaries live in member crates. Detector must walk
        // [workspace].members (concrete paths and `crates/*` glob form).
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[workspace]
resolver = "2"
members = ["crates/*"]
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("crates/cli/src")).unwrap();
        fs::write(
            dir.path().join("crates/cli/Cargo.toml"),
            r#"
[package]
name = "cli"
version = "0.1.0"

[[bin]]
name = "mycli"
path = "src/main.rs"
"#,
        )
        .unwrap();
        fs::write(dir.path().join("crates/cli/src/main.rs"), "fn main() {}").unwrap();

        let eps = detect(dir.path(), None);
        let mycli = eps
            .iter()
            .find(|e| e.kind == EntryPointKind::Binary && e.name == "mycli")
            .expect("workspace member [[bin]] should appear");
        // Path must be repo-rooted, not member-rooted.
        assert_eq!(mycli.path, "crates/cli/src/main.rs");
    }

    #[test]
    fn cargo_implicit_main_skipped_when_explicit_bin_overrides() {
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[[bin]]
name = "demo"
path = "src/main.rs"
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        let eps = detect(dir.path(), None);
        let bins: Vec<_> = eps
            .iter()
            .filter(|e| e.path == "src/main.rs" && e.kind == EntryPointKind::Binary)
            .collect();
        assert_eq!(
            bins.len(),
            1,
            "explicit [[bin]] should not be duplicated by implicit detection"
        );
    }

    #[test]
    fn package_json_bin_string_form() {
        let dir = make_repo();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name": "tool", "bin": "./cli.js", "scripts": {"build": "tsc"}}"#,
        )
        .unwrap();

        let eps = detect(dir.path(), None);
        let cli = eps.iter().find(|e| e.path == "./cli.js").unwrap();
        assert_eq!(cli.kind, EntryPointKind::Binary);
        assert_eq!(cli.name, "tool", "string-form bin uses package name");

        let build = eps
            .iter()
            .find(|e| e.kind == EntryPointKind::NpmScript && e.name == "build")
            .unwrap();
        assert_eq!(build.path, "package.json");
    }

    #[test]
    fn package_json_bin_object_form() {
        let dir = make_repo();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name": "pkg", "bin": {"foo": "./bin/foo.js", "bar": "./bin/bar.js"}}"#,
        )
        .unwrap();

        let eps = detect(dir.path(), None);
        let names: Vec<&str> = eps
            .iter()
            .filter(|e| e.kind == EntryPointKind::Binary)
            .map(|e| e.name.as_str())
            .collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn pyproject_scripts_detected() {
        let dir = make_repo();
        fs::write(
            dir.path().join("pyproject.toml"),
            r#"
[project]
name = "demo"
version = "0.1.0"

[project.scripts]
mytool = "demo.cli:main"
"#,
        )
        .unwrap();

        let eps = detect(dir.path(), None);
        let mytool = eps
            .iter()
            .find(|e| e.kind == EntryPointKind::Binary && e.name.starts_with("mytool"))
            .expect("[project.scripts] entry should appear");
        assert!(mytool.name.contains("demo.cli:main"));
        assert_eq!(mytool.path, "pyproject.toml");
    }

    #[test]
    fn ast_main_symbols_detected() {
        let dir = make_repo();
        let mut syms = HashMap::new();
        syms.insert(
            "src/main.rs".to_string(),
            FileSymbols {
                exports: vec![],
                imports: vec![ImportEntry {
                    from: "std".into(),
                    names: vec!["env".into()],
                }],
                definitions: vec![
                    DefinitionEntry {
                        name: "main".to_string(),
                        kind: SymbolKind::Function,
                        line: 7,
                        complexity: 1,
                    },
                    DefinitionEntry {
                        name: "helper".to_string(),
                        kind: SymbolKind::Function,
                        line: 20,
                        complexity: 1,
                    },
                ],
            },
        );

        let eps = detect(dir.path(), Some(&syms));
        let main = eps
            .iter()
            .find(|e| e.kind == EntryPointKind::Main)
            .expect("AST main should be surfaced");
        assert_eq!(main.path, "src/main.rs");
        assert_eq!(main.line, Some(7));
        assert_eq!(main.name, "main");
        // Non-main definitions must not show up.
        assert!(eps.iter().all(|e| e.name != "helper"));
    }

    #[test]
    fn ast_main_only_matches_functions_not_other_symbol_kinds() {
        // Reviewer-caught: a struct, constant, or module literally named
        // `main` would previously surface as a Main entry point because
        // detect_main_symbols only filtered on the name. Filter on kind
        // too so only actual `fn main` / `def main` / `func main` hit.
        let dir = make_repo();
        let mut syms = HashMap::new();
        syms.insert(
            "src/types.rs".to_string(),
            FileSymbols {
                exports: vec![],
                imports: vec![],
                definitions: vec![
                    DefinitionEntry {
                        name: "main".to_string(),
                        kind: SymbolKind::Struct,
                        line: 5,
                        complexity: 1,
                    },
                    DefinitionEntry {
                        name: "main".to_string(),
                        kind: SymbolKind::Constant,
                        line: 10,
                        complexity: 1,
                    },
                ],
            },
        );

        let eps = detect(dir.path(), Some(&syms));
        assert!(
            eps.iter().all(|e| e.kind != EntryPointKind::Main),
            "non-function `main` symbols must not surface as entry points"
        );
    }

    #[test]
    fn empty_repo_yields_empty_result() {
        let dir = make_repo();
        assert!(detect(dir.path(), None).is_empty());
    }

    #[test]
    fn output_is_sorted_and_deduped() {
        let dir = make_repo();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name": "p", "bin": {"zeta": "./z.js", "alpha": "./a.js"}}"#,
        )
        .unwrap();

        let eps = detect(dir.path(), None);
        let names: Vec<&str> = eps
            .iter()
            .filter(|e| e.kind == EntryPointKind::Binary)
            .map(|e| e.name.as_str())
            .collect();
        // Sorted by path; "./a.js" < "./z.js" so alpha comes first.
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    // ── Cargo auto-discovered targets ───────────────

    #[test]
    fn cargo_tests_dir_files_surface_as_binary() {
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"[package]
name = "p"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("tests/integration.rs"), "").unwrap();
        fs::write(dir.path().join("tests/another.rs"), "").unwrap();

        let eps = detect(dir.path(), None);
        let test_eps: Vec<&str> = eps
            .iter()
            .filter(|e| e.kind == EntryPointKind::Binary && e.path.starts_with("tests/"))
            .map(|e| e.path.as_str())
            .collect();
        assert!(test_eps.contains(&"tests/integration.rs"));
        assert!(test_eps.contains(&"tests/another.rs"));
    }

    #[test]
    fn cargo_benches_and_examples_surface_as_binary() {
        let dir = make_repo();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"[package]
name = "p"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("benches")).unwrap();
        fs::create_dir_all(dir.path().join("examples")).unwrap();
        fs::write(dir.path().join("benches/perf.rs"), "").unwrap();
        fs::write(dir.path().join("examples/demo.rs"), "").unwrap();

        let eps = detect(dir.path(), None);
        let paths: Vec<&str> = eps.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"benches/perf.rs"));
        assert!(paths.contains(&"examples/demo.rs"));
    }

    // ── Python __main__.py ───────────

    #[test]
    fn python_main_files_detected_at_any_depth() {
        let dir = make_repo();
        fs::create_dir_all(dir.path().join("pkg/sub")).unwrap();
        fs::write(dir.path().join("pkg/__main__.py"), "").unwrap();
        fs::write(dir.path().join("pkg/sub/__main__.py"), "").unwrap();

        let eps = detect(dir.path(), None);
        let main_paths: Vec<&str> = eps
            .iter()
            .filter(|e| e.kind == EntryPointKind::Main && e.path.ends_with("__main__.py"))
            .map(|e| e.path.as_str())
            .collect();
        assert!(main_paths.contains(&"pkg/__main__.py"));
        assert!(main_paths.contains(&"pkg/sub/__main__.py"));
    }

    // ── Framework configs ───────────

    #[test]
    fn standalone_framework_configs_detected() {
        let dir = make_repo();
        fs::write(dir.path().join("next.config.js"), "module.exports = {};").unwrap();
        fs::create_dir_all(dir.path().join("app")).unwrap();
        fs::write(dir.path().join("app/vite.config.ts"), "export default {};").unwrap();

        let eps = detect(dir.path(), None);
        let names: Vec<&str> = eps
            .iter()
            .filter(|e| e.kind == EntryPointKind::FrameworkConfig)
            .map(|e| e.name.as_str())
            .collect();
        assert!(names.contains(&"next.config.js"));
        assert!(names.contains(&"vite.config.ts"));
    }

    #[test]
    fn docusaurus_companions_detected_only_when_anchor_present() {
        let dir = make_repo();
        fs::create_dir_all(dir.path().join("website")).unwrap();
        fs::write(
            dir.path().join("website/docusaurus.config.js"),
            "module.exports = {};",
        )
        .unwrap();
        fs::write(
            dir.path().join("website/sidebars.js"),
            "module.exports = [];",
        )
        .unwrap();
        // A `sidebars.js` outside the docusaurus directory should NOT
        // be flagged — convention only applies when the anchor is
        // present in the same dir.
        fs::create_dir_all(dir.path().join("other")).unwrap();
        fs::write(dir.path().join("other/sidebars.js"), "module.exports = [];").unwrap();

        let eps = detect(dir.path(), None);
        let configs: Vec<&str> = eps
            .iter()
            .filter(|e| e.kind == EntryPointKind::FrameworkConfig)
            .map(|e| e.path.as_str())
            .collect();
        assert!(configs.contains(&"website/docusaurus.config.js"));
        assert!(configs.contains(&"website/sidebars.js"));
        assert!(
            !configs.contains(&"other/sidebars.js"),
            "sidebars.js outside docusaurus dir should not be flagged"
        );
    }
}
