//! Walk source files and extract symbols using tree-sitter.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;

use analyzer_core::types::{DefinitionEntry, FileSymbols, ImportEntry, SymbolEntry, SymbolKind};
use analyzer_core::walk;

use crate::complexity::cyclomatic_complexity;
use crate::parser::{detect_language, parse_source, Language};

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
    let mut import_graph: HashMap<String, Vec<String>> = HashMap::new();

    for (path, syms) in results {
        // Build import graph from import entries
        let imports: Vec<String> = syms.imports.iter().map(|i| i.from.clone()).collect();
        if !imports.is_empty() {
            import_graph.insert(path.clone(), imports);
        }
        symbols_map.insert(path, syms);
    }

    Ok((symbols_map, import_graph))
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
                            name: name_str,
                            kind: SymbolKind::Module,
                            line,
                        });
                    }
                }
                // Recurse into mod body (e.g., #[cfg(test)] mod tests { ... })
                if let Some(body) = child.child_by_field_name("body") {
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
        assert!(syms
            .definitions
            .iter()
            .any(|d| d.name == "Active" && d.kind == SymbolKind::EnumVariant));
        assert!(syms
            .definitions
            .iter()
            .any(|d| d.name == "Inactive" && d.kind == SymbolKind::EnumVariant));
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
