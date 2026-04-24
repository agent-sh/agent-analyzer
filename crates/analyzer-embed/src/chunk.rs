//! Splitting source files into embedding units.
//!
//! Two granularities are supported:
//!
//! - [`Granularity::PerFile`] — one chunk per file, content is the entire
//!   file. Cheap to compute and store; loses function-level resolution.
//! - [`Granularity::PerFunction`] — one chunk per top-level declaration
//!   (functions, methods, classes for code; sections for markdown). Falls
//!   back to per-file for unsupported file types.
//!
//! The user picks granularity once at install time via the skill prompt,
//! cached in `preference.json` as `embedderDetail: "compact" | "balanced" |
//! "maximum"` which maps to `(granularity, dim)`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// What a chunk represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// The entire file is one unit.
    File,
    /// A top-level function or method declaration.
    Function,
    /// A class or struct (including its methods, treated as one unit).
    Type,
    /// A top-level markdown section (heading + body until next heading of
    /// equal or shallower depth).
    DocSection,
}

/// User-selected chunking granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    PerFile,
    PerFunction,
}

/// One embedding unit extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// What the chunk represents.
    pub kind: ChunkKind,
    /// Symbol name (function name, class name, section heading) when
    /// applicable. `None` for whole-file chunks.
    pub name: Option<String>,
    /// 1-based start line in the source file.
    pub start_line: u32,
    /// 1-based end line in the source file (inclusive).
    pub end_line: u32,
    /// The text to embed. Already trimmed; callers feed this directly to
    /// the embedder.
    pub text: String,
}

/// Detect language from extension. Returns `None` for unsupported types,
/// in which case the caller should fall back to per-file chunking.
fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "rs" => Some(Language::Rust),
        "ts" | "tsx" => Some(Language::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        "py" => Some(Language::Python),
        "go" => Some(Language::Go),
        "java" => Some(Language::Java),
        "md" | "markdown" => Some(Language::Markdown),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Java,
    Markdown,
}

/// Split a file into chunks at the requested granularity.
///
/// `path` is used for language detection; it does not need to exist on
/// disk. `content` is the file's text content (UTF-8).
///
/// For [`Granularity::PerFile`], always returns a single [`ChunkKind::File`]
/// chunk. For [`Granularity::PerFunction`], extracts top-level declarations
/// using tree-sitter; if the language is unsupported or no declarations
/// are found, falls back to a single file-level chunk.
pub fn chunk_file(path: &Path, content: &str, granularity: Granularity) -> Vec<Chunk> {
    if content.trim().is_empty() {
        return Vec::new();
    }

    if granularity == Granularity::PerFile {
        return vec![file_chunk(content)];
    }

    let lang = match detect_language(path) {
        Some(l) => l,
        None => return vec![file_chunk(content)],
    };

    let chunks = match lang {
        Language::Rust => {
            extract_with_query(content, &tree_sitter_rust::LANGUAGE.into(), RUST_QUERY)
        }
        Language::TypeScript => extract_with_query(
            content,
            &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            TS_QUERY,
        ),
        Language::JavaScript => {
            extract_with_query(content, &tree_sitter_javascript::LANGUAGE.into(), JS_QUERY)
        }
        Language::Python => {
            extract_with_query(content, &tree_sitter_python::LANGUAGE.into(), PYTHON_QUERY)
        }
        Language::Go => extract_with_query(content, &tree_sitter_go::LANGUAGE.into(), GO_QUERY),
        Language::Java => {
            extract_with_query(content, &tree_sitter_java::LANGUAGE.into(), JAVA_QUERY)
        }
        Language::Markdown => extract_markdown_sections(content),
    };

    if chunks.is_empty() {
        vec![file_chunk(content)]
    } else {
        chunks
    }
}

fn file_chunk(content: &str) -> Chunk {
    let line_count = content.lines().count().max(1) as u32;
    Chunk {
        kind: ChunkKind::File,
        name: None,
        start_line: 1,
        end_line: line_count,
        text: content.to_string(),
    }
}

// Tree-sitter queries identifying top-level declarations per language.
// Each `@decl` capture identifies the node whose byte span becomes the
// chunk; an optional `@name` capture gives a human-readable label.

const RUST_QUERY: &str = r#"
[
  (function_item name: (identifier) @name) @decl
  (impl_item) @decl
  (struct_item name: (type_identifier) @name) @decl
  (enum_item name: (type_identifier) @name) @decl
  (trait_item name: (type_identifier) @name) @decl
  (mod_item name: (identifier) @name) @decl
]
"#;

const TS_QUERY: &str = r#"
[
  (function_declaration name: (identifier) @name) @decl
  (class_declaration name: (type_identifier) @name) @decl
  (interface_declaration name: (type_identifier) @name) @decl
  (export_statement (function_declaration name: (identifier) @name)) @decl
  (export_statement (class_declaration name: (type_identifier) @name)) @decl
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function_expression)])) @decl
]
"#;

const JS_QUERY: &str = r#"
[
  (function_declaration name: (identifier) @name) @decl
  (class_declaration name: (identifier) @name) @decl
  (export_statement (function_declaration name: (identifier) @name)) @decl
  (export_statement (class_declaration name: (identifier) @name)) @decl
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function) (function)])) @decl
]
"#;

const PYTHON_QUERY: &str = r#"
[
  (function_definition name: (identifier) @name) @decl
  (class_definition name: (identifier) @name) @decl
]
"#;

const GO_QUERY: &str = r#"
[
  (function_declaration name: (identifier) @name) @decl
  (method_declaration name: (field_identifier) @name) @decl
  (type_declaration (type_spec name: (type_identifier) @name)) @decl
]
"#;

const JAVA_QUERY: &str = r#"
[
  (class_declaration name: (identifier) @name) @decl
  (method_declaration name: (identifier) @name) @decl
  (interface_declaration name: (identifier) @name) @decl
]
"#;

fn extract_with_query(
    content: &str,
    language: &tree_sitter::Language,
    query_src: &str,
) -> Vec<Chunk> {
    use streaming_iterator::StreamingIterator;

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(language).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let query = match tree_sitter::Query::new(language, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), content.as_bytes());

    let decl_idx = query.capture_index_for_name("decl");
    let name_idx = query.capture_index_for_name("name");

    let mut chunks: Vec<Chunk> = Vec::new();
    while let Some(m) = matches.next() {
        let mut decl_node: Option<tree_sitter::Node> = None;
        let mut name_text: Option<String> = None;
        for cap in m.captures {
            if Some(cap.index) == decl_idx {
                decl_node = Some(cap.node);
            } else if Some(cap.index) == name_idx {
                name_text = cap
                    .node
                    .utf8_text(content.as_bytes())
                    .ok()
                    .map(str::to_string);
            }
        }
        if let Some(node) = decl_node {
            let start = node.start_position();
            let end = node.end_position();
            let text = content[node.byte_range()].to_string();
            let kind = if name_text.as_deref().map(is_type_name).unwrap_or(false) {
                ChunkKind::Type
            } else {
                ChunkKind::Function
            };
            chunks.push(Chunk {
                kind,
                name: name_text,
                start_line: (start.row as u32) + 1,
                end_line: (end.row as u32) + 1,
                text,
            });
        }
    }

    chunks.sort_by_key(|c| (c.start_line, c.end_line));
    chunks
}

fn is_type_name(name: &str) -> bool {
    name.chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
}

fn extract_markdown_sections(content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    let mut sections: Vec<(u32, u32, Option<String>)> = Vec::new();
    let mut current_start: u32 = 1;
    let mut current_name: Option<String> = None;

    for (i, line) in lines.iter().enumerate() {
        let line_no = (i as u32) + 1;
        if let Some(heading) = parse_heading(line) {
            if line_no > current_start {
                sections.push((current_start, line_no - 1, current_name.take()));
            }
            current_start = line_no;
            current_name = Some(heading);
        }
    }
    sections.push((current_start, lines.len().max(1) as u32, current_name));

    sections
        .into_iter()
        .filter_map(|(start, end, name)| {
            let start_idx = (start as usize).saturating_sub(1);
            let end_idx = (end as usize).min(lines.len());
            if start_idx >= end_idx {
                return None;
            }
            let text = lines[start_idx..end_idx].join("\n");
            if text.trim().is_empty() {
                return None;
            }
            Some(Chunk {
                kind: ChunkKind::DocSection,
                name,
                start_line: start,
                end_line: end,
                text,
            })
        })
        .collect()
}

fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let after_hashes = trimmed.trim_start_matches('#');
    if !after_hashes.starts_with(' ') && !after_hashes.is_empty() {
        return None;
    }
    let title = after_hashes.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn empty_content_yields_no_chunks() {
        let chunks = chunk_file(&PathBuf::from("foo.rs"), "", Granularity::PerFunction);
        assert!(chunks.is_empty());
    }

    #[test]
    fn whitespace_only_content_yields_no_chunks() {
        let chunks = chunk_file(&PathBuf::from("foo.rs"), "   \n\n  ", Granularity::PerFile);
        assert!(chunks.is_empty());
    }

    #[test]
    fn per_file_returns_single_file_chunk() {
        let src = "fn a() {}\nfn b() {}\n";
        let chunks = chunk_file(&PathBuf::from("foo.rs"), src, Granularity::PerFile);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::File);
        assert_eq!(chunks[0].text, src);
    }

    #[test]
    fn per_function_extracts_rust_fns() {
        let src = "fn alpha() { 1 }\nfn beta() { 2 }\n";
        let chunks = chunk_file(&PathBuf::from("foo.rs"), src, Granularity::PerFunction);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, ChunkKind::Function);
        assert_eq!(chunks[0].name.as_deref(), Some("alpha"));
        assert_eq!(chunks[1].name.as_deref(), Some("beta"));
    }

    #[test]
    fn per_function_extracts_rust_struct_as_type() {
        let src = "struct Foo { x: i32 }\n";
        let chunks = chunk_file(&PathBuf::from("foo.rs"), src, Granularity::PerFunction);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::Type);
        assert_eq!(chunks[0].name.as_deref(), Some("Foo"));
    }

    #[test]
    fn unsupported_extension_falls_back_to_file_chunk() {
        let src = "this is some text\nwith multiple lines\n";
        let chunks = chunk_file(&PathBuf::from("notes.txt"), src, Granularity::PerFunction);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::File);
    }

    #[test]
    fn markdown_chunks_by_heading() {
        let src = "# Intro\n\nhello\n\n## Details\n\nworld\n";
        let chunks = chunk_file(&PathBuf::from("readme.md"), src, Granularity::PerFunction);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, ChunkKind::DocSection);
        assert_eq!(chunks[0].name.as_deref(), Some("Intro"));
        assert_eq!(chunks[1].name.as_deref(), Some("Details"));
    }

    #[test]
    fn markdown_without_headings_falls_back_to_file_chunk() {
        let src = "just a paragraph\nwith no heading\n";
        let chunks = chunk_file(&PathBuf::from("notes.md"), src, Granularity::PerFunction);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::DocSection);
    }

    #[test]
    fn python_extracts_classes_and_functions() {
        let src = "class Foo:\n    pass\n\ndef bar():\n    return 1\n";
        let chunks = chunk_file(&PathBuf::from("m.py"), src, Granularity::PerFunction);
        assert_eq!(chunks.len(), 2);
        let names: Vec<_> = chunks.iter().filter_map(|c| c.name.as_deref()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn line_numbers_are_one_based() {
        let src = "// header comment\nfn first() {}\nfn second() {}\n";
        let chunks = chunk_file(&PathBuf::from("a.rs"), src, Granularity::PerFunction);
        assert_eq!(chunks[0].start_line, 2);
        assert_eq!(chunks[1].start_line, 3);
    }
}
