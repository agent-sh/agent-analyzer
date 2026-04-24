//! Walk source files and extract symbols using tree-sitter.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;

use analyzer_core::types::{DefinitionEntry, FileSymbols, ImportEntry, SymbolEntry, SymbolKind};
use analyzer_core::walk;

use crate::complexity::cyclomatic_complexity;
use crate::parser::{Language, detect_language, parse_source};

/// Maximum file size to parse (500KB). Larger files are likely generated.
const MAX_FILE_SIZE: u64 = 500_000;

/// Extracted symbol data from a repository.
pub type SymbolData = (HashMap<String, FileSymbols>, HashMap<String, Vec<String>>);

/// Extract symbols from all source files in a repository.
/// Returns (symbols_map, import_graph).
pub fn extract_symbols(repo_path: &Path) -> Result<SymbolData> {
    // Collect all parseable files
    let mut files: Vec<(String, Language)> = Vec::new();
    walk::walk_files(repo_path, |path| {
        let rel = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if walk::is_noise(&rel) {
            return;
        }
        if let Some(lang) = detect_language(&rel) {
            // Skip large files
            if let Ok(meta) = std::fs::metadata(path) {
                if meta.len() <= MAX_FILE_SIZE {
                    files.push((rel, lang));
                }
            }
        }
    })?;

    // Parse files in parallel
    let results: Vec<(String, FileSymbols)> = files
        .par_iter()
        .filter_map(|(rel, lang)| {
            let abs = repo_path.join(rel);
            let source = std::fs::read(&abs).ok()?;
            match extract_file_symbols(&source, *lang) {
                Ok(syms) => Some((rel.clone(), syms)),
                Err(_) => None, // skip files that fail to parse
            }
        })
        .collect();

    let mut symbols_map = HashMap::new();
    for (path, syms) in results {
        symbols_map.insert(path, syms);
    }

    // Build a set of known file paths so the resolver can verify a
    // candidate target actually exists in the repo's symbol map.
    let known_paths: std::collections::HashSet<String> = symbols_map.keys().cloned().collect();

    let mut import_graph: HashMap<String, Vec<String>> = HashMap::new();
    for (path, syms) in &symbols_map {
        let lang = detect_language(path);
        let mut imports: Vec<String> = Vec::with_capacity(syms.imports.len());
        for imp in &syms.imports {
            // Resolve `mod foo;` declarations to actual sibling files
            // — without this the import graph misses every intra-crate
            // reference that flows through `mod foo;` instead of
            // `use crate::foo`. Falls back to the textual `from` when
            // resolution fails (e.g. the target file is excluded).
            if let Some(name) = imp.from.strip_prefix("__mod_decl__::") {
                if let Some(resolved) = resolve_rust_mod_decl(path, name, &known_paths) {
                    imports.push(resolved);
                    continue;
                }
                // Could not resolve — drop the synthetic edge rather
                // than carry the sentinel through to consumers.
                continue;
            }
            // TS/JS relative imports: `./foo`, `../utils/helper`, etc.
            // Try every reasonable extension and `index.*` fallback.
            // Bare imports (`react`, `lodash`) stay textual since they're
            // third-party and won't match a repo file.
            if matches!(
                lang,
                Some(Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx)
            ) && (imp.from.starts_with("./") || imp.from.starts_with("../"))
            {
                if let Some(resolved) = resolve_ts_js_relative(path, &imp.from, &known_paths) {
                    imports.push(resolved);
                    continue;
                }
            }
            // Python relative imports: `.foo`, `..pkg.foo`, plus absolute
            // `pkg.module` style. Try resolving against known files;
            // if no match, keep the textual form.
            if matches!(lang, Some(Language::Python)) {
                if let Some(resolved) = resolve_python_import(path, &imp.from, &known_paths) {
                    imports.push(resolved);
                    continue;
                }
            }
            imports.push(imp.from.clone());
        }
        if !imports.is_empty() {
            import_graph.insert(path.clone(), imports);
        }
    }

    Ok((symbols_map, import_graph))
}

/// Resolve `mod foo;` in `importer_path` to the file that defines the
/// `foo` module. Handles the three Rust layouts in the wild:
///
/// **2015 edition (mod.rs style):** importer is `lib.rs` / `main.rs` /
/// `mod.rs`, siblings live in the same dir.
///
///   - `<importer_dir>/foo.rs`
///   - `<importer_dir>/foo/mod.rs`
///
/// **2018 edition (sibling .rs files):** importer is `bar.rs` (NOT
/// `mod.rs`/`lib.rs`/`main.rs`), and `mod foo;` may resolve to a file
/// in the `bar/` subdirectory rather than a true sibling.
///
///   - `<importer_dir>/<importer_stem>/foo.rs`     (preferred)
///   - `<importer_dir>/<importer_stem>/foo/mod.rs`
///   - then fall through to the 2015 candidates
///
/// Returns `None` when no candidate is a known file. Caller drops the
/// edge in that case.
fn resolve_rust_mod_decl(
    importer_path: &str,
    mod_name: &str,
    known_paths: &std::collections::HashSet<String>,
) -> Option<String> {
    let (importer_dir, importer_file) = match importer_path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", importer_path),
    };
    let importer_stem = importer_file.strip_suffix(".rs").unwrap_or(importer_file);
    let is_root_file = matches!(importer_stem, "mod" | "lib" | "main");

    let mut candidates: Vec<String> = Vec::new();

    // 2018-edition nested case: `<dir>/<stem>/<mod>.rs`. Only try this
    // when importer is NOT a root file (mod.rs/lib.rs/main.rs all
    // resolve via siblings, not nested children).
    if !is_root_file {
        let nest_dir = if importer_dir.is_empty() {
            importer_stem.to_string()
        } else {
            format!("{importer_dir}/{importer_stem}")
        };
        candidates.push(format!("{nest_dir}/{mod_name}.rs"));
        candidates.push(format!("{nest_dir}/{mod_name}/mod.rs"));
    }

    // 2015-edition / sibling case: `<importer_dir>/<mod>.rs`.
    let sibling_prefix = if importer_dir.is_empty() {
        String::new()
    } else {
        format!("{importer_dir}/")
    };
    candidates.push(format!("{sibling_prefix}{mod_name}.rs"));
    candidates.push(format!("{sibling_prefix}{mod_name}/mod.rs"));

    candidates.into_iter().find(|c| known_paths.contains(c))
}

/// Resolve a TS/JS relative import like `./foo` or `../utils/helper`
/// to an actual repo file. Tries every common extension plus the
/// `index.*` fallback for directory imports. Returns `None` when no
/// candidate matches a known file.
///
/// Resolution order (per the Node module algorithm):
///
///   1. `<resolved>.{ts, tsx, js, jsx, mjs, cjs}` — exact file
///   2. `<resolved>/index.{ts, tsx, js, jsx, mjs, cjs}` — dir index
fn resolve_ts_js_relative(
    importer_path: &str,
    import_from: &str,
    known_paths: &std::collections::HashSet<String>,
) -> Option<String> {
    let importer_dir = match importer_path.rsplit_once('/') {
        Some((d, _)) => d,
        None => "",
    };
    let combined = if importer_dir.is_empty() {
        import_from.to_string()
    } else {
        format!("{importer_dir}/{import_from}")
    };
    let normalized = normalize_path(&combined);
    const EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    for ext in EXTS {
        let candidate = format!("{normalized}.{ext}");
        if known_paths.contains(&candidate) {
            return Some(candidate);
        }
    }
    for ext in EXTS {
        let candidate = format!("{normalized}/index.{ext}");
        if known_paths.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Resolve a Python import (`from .foo import bar`, `from ..pkg.x
/// import y`, or absolute `from pkg.module import name`) to a file
/// path. Handles both `<name>.py` files and `<name>/__init__.py`
/// package directories.
///
/// Relative imports are scoped against the importer's directory
/// (with `.` going up one level per leading dot). Absolute imports
/// fall back to scanning the known-paths set for any file whose
/// trailing path components match — covers `pkg.module` regardless
/// of whether `pkg` is a directory at the repo root or nested under
/// `src/`. Returns `None` when nothing matches.
fn resolve_python_import(
    importer_path: &str,
    import_from: &str,
    known_paths: &std::collections::HashSet<String>,
) -> Option<String> {
    let importer_dir = match importer_path.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => String::new(),
    };

    // Relative imports: count leading dots, walk up that many dirs.
    let leading_dots = import_from.chars().take_while(|c| *c == '.').count();
    if leading_dots > 0 {
        let rest = &import_from[leading_dots..];
        let rest_parts: Vec<&str> = rest.split('.').filter(|s| !s.is_empty()).collect();
        let mut base = importer_dir.clone();
        for _ in 0..leading_dots.saturating_sub(1) {
            base = match base.rsplit_once('/') {
                Some((parent, _)) => parent.to_string(),
                None => String::new(),
            };
        }
        let nested = if rest_parts.is_empty() {
            base.clone()
        } else if base.is_empty() {
            rest_parts.join("/")
        } else {
            format!("{base}/{}", rest_parts.join("/"))
        };
        let candidates = [format!("{nested}.py"), format!("{nested}/__init__.py")];
        for c in candidates {
            if known_paths.contains(&c) {
                return Some(c);
            }
        }
        return None;
    }

    // Absolute import: `pkg.module.x` → look for any known file ending
    // with `pkg/module/x.py` or `pkg/module/x/__init__.py`. Skips
    // standard-library names (single component, no dot) since those
    // won't be repo files anyway.
    if !import_from.contains('.') {
        // Single-component name might be a stdlib module OR a top-level
        // repo module — check both directly under the importer's dir
        // and at the repo root.
        let suffixes = [
            format!("{import_from}.py"),
            format!("{import_from}/__init__.py"),
        ];
        for s in suffixes {
            if known_paths.contains(&s) {
                return Some(s);
            }
            if !importer_dir.is_empty() {
                let scoped = format!("{importer_dir}/{s}");
                if known_paths.contains(&scoped) {
                    return Some(scoped);
                }
            }
        }
        return None;
    }

    let parts = import_from.replace('.', "/");
    let suffixes = [format!("{parts}.py"), format!("{parts}/__init__.py")];
    for suf in &suffixes {
        for known in known_paths {
            if known == suf || known.ends_with(&format!("/{suf}")) {
                return Some(known.clone());
            }
        }
    }
    None
}

/// Normalize a path by collapsing `.` and `..` segments. Pure string
/// operation — doesn't touch the filesystem. Used by the TS/JS and
/// Python resolvers when joining a relative import against the
/// importer's directory.
fn normalize_path(p: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

/// Extract symbols from a single file's source code.
pub fn extract_file_symbols(source: &[u8], lang: Language) -> Result<FileSymbols> {
    let tree = parse_source(source, lang)?;
    let root = tree.root_node();

    let mut exports = Vec::new();
    let mut imports = Vec::new();
    let mut definitions = Vec::new();

    extract_from_node(
        &root,
        source,
        lang,
        &mut exports,
        &mut imports,
        &mut definitions,
    )?;

    Ok(FileSymbols {
        exports,
        imports,
        definitions,
    })
}

fn extract_from_node(
    node: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) -> Result<()> {
    match lang {
        Language::Rust => extract_rust(node, source, exports, imports, definitions),
        Language::TypeScript | Language::Tsx => {
            extract_ts_js(node, source, lang, true, exports, imports, definitions)
        }
        Language::JavaScript | Language::Jsx => {
            extract_ts_js(node, source, lang, false, exports, imports, definitions)
        }
        Language::Python => extract_python(node, source, exports, imports, definitions),
        Language::Go => extract_go(node, source, exports, imports, definitions),
        Language::Java => extract_java(node, source, lang, exports, imports, definitions),
    }
    Ok(())
}

// ─── Rust ───────────────────────────────────────────────────────

fn extract_rust(
    root: &tree_sitter::Node,
    source: &[u8],
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    let cc = cyclomatic_complexity(&child, source, Language::Rust);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                        });
                    }
                }
            }
            "struct_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Struct,
                        line,
                        complexity: 1,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Struct,
                            line,
                        });
                    }
                }
                // Extract struct fields
                if let Some(body) = child.child_by_field_name("body") {
                    extract_rust_fields(&body, source, definitions);
                }
            }
            "enum_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Enum,
                        line,
                        complexity: 1,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Enum,
                            line,
                        });
                    }
                }
                // Extract enum variants
                if let Some(body) = child.child_by_field_name("body") {
                    extract_rust_enum_variants(&body, source, definitions);
                }
            }
            "trait_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Trait,
                        line,
                        complexity: 1,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Trait,
                            line,
                        });
                    }
                }
            }
            "const_item" | "static_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Constant,
                        line,
                        complexity: 1,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Constant,
                            line,
                        });
                    }
                }
            }
            "type_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::TypeAlias,
                        line,
                        complexity: 1,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::TypeAlias,
                            line,
                        });
                    }
                }
            }
            "mod_item" => {
                let body = child.child_by_field_name("body");
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_pub = child_has_visibility(&child);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Module,
                        line,
                        complexity: 1,
                    });
                    if is_pub {
                        exports.push(SymbolEntry {
                            name: name_str.clone(),
                            kind: SymbolKind::Module,
                            line,
                        });
                    }
                    // `mod foo;` (no body) declares an external file.
                    // Tag with a sentinel `from` so the post-extraction
                    // resolver knows to map it to a sibling .rs file.
                    // Without this edge the import graph misses every
                    // intra-crate sibling-module reference, which makes
                    // every "module-only" file look like an orphan
                    // export to slop-fixes.
                    if body.is_none() {
                        imports.push(ImportEntry {
                            from: format!("__mod_decl__::{name_str}"),
                            names: vec![name_str],
                        });
                    }
                }
                // Recurse into mod body (e.g., #[cfg(test)] mod tests { ... })
                if let Some(body) = body {
                    extract_rust(&body, source, exports, imports, definitions);
                }
            }
            "impl_item" => {
                // Recurse into impl blocks to extract methods
                if let Some(body) = child.child_by_field_name("body") {
                    extract_rust(&body, source, exports, imports, definitions);
                }
            }
            "use_declaration" => {
                if let Some(imp) = parse_rust_use(&child, source) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
    }
}

fn child_has_visibility(node: &tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return true;
        }
    }
    false
}

/// Extract field names from a Rust struct body (field_declaration_list).
fn extract_rust_fields(
    body: &tree_sitter::Node,
    source: &[u8],
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "field_declaration" {
            if let Some(name) = child.child_by_field_name("name") {
                let name_str = node_text(&name, source);
                let line = child.start_position().row + 1;
                definitions.push(DefinitionEntry {
                    name: name_str,
                    kind: SymbolKind::Field,
                    line,
                    complexity: 0,
                });
            }
        }
    }
}

/// Extract variant names from a Rust enum body (enum_variant_list).
fn extract_rust_enum_variants(
    body: &tree_sitter::Node,
    source: &[u8],
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "enum_variant" {
            if let Some(name) = child.child_by_field_name("name") {
                let name_str = node_text(&name, source);
                let line = child.start_position().row + 1;
                definitions.push(DefinitionEntry {
                    name: name_str,
                    kind: SymbolKind::EnumVariant,
                    line,
                    complexity: 0,
                });
            }
        }
    }
}

fn parse_rust_use(node: &tree_sitter::Node, source: &[u8]) -> Option<ImportEntry> {
    let text = node.utf8_text(source).ok()?;
    // Simple parse: "use crate::foo::Bar;" or "use std::collections::HashMap;"
    let text = text.trim().trim_start_matches("use ").trim_end_matches(';');
    let parts: Vec<&str> = text.split("::").collect();
    if parts.len() >= 2 {
        let module = parts[..parts.len() - 1].join("::");
        let name = parts.last().unwrap_or(&"").to_string();
        Some(ImportEntry {
            from: module,
            names: vec![name],
        })
    } else {
        None
    }
}

// ─── TypeScript / JavaScript ────────────────────────────────────

fn extract_ts_js(
    root: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    _is_ts: bool,
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let cc = cyclomatic_complexity(&child, source, lang);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                }
            }
            "class_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Class,
                        line,
                        complexity: 1,
                    });
                }
                // Extract class methods and properties
                if let Some(body) = child.child_by_field_name("body") {
                    extract_ts_class_members(&body, source, lang, definitions);
                }
            }
            "interface_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Interface,
                        line,
                        complexity: 1,
                    });
                }
                // Extract interface properties
                if let Some(body) = child.child_by_field_name("body") {
                    extract_ts_interface_members(&body, source, definitions);
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::TypeAlias,
                        line,
                        complexity: 1,
                    });
                }
            }
            "enum_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Enum,
                        line,
                        complexity: 1,
                    });
                }
                // Extract enum members
                if let Some(body) = child.child_by_field_name("body") {
                    extract_ts_enum_members(&body, source, definitions);
                }
            }
            "export_statement" => {
                extract_ts_export(&child, source, lang, exports, definitions);
            }
            "import_statement" => {
                if let Some(imp) = parse_ts_import(&child, source) {
                    imports.push(imp);
                }
            }
            // const foo = function() {} or const foo = () => {}
            "lexical_declaration" | "variable_declaration" => {
                extract_js_var_decl(&child, source, lang, definitions);
            }
            // module.exports = ... or CJS require()
            "expression_statement" => {
                extract_js_expression(&child, source, exports, imports);
            }
            _ => {}
        }
    }
}

/// Extract definitions from const/let/var declarations with function values.
fn extract_js_var_decl(
    node: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name) = child.child_by_field_name("name") {
                if let Some(value) = child.child_by_field_name("value") {
                    let kind_str = value.kind();
                    // Arrow function or function expression
                    if kind_str == "arrow_function"
                        || kind_str == "function"
                        || kind_str == "function_expression"
                    {
                        let name_str = node_text(&name, source);
                        let line = child.start_position().row + 1;
                        let cc = cyclomatic_complexity(&value, source, lang);
                        definitions.push(DefinitionEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                            complexity: cc,
                        });
                    }
                }
            }
        }
    }
}

/// Extract module.exports and CJS require() from expression statements.
fn extract_js_expression(
    node: &tree_sitter::Node,
    source: &[u8],
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "assignment_expression" {
            // module.exports = { ... } or module.exports.foo = ...
            if let Some(left) = child.child_by_field_name("left") {
                let left_text = node_text(&left, source);
                if left_text.starts_with("module.exports") {
                    if let Some(right) = child.child_by_field_name("right") {
                        if right.kind() == "object" {
                            // module.exports = { foo, bar, baz }
                            let mut obj_cursor = right.walk();
                            for prop in right.children(&mut obj_cursor) {
                                if prop.kind() == "shorthand_property_identifier" {
                                    let name = node_text(&prop, source);
                                    let line = prop.start_position().row + 1;
                                    exports.push(SymbolEntry {
                                        name,
                                        kind: SymbolKind::Function,
                                        line,
                                    });
                                } else if prop.kind() == "pair" {
                                    if let Some(key) = prop.child_by_field_name("key") {
                                        let name = node_text(&key, source);
                                        let line = prop.start_position().row + 1;
                                        exports.push(SymbolEntry {
                                            name,
                                            kind: SymbolKind::Function,
                                            line,
                                        });
                                    }
                                }
                            }
                        } else if right.kind() == "identifier" {
                            // module.exports = myFunc
                            let name = node_text(&right, source);
                            let line = right.start_position().row + 1;
                            exports.push(SymbolEntry {
                                name,
                                kind: SymbolKind::Function,
                                line,
                            });
                        }
                    }
                }
            }
        }
        // CJS require: const foo = require('bar')
        if child.kind() == "call_expression" {
            if let Some(func) = child.child_by_field_name("function") {
                if node_text(&func, source) == "require" {
                    if let Some(args) = child.child_by_field_name("arguments") {
                        let mut arg_cursor = args.walk();
                        for arg in args.children(&mut arg_cursor) {
                            if arg.kind() == "string" || arg.kind() == "string_literal" {
                                let path = node_text(&arg, source)
                                    .trim_matches('"')
                                    .trim_matches('\'')
                                    .to_string();
                                imports.push(ImportEntry {
                                    from: path,
                                    names: vec![],
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

fn extract_ts_export(
    node: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    exports: &mut Vec<SymbolEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let cc = cyclomatic_complexity(&child, source, lang);
                    exports.push(SymbolEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                    });
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                }
            }
            "class_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    exports.push(SymbolEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Class,
                        line,
                    });
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::Class,
                        line,
                        complexity: 1,
                    });
                }
                // Extract class members
                if let Some(body) = child.child_by_field_name("body") {
                    extract_ts_class_members(&body, source, lang, definitions);
                }
            }
            "interface_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    exports.push(SymbolEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Interface,
                        line,
                    });
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::Interface,
                        line,
                        complexity: 1,
                    });
                }
                // Extract interface members
                if let Some(body) = child.child_by_field_name("body") {
                    extract_ts_interface_members(&body, source, definitions);
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    exports.push(SymbolEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::TypeAlias,
                        line,
                    });
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::TypeAlias,
                        line,
                        complexity: 1,
                    });
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                // export const foo = ... (detect if value is arrow/function)
                let mut inner = child.walk();
                for decl in child.children(&mut inner) {
                    if decl.kind() == "variable_declarator" {
                        if let Some(name) = decl.child_by_field_name("name") {
                            let name_str = node_text(&name, source);
                            let line = decl.start_position().row + 1;
                            // Check if value is a function/arrow function
                            let (kind, cc) = if let Some(value) = decl.child_by_field_name("value")
                            {
                                match value.kind() {
                                    "arrow_function" | "function" | "function_expression" => (
                                        SymbolKind::Function,
                                        cyclomatic_complexity(&value, source, lang),
                                    ),
                                    _ => (SymbolKind::Constant, 1),
                                }
                            } else {
                                (SymbolKind::Constant, 1)
                            };
                            exports.push(SymbolEntry {
                                name: name_str.clone(),
                                kind: kind.clone(),
                                line,
                            });
                            definitions.push(DefinitionEntry {
                                name: name_str,
                                kind,
                                line,
                                complexity: cc,
                            });
                        }
                    }
                }
            }
            // Re-exports: export { Queue, Worker } from './queue'
            "export_clause" => {
                let mut inner = child.walk();
                for spec in child.children(&mut inner) {
                    if spec.kind() == "export_specifier" {
                        if let Some(name) = spec.child_by_field_name("name") {
                            let name_str = node_text(&name, source);
                            let line = spec.start_position().row + 1;
                            exports.push(SymbolEntry {
                                name: name_str,
                                kind: SymbolKind::Function, // generic - could be any type
                                line,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_ts_import(node: &tree_sitter::Node, source: &[u8]) -> Option<ImportEntry> {
    let mut from = String::new();
    let mut names = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string" || child.kind() == "string_literal" {
            from = node_text(&child, source)
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
        }
        if child.kind() == "import_clause" {
            let mut inner = child.walk();
            for c in child.children(&mut inner) {
                match c.kind() {
                    "identifier" => names.push(node_text(&c, source)),
                    "named_imports" => {
                        let mut imp_cursor = c.walk();
                        for spec in c.children(&mut imp_cursor) {
                            if spec.kind() == "import_specifier" {
                                if let Some(name_node) = spec.child_by_field_name("name") {
                                    names.push(node_text(&name_node, source));
                                }
                            }
                        }
                    }
                    "namespace_import" => {
                        names.push("*".to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    if from.is_empty() {
        return None;
    }

    Some(ImportEntry { from, names })
}

/// Extract class methods and properties from a TS/JS class body.
fn extract_ts_class_members(
    body: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method_definition" => {
                if let Some(name_str) = find_child_text(&child, "property_identifier", source) {
                    // Skip constructor
                    if name_str != "constructor" {
                        let line = child.start_position().row + 1;
                        let cc = cyclomatic_complexity(&child, source, lang);
                        definitions.push(DefinitionEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                            complexity: cc,
                        });
                    }
                }
            }
            "public_field_definition" | "property_definition" => {
                if let Some(name_str) = find_child_text(&child, "property_identifier", source) {
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::Property,
                        line: child.start_position().row + 1,
                        complexity: 0,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Extract property signatures from a TS interface body.
fn extract_ts_interface_members(
    body: &tree_sitter::Node,
    source: &[u8],
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "property_signature" => {
                if let Some(name_str) = find_child_text(&child, "property_identifier", source) {
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::Property,
                        line: child.start_position().row + 1,
                        complexity: 0,
                    });
                }
            }
            "method_signature" => {
                if let Some(name_str) = find_child_text(&child, "property_identifier", source) {
                    definitions.push(DefinitionEntry {
                        name: name_str,
                        kind: SymbolKind::Function,
                        line: child.start_position().row + 1,
                        complexity: 0,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Find the first child of a given kind and return its text.
fn find_child_text(node: &tree_sitter::Node, kind: &str, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(node_text(&child, source));
        }
    }
    None
}

/// Extract enum member names from a TS enum body.
fn extract_ts_enum_members(
    body: &tree_sitter::Node,
    source: &[u8],
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "enum_assignment" || child.kind() == "property_identifier" {
            let name = node_text(&child, source);
            if !name.is_empty() && name != "," {
                definitions.push(DefinitionEntry {
                    name,
                    kind: SymbolKind::EnumVariant,
                    line: child.start_position().row + 1,
                    complexity: 0,
                });
            }
        }
    }
}

// ─── Python ─────────────────────────────────────────────────────

fn extract_python(
    root: &tree_sitter::Node,
    source: &[u8],
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_public = !name_str.starts_with('_');
                    let cc = cyclomatic_complexity(&child, source, Language::Python);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                    if is_public {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                        });
                    }
                }
            }
            "class_definition" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_public = !name_str.starts_with('_');
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Class,
                        line,
                        complexity: 1,
                    });
                    if is_public {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Class,
                            line,
                        });
                    }
                }
                // Extract class methods
                if let Some(body) = child.child_by_field_name("body") {
                    let mut body_cursor = body.walk();
                    for member in body.children(&mut body_cursor) {
                        if member.kind() == "function_definition" {
                            if let Some(name) = member.child_by_field_name("name") {
                                let name_str = node_text(&name, source);
                                if name_str != "__init__" && !name_str.starts_with('_') {
                                    let cc =
                                        cyclomatic_complexity(&member, source, Language::Python);
                                    definitions.push(DefinitionEntry {
                                        name: name_str,
                                        kind: SymbolKind::Function,
                                        line: member.start_position().row + 1,
                                        complexity: cc,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "import_statement" => {
                let text = node_text(&child, source);
                let module = text.trim_start_matches("import ").trim().to_string();
                imports.push(ImportEntry {
                    from: module,
                    names: vec![],
                });
            }
            "import_from_statement" => {
                if let Some(imp) = parse_python_import_from(&child, source) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
    }
}

fn parse_python_import_from(node: &tree_sitter::Node, source: &[u8]) -> Option<ImportEntry> {
    let mut module = String::new();
    let mut names = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" | "relative_import" => {
                if module.is_empty() {
                    module = node_text(&child, source);
                } else {
                    names.push(node_text(&child, source));
                }
            }
            "import_prefix" => {
                module = node_text(&child, source);
            }
            "wildcard_import" => {
                names.push("*".to_string());
            }
            _ => {}
        }
    }

    if module.is_empty() {
        return None;
    }

    Some(ImportEntry {
        from: module,
        names,
    })
}

// ─── Go ─────────────────────────────────────────────────────────

fn extract_go(
    root: &tree_sitter::Node,
    source: &[u8],
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_exported = name_str.chars().next().is_some_and(|c| c.is_uppercase());
                    let cc = cyclomatic_complexity(&child, source, Language::Go);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                    if is_exported {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                        });
                    }
                }
            }
            "method_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let cc = cyclomatic_complexity(&child, source, Language::Go);
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                    if name_str.chars().next().is_some_and(|c| c.is_uppercase()) {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                        });
                    }
                }
            }
            "type_declaration" => {
                let mut inner = child.walk();
                for spec in child.children(&mut inner) {
                    if spec.kind() == "type_spec" {
                        if let Some(name) = spec.child_by_field_name("name") {
                            let name_str = node_text(&name, source);
                            let line = spec.start_position().row + 1;
                            let is_exported =
                                name_str.chars().next().is_some_and(|c| c.is_uppercase());

                            // Determine kind from the type body
                            let type_node = spec.child_by_field_name("type");
                            let kind = if let Some(ref tn) = type_node {
                                match tn.kind() {
                                    "struct_type" => SymbolKind::Struct,
                                    "interface_type" => SymbolKind::Interface,
                                    _ => SymbolKind::TypeAlias,
                                }
                            } else {
                                SymbolKind::TypeAlias
                            };

                            definitions.push(DefinitionEntry {
                                name: name_str.clone(),
                                kind: kind.clone(),
                                line,
                                complexity: 1,
                            });
                            if is_exported {
                                exports.push(SymbolEntry {
                                    name: name_str,
                                    kind,
                                    line,
                                });
                            }

                            // Extract struct fields and interface methods
                            if let Some(ref tn) = type_node {
                                extract_go_type_members(tn, source, definitions);
                            }
                        }
                    }
                }
            }
            "import_declaration" => {
                parse_go_imports(&child, source, imports);
            }
            _ => {}
        }
    }
}

fn parse_go_imports(node: &tree_sitter::Node, source: &[u8], imports: &mut Vec<ImportEntry>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_spec" || child.kind() == "import_spec_list" {
            let mut inner = child.walk();
            for spec in child.children(&mut inner) {
                if spec.kind() == "import_spec" {
                    if let Some(path_node) = spec.child_by_field_name("path") {
                        let path = node_text(&path_node, source).trim_matches('"').to_string();
                        imports.push(ImportEntry {
                            from: path,
                            names: vec![],
                        });
                    }
                } else if spec.kind() == "interpreted_string_literal" {
                    let path = node_text(&spec, source).trim_matches('"').to_string();
                    imports.push(ImportEntry {
                        from: path,
                        names: vec![],
                    });
                }
            }
        }
        // Single import without parens
        if child.kind() == "import_spec" {
            if let Some(path_node) = child.child_by_field_name("path") {
                let path = node_text(&path_node, source).trim_matches('"').to_string();
                imports.push(ImportEntry {
                    from: path,
                    names: vec![],
                });
            }
        }
    }
}

/// Extract struct fields and interface method signatures from a Go type node.
fn extract_go_type_members(
    type_node: &tree_sitter::Node,
    source: &[u8],
    definitions: &mut Vec<DefinitionEntry>,
) {
    match type_node.kind() {
        "struct_type" => {
            if let Some(fields) = type_node.child_by_field_name("fields") {
                let mut cursor = fields.walk();
                for child in fields.children(&mut cursor) {
                    if child.kind() == "field_declaration" {
                        if let Some(name) = child.child_by_field_name("name") {
                            definitions.push(DefinitionEntry {
                                name: node_text(&name, source),
                                kind: SymbolKind::Field,
                                line: child.start_position().row + 1,
                                complexity: 0,
                            });
                        }
                    }
                }
            }
        }
        "interface_type" => {
            let mut cursor = type_node.walk();
            for child in type_node.children(&mut cursor) {
                if child.kind() == "method_spec" {
                    if let Some(name) = child.child_by_field_name("name") {
                        definitions.push(DefinitionEntry {
                            name: node_text(&name, source),
                            kind: SymbolKind::Function,
                            line: child.start_position().row + 1,
                            complexity: 0,
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

// ─── Java ───────────────────────────────────────────────────────

fn extract_java(
    root: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    exports: &mut Vec<SymbolEntry>,
    imports: &mut Vec<ImportEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "class_declaration" => {
                extract_java_class(&child, source, lang, exports, definitions);
            }
            "interface_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_public = has_modifier(&child, source, "public");
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Interface,
                        line,
                        complexity: 1,
                    });
                    if is_public {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Interface,
                            line,
                        });
                    }
                }
            }
            "enum_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let is_public = has_modifier(&child, source, "public");
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Enum,
                        line,
                        complexity: 1,
                    });
                    if is_public {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Enum,
                            line,
                        });
                    }
                }
            }
            "import_declaration" => {
                let text = node_text(&child, source);
                let path = text
                    .trim_start_matches("import ")
                    .trim_start_matches("static ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                let parts: Vec<&str> = path.rsplitn(2, '.').collect();
                if parts.len() == 2 {
                    imports.push(ImportEntry {
                        from: parts[1].to_string(),
                        names: vec![parts[0].to_string()],
                    });
                }
            }
            _ => {}
        }
    }
}

fn extract_java_class(
    node: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
    exports: &mut Vec<SymbolEntry>,
    definitions: &mut Vec<DefinitionEntry>,
) {
    if let Some(name) = node.child_by_field_name("name") {
        let name_str = node_text(&name, source);
        let line = node.start_position().row + 1;
        let is_public = has_modifier(node, source, "public");
        definitions.push(DefinitionEntry {
            name: name_str.clone(),
            kind: SymbolKind::Class,
            line,
            complexity: 1,
        });
        if is_public {
            exports.push(SymbolEntry {
                name: name_str,
                kind: SymbolKind::Class,
                line,
            });
        }
    }

    // Extract methods from class body
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "method_declaration" {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = node_text(&name, source);
                    let line = child.start_position().row + 1;
                    let cc = cyclomatic_complexity(&child, source, lang);
                    let is_public = has_modifier(&child, source, "public");
                    definitions.push(DefinitionEntry {
                        name: name_str.clone(),
                        kind: SymbolKind::Function,
                        line,
                        complexity: cc,
                    });
                    if is_public {
                        exports.push(SymbolEntry {
                            name: name_str,
                            kind: SymbolKind::Function,
                            line,
                        });
                    }
                }
            }
            // Extract fields
            if child.kind() == "field_declaration" {
                if let Some(declarator) = child.child_by_field_name("declarator") {
                    if let Some(name) = declarator.child_by_field_name("name") {
                        let name_str = node_text(&name, source);
                        definitions.push(DefinitionEntry {
                            name: name_str,
                            kind: SymbolKind::Field,
                            line: child.start_position().row + 1,
                            complexity: 0,
                        });
                    }
                }
            }
        }
    }
}

fn has_modifier(node: &tree_sitter::Node, source: &[u8], modifier: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" || child.kind() == "modifier" {
            let text = node_text(&child, source);
            if text.contains(modifier) {
                return true;
            }
        }
    }
    false
}

// ─── Helpers ────────────────────────────────────────────────────

fn node_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_function() {
        let source = b"pub fn validate(input: &str) -> bool { true }";
        let syms = extract_file_symbols(source, Language::Rust).unwrap();
        assert_eq!(syms.definitions.len(), 1);
        assert_eq!(syms.definitions[0].name, "validate");
        assert_eq!(syms.definitions[0].kind, SymbolKind::Function);
        assert_eq!(syms.exports.len(), 1);
        assert_eq!(syms.exports[0].name, "validate");
    }

    #[test]
    fn test_rust_private_function() {
        let source = b"fn internal() {}";
        let syms = extract_file_symbols(source, Language::Rust).unwrap();
        assert_eq!(syms.definitions.len(), 1);
        assert_eq!(syms.exports.len(), 0);
    }

    #[test]
    fn test_rust_struct_and_enum() {
        let source =
            b"pub struct Config { pub name: String }\npub enum Status { Active, Inactive }";
        let syms = extract_file_symbols(source, Language::Rust).unwrap();
        // Config (struct) + name (field) + Status (enum) + Active + Inactive (variants)
        assert_eq!(syms.definitions.len(), 5);
        assert_eq!(syms.exports.len(), 2);
        assert_eq!(syms.definitions[0].kind, SymbolKind::Struct);
        assert_eq!(syms.definitions[1].kind, SymbolKind::Field);
        assert_eq!(syms.definitions[1].name, "name");
        assert_eq!(syms.definitions[2].kind, SymbolKind::Enum);
        assert!(
            syms.definitions
                .iter()
                .any(|d| d.name == "Active" && d.kind == SymbolKind::EnumVariant)
        );
        assert!(
            syms.definitions
                .iter()
                .any(|d| d.name == "Inactive" && d.kind == SymbolKind::EnumVariant)
        );
    }

    #[test]
    fn test_rust_use() {
        let source = b"use std::collections::HashMap;";
        let syms = extract_file_symbols(source, Language::Rust).unwrap();
        assert_eq!(syms.imports.len(), 1);
        assert_eq!(syms.imports[0].from, "std::collections");
        assert_eq!(syms.imports[0].names, vec!["HashMap"]);
    }

    #[test]
    fn test_rust_mod_decl_emits_resolvable_import() {
        // `mod foo;` (no body) should produce a sentinel import that
        // the post-extraction resolver picks up.
        let source = b"mod foo;\nfn main() {}\n";
        let syms = extract_file_symbols(source, Language::Rust).unwrap();
        let mod_imports: Vec<_> = syms
            .imports
            .iter()
            .filter(|i| i.from.starts_with("__mod_decl__::"))
            .collect();
        assert_eq!(
            mod_imports.len(),
            1,
            "expected one mod-decl sentinel; got {:?}",
            syms.imports
        );
        assert_eq!(mod_imports[0].from, "__mod_decl__::foo");
    }

    #[test]
    fn test_rust_mod_with_body_does_not_emit_decl_import() {
        // Inline `mod tests { ... }` is intra-file — no resolver edge.
        let source = b"#[cfg(test)]\nmod tests {\n    fn x() {}\n}\n";
        let syms = extract_file_symbols(source, Language::Rust).unwrap();
        let mod_imports: Vec<_> = syms
            .imports
            .iter()
            .filter(|i| i.from.starts_with("__mod_decl__::"))
            .collect();
        assert!(
            mod_imports.is_empty(),
            "inline mod should not emit decl import"
        );
    }

    #[test]
    fn resolve_rust_mod_decl_prefers_sibling_rs_over_mod_rs() {
        let mut known = std::collections::HashSet::new();
        known.insert("crates/x/src/foo.rs".to_string());
        known.insert("crates/x/src/foo/mod.rs".to_string());
        let r = resolve_rust_mod_decl("crates/x/src/lib.rs", "foo", &known);
        assert_eq!(r.as_deref(), Some("crates/x/src/foo.rs"));
    }

    #[test]
    fn resolve_rust_mod_decl_falls_back_to_mod_rs() {
        let mut known = std::collections::HashSet::new();
        known.insert("crates/x/src/bar/mod.rs".to_string());
        let r = resolve_rust_mod_decl("crates/x/src/lib.rs", "bar", &known);
        assert_eq!(r.as_deref(), Some("crates/x/src/bar/mod.rs"));
    }

    #[test]
    fn resolve_rust_mod_decl_returns_none_when_target_missing() {
        let known = std::collections::HashSet::new();
        let r = resolve_rust_mod_decl("crates/x/src/lib.rs", "ghost", &known);
        assert!(r.is_none());
    }

    #[test]
    fn resolve_rust_mod_decl_handles_root_importer() {
        let mut known = std::collections::HashSet::new();
        known.insert("foo.rs".to_string());
        let r = resolve_rust_mod_decl("lib.rs", "foo", &known);
        assert_eq!(r.as_deref(), Some("foo.rs"));
    }

    #[test]
    fn resolve_rust_mod_decl_handles_2018_nested_layout() {
        // Importer `src/config.rs` with `mod builder;` → 2018 edition
        // resolves into `src/config/builder.rs`, NOT `src/builder.rs`.
        let mut known = std::collections::HashSet::new();
        known.insert("crates/x/src/config/builder.rs".to_string());
        // Also have a sibling `src/builder.rs` to verify nested wins.
        known.insert("crates/x/src/builder.rs".to_string());
        let r = resolve_rust_mod_decl("crates/x/src/config.rs", "builder", &known);
        assert_eq!(
            r.as_deref(),
            Some("crates/x/src/config/builder.rs"),
            "2018 nested layout should win over sibling"
        );
    }

    #[test]
    fn resolve_rust_mod_decl_falls_back_to_sibling_when_no_nested() {
        // Importer `src/config.rs` but NO `src/config/` subdir — fall
        // back to sibling `src/something.rs`.
        let mut known = std::collections::HashSet::new();
        known.insert("crates/x/src/builder.rs".to_string());
        let r = resolve_rust_mod_decl("crates/x/src/config.rs", "builder", &known);
        assert_eq!(r.as_deref(), Some("crates/x/src/builder.rs"));
    }

    // ── TS/JS relative import resolver ───────────

    #[test]
    fn resolve_ts_js_dot_slash_finds_sibling_ts() {
        let mut known = std::collections::HashSet::new();
        known.insert("editors/vscode/src/foo.ts".to_string());
        let r = resolve_ts_js_relative("editors/vscode/src/main.ts", "./foo", &known);
        assert_eq!(r.as_deref(), Some("editors/vscode/src/foo.ts"));
    }

    #[test]
    fn resolve_ts_js_dot_dot_walks_up() {
        let mut known = std::collections::HashSet::new();
        known.insert("editors/vscode/src/util/helper.ts".to_string());
        let r = resolve_ts_js_relative(
            "editors/vscode/src/feature/index.ts",
            "../util/helper",
            &known,
        );
        assert_eq!(r.as_deref(), Some("editors/vscode/src/util/helper.ts"));
    }

    #[test]
    fn resolve_ts_js_falls_back_to_index() {
        let mut known = std::collections::HashSet::new();
        known.insert("src/util/index.ts".to_string());
        let r = resolve_ts_js_relative("src/main.ts", "./util", &known);
        assert_eq!(r.as_deref(), Some("src/util/index.ts"));
    }

    #[test]
    fn resolve_ts_js_prefers_ts_over_js() {
        let mut known = std::collections::HashSet::new();
        known.insert("src/foo.ts".to_string());
        known.insert("src/foo.js".to_string());
        let r = resolve_ts_js_relative("src/main.ts", "./foo", &known);
        assert_eq!(r.as_deref(), Some("src/foo.ts"));
    }

    #[test]
    fn resolve_ts_js_returns_none_for_unknown() {
        let known = std::collections::HashSet::new();
        let r = resolve_ts_js_relative("src/main.ts", "./ghost", &known);
        assert!(r.is_none());
    }

    // ── Python import resolver ───────────

    #[test]
    fn resolve_python_relative_dot_imports_sibling() {
        let mut known = std::collections::HashSet::new();
        known.insert("scripts/utils.py".to_string());
        let r = resolve_python_import("scripts/main.py", ".utils", &known);
        assert_eq!(r.as_deref(), Some("scripts/utils.py"));
    }

    #[test]
    fn resolve_python_relative_double_dot_walks_up() {
        let mut known = std::collections::HashSet::new();
        known.insert("pkg/util/helper.py".to_string());
        let r = resolve_python_import("pkg/feature/main.py", "..util.helper", &known);
        assert_eq!(r.as_deref(), Some("pkg/util/helper.py"));
    }

    #[test]
    fn resolve_python_dotted_module_finds_nested() {
        let mut known = std::collections::HashSet::new();
        known.insert("scripts/lib/foo.py".to_string());
        let r = resolve_python_import("scripts/main.py", "lib.foo", &known);
        assert_eq!(r.as_deref(), Some("scripts/lib/foo.py"));
    }

    #[test]
    fn resolve_python_falls_back_to_init() {
        let mut known = std::collections::HashSet::new();
        known.insert("pkg/sub/__init__.py".to_string());
        let r = resolve_python_import("pkg/main.py", ".sub", &known);
        assert_eq!(r.as_deref(), Some("pkg/sub/__init__.py"));
    }

    #[test]
    fn resolve_python_returns_none_for_stdlib_name() {
        let known = std::collections::HashSet::new();
        let r = resolve_python_import("scripts/main.py", "os", &known);
        assert!(r.is_none());
    }

    #[test]
    fn normalize_path_collapses_dot_dot() {
        assert_eq!(normalize_path("a/b/../c"), "a/c");
        assert_eq!(normalize_path("./a/./b"), "a/b");
        assert_eq!(normalize_path("a/b/c/../../d"), "a/d");
    }

    #[test]
    fn resolve_rust_mod_decl_lib_rs_uses_sibling_only() {
        // `src/lib.rs` with `mod foo;` should NOT try `src/lib/foo.rs`
        // (that would be wrong — lib.rs is a root, not a 2018-nested
        // parent). Should only try `src/foo.rs` and `src/foo/mod.rs`.
        let mut known = std::collections::HashSet::new();
        known.insert("crates/x/src/lib/foo.rs".to_string()); // bait
        known.insert("crates/x/src/foo.rs".to_string()); // correct
        let r = resolve_rust_mod_decl("crates/x/src/lib.rs", "foo", &known);
        assert_eq!(r.as_deref(), Some("crates/x/src/foo.rs"));
    }

    #[test]
    fn test_ts_export_function() {
        let source = b"export function greet(name: string): void { console.log(name); }";
        let syms = extract_file_symbols(source, Language::TypeScript).unwrap();
        assert_eq!(syms.exports.len(), 1);
        assert_eq!(syms.exports[0].name, "greet");
        assert_eq!(syms.definitions.len(), 1);
    }

    #[test]
    fn test_ts_class_members() {
        let source = b"export class Queue {\n  async add(data: any): Promise<any> { return null; }\n  close(): void {}\n  get count(): number { return 0; }\n}\nexport interface JobOptions {\n  delay?: number;\n  attempts?: number;\n}";

        let syms = extract_file_symbols(source, Language::TypeScript).unwrap();
        // Should find: Queue (class) + add, close, count (methods) + delay, attempts (interface props) + JobOptions
        assert!(
            syms.definitions.iter().any(|d| d.name == "Queue"),
            "missing Queue"
        );
        assert!(
            syms.definitions.iter().any(|d| d.name == "add"),
            "missing add method"
        );
        assert!(
            syms.definitions.iter().any(|d| d.name == "close"),
            "missing close method"
        );
        assert!(
            syms.definitions.iter().any(|d| d.name == "JobOptions"),
            "missing JobOptions"
        );
        assert!(
            syms.definitions.iter().any(|d| d.name == "delay"),
            "missing delay prop"
        );
        assert!(
            syms.definitions.iter().any(|d| d.name == "attempts"),
            "missing attempts prop"
        );
    }

    #[test]
    fn test_ts_import() {
        let source = b"import { foo, bar } from './utils';";
        let syms = extract_file_symbols(source, Language::TypeScript).unwrap();
        assert_eq!(syms.imports.len(), 1);
        assert_eq!(syms.imports[0].from, "./utils");
        assert!(syms.imports[0].names.contains(&"foo".to_string()));
        assert!(syms.imports[0].names.contains(&"bar".to_string()));
    }

    #[test]
    fn test_python_function() {
        let source = b"def greet(name):\n    print(name)\n";
        let syms = extract_file_symbols(source, Language::Python).unwrap();
        assert_eq!(syms.definitions.len(), 1);
        assert_eq!(syms.definitions[0].name, "greet");
        assert_eq!(syms.exports.len(), 1); // public (no underscore)
    }

    #[test]
    fn test_python_private() {
        let source = b"def _internal():\n    pass\n";
        let syms = extract_file_symbols(source, Language::Python).unwrap();
        assert_eq!(syms.definitions.len(), 1);
        assert_eq!(syms.exports.len(), 0);
    }

    #[test]
    fn test_go_exported_function() {
        let source = b"package main\nfunc Validate(s string) bool { return true }\n";
        let syms = extract_file_symbols(source, Language::Go).unwrap();
        assert_eq!(syms.definitions.len(), 1);
        assert_eq!(syms.exports.len(), 1);
        assert_eq!(syms.exports[0].name, "Validate");
    }

    #[test]
    fn test_go_unexported_function() {
        let source = b"package main\nfunc validate(s string) bool { return true }\n";
        let syms = extract_file_symbols(source, Language::Go).unwrap();
        assert_eq!(syms.definitions.len(), 1);
        assert_eq!(syms.exports.len(), 0);
    }

    #[test]
    fn test_java_class_and_method() {
        let source = b"public class Main { public void run() {} private void helper() {} }";
        let syms = extract_file_symbols(source, Language::Java).unwrap();
        // Should find: Main (class), run (method), helper (method)
        assert!(syms.definitions.len() >= 2);
        assert!(syms.exports.iter().any(|e| e.name == "Main"));
    }
}
