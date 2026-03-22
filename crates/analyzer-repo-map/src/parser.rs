//! Language detection and tree-sitter parser initialization.

use anyhow::{Context, Result};
use std::path::Path;

/// Supported languages for AST parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Go,
    Java,
}

impl Language {
    /// Human-readable name for JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript | Language::Tsx => "typescript",
            Language::JavaScript | Language::Jsx => "javascript",
            Language::Python => "python",
            Language::Go => "go",
            Language::Java => "java",
        }
    }
}

/// Detect language from file extension.
pub fn detect_language(path: &str) -> Option<Language> {
    let ext = Path::new(path).extension()?.to_str()?;
    match ext {
        "rs" => Some(Language::Rust),
        "ts" => Some(Language::TypeScript),
        "tsx" => Some(Language::Tsx),
        "js" | "mjs" | "cjs" => Some(Language::JavaScript),
        "jsx" => Some(Language::Jsx),
        "py" => Some(Language::Python),
        "go" => Some(Language::Go),
        "java" => Some(Language::Java),
        _ => None,
    }
}

/// Create a tree-sitter parser for the given language.
pub fn create_parser(lang: Language) -> Result<tree_sitter::Parser> {
    let mut parser = tree_sitter::Parser::new();
    let language = get_language(lang);
    parser
        .set_language(&language)
        .context("failed to set parser language")?;
    Ok(parser)
}

/// Get the tree-sitter language grammar.
fn get_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript | Language::Jsx => tree_sitter_javascript::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
    }
}

/// Parse source code and return a tree-sitter tree.
pub fn parse_source(source: &[u8], lang: Language) -> Result<tree_sitter::Tree> {
    let mut parser = create_parser(lang)?;
    parser
        .parse(source, None)
        .context("tree-sitter failed to parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("src/main.rs"), Some(Language::Rust));
        assert_eq!(detect_language("app.ts"), Some(Language::TypeScript));
        assert_eq!(detect_language("app.tsx"), Some(Language::Tsx));
        assert_eq!(detect_language("app.js"), Some(Language::JavaScript));
        assert_eq!(detect_language("app.jsx"), Some(Language::Jsx));
        assert_eq!(detect_language("app.py"), Some(Language::Python));
        assert_eq!(detect_language("main.go"), Some(Language::Go));
        assert_eq!(detect_language("Main.java"), Some(Language::Java));
        assert_eq!(detect_language("README.md"), None);
        assert_eq!(detect_language("Cargo.toml"), None);
    }

    #[test]
    fn test_parse_rust() {
        let source = b"fn main() { println!(\"hello\"); }";
        let tree = parse_source(source, Language::Rust).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn test_parse_typescript() {
        let source = b"function greet(name: string): void { console.log(name); }";
        let tree = parse_source(source, Language::TypeScript).unwrap();
        assert_eq!(tree.root_node().kind(), "program");
    }

    #[test]
    fn test_parse_python() {
        let source = b"def greet(name):\n    print(name)\n";
        let tree = parse_source(source, Language::Python).unwrap();
        assert_eq!(tree.root_node().kind(), "module");
    }

    #[test]
    fn test_parse_go() {
        let source = b"package main\nfunc main() {}\n";
        let tree = parse_source(source, Language::Go).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn test_parse_java() {
        let source = b"public class Main { public static void main(String[] args) {} }";
        let tree = parse_source(source, Language::Java).unwrap();
        assert_eq!(tree.root_node().kind(), "program");
    }
}
