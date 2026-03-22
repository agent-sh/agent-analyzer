//! Detect naming conventions and test patterns from extracted symbols.

use std::collections::HashMap;
use std::path::Path;

use analyzer_core::types::{
    FileSymbols, NamingPatterns, SymbolKind, TestPatterns,
};

/// Detect naming conventions from symbol definitions.
pub fn detect_naming(symbols: &HashMap<String, FileSymbols>) -> NamingPatterns {
    let mut func_cases: HashMap<&str, usize> = HashMap::new();
    let mut type_cases: HashMap<&str, usize> = HashMap::new();
    let mut const_cases: HashMap<&str, usize> = HashMap::new();

    for syms in symbols.values() {
        for def in &syms.definitions {
            let case = classify_case(&def.name);
            match def.kind {
                SymbolKind::Function => {
                    *func_cases.entry(case).or_default() += 1;
                }
                SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Trait
                | SymbolKind::Interface
                | SymbolKind::Enum
                | SymbolKind::TypeAlias => {
                    *type_cases.entry(case).or_default() += 1;
                }
                SymbolKind::Constant => {
                    *const_cases.entry(case).or_default() += 1;
                }
                SymbolKind::Module
                | SymbolKind::Field
                | SymbolKind::EnumVariant
                | SymbolKind::Property => {}
            }
        }
    }

    NamingPatterns {
        functions: majority_case(&func_cases),
        types: majority_case(&type_cases),
        constants: majority_case(&const_cases),
    }
}

/// Classify a name into a naming convention.
fn classify_case(name: &str) -> &'static str {
    if name.is_empty() {
        return "unknown";
    }
    // SCREAMING_SNAKE: all uppercase with underscores
    if name.len() > 1
        && name.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
    {
        return "SCREAMING_SNAKE";
    }
    // PascalCase: starts with uppercase, has lowercase
    if name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().any(|c| c.is_ascii_lowercase())
        && !name.contains('_')
    {
        return "PascalCase";
    }
    // snake_case: all lowercase with underscores
    if name.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()) {
        return "snake_case";
    }
    // camelCase: starts with lowercase, has uppercase
    if name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.chars().any(|c| c.is_ascii_uppercase())
        && !name.contains('_')
    {
        return "camelCase";
    }
    "mixed"
}

/// Return the case with the most votes.
fn majority_case(cases: &HashMap<&str, usize>) -> String {
    cases
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(case, _)| case.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Detect test framework and patterns from file paths and symbols.
pub fn detect_test_patterns(
    repo_path: &Path,
    symbols: &HashMap<String, FileSymbols>,
) -> TestPatterns {
    let framework = detect_framework(symbols);
    let location = detect_test_location(repo_path, symbols);
    let naming = detect_test_naming(symbols);

    TestPatterns {
        framework,
        location,
        naming,
    }
}

fn detect_framework(symbols: &HashMap<String, FileSymbols>) -> String {
    let mut scores: HashMap<&str, usize> = HashMap::new();

    for (path, syms) in symbols {
        // Check file extensions for language hints
        if path.ends_with(".rs") {
            // Rust uses #[test] attribute - check for test_ prefix functions
            for def in &syms.definitions {
                if def.name.starts_with("test_") {
                    *scores.entry("cargo-test").or_default() += 1;
                }
            }
        }

        // Check imports for framework detection
        for imp in &syms.imports {
            match imp.from.as_str() {
                "pytest" | "unittest" => {
                    *scores.entry("pytest").or_default() += 5;
                }
                "testing" => {
                    // Go testing package
                    *scores.entry("go-test").or_default() += 5;
                }
                _ => {}
            }
            for name in &imp.names {
                match name.as_str() {
                    "describe" | "it" | "expect" | "test" | "jest" => {
                        *scores.entry("jest").or_default() += 1;
                    }
                    "Test" if imp.from.contains("junit") => {
                        *scores.entry("junit").or_default() += 5;
                    }
                    _ => {}
                }
            }
        }
    }

    scores
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(fw, _)| fw.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn detect_test_location(
    _repo_path: &Path,
    symbols: &HashMap<String, FileSymbols>,
) -> String {
    let mut locations: HashMap<&str, usize> = HashMap::new();

    for path in symbols.keys() {
        let normalized = path.replace('\\', "/");
        if normalized.contains("__tests__/") {
            *locations.entry("__tests__").or_default() += 1;
        } else if normalized.contains("/tests/") || normalized.starts_with("tests/") {
            *locations.entry("tests/").or_default() += 1;
        } else if normalized.contains("/spec/") || normalized.starts_with("spec/") {
            *locations.entry("spec/").or_default() += 1;
        } else if normalized.contains("_test.")
            || normalized.contains(".test.")
            || normalized.contains(".spec.")
            || normalized.contains("_spec.")
        {
            *locations.entry("co-located").or_default() += 1;
        }
    }

    // Check for inline tests (Rust #[cfg(test)] mod tests)
    for (path, syms) in symbols {
        if path.ends_with(".rs") {
            for def in &syms.definitions {
                if def.name == "tests" && def.kind == SymbolKind::Module {
                    *locations.entry("inline").or_default() += 1;
                }
            }
        }
    }

    locations
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(loc, _)| loc.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn detect_test_naming(symbols: &HashMap<String, FileSymbols>) -> String {
    let mut patterns: HashMap<&str, usize> = HashMap::new();

    for syms in symbols.values() {
        for def in &syms.definitions {
            if def.kind == SymbolKind::Function {
                if def.name.starts_with("test_") {
                    *patterns.entry("test_*").or_default() += 1;
                } else if def.name.starts_with("Test") {
                    *patterns.entry("Test*").or_default() += 1;
                } else if def.name.ends_with("_test") {
                    *patterns.entry("*_test").or_default() += 1;
                } else if def.name.starts_with("should_") || def.name.starts_with("it_") {
                    *patterns.entry("should_*").or_default() += 1;
                }
            }
        }
    }

    patterns
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(pat, _)| pat.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_case() {
        assert_eq!(classify_case("snake_case_name"), "snake_case");
        assert_eq!(classify_case("camelCaseName"), "camelCase");
        assert_eq!(classify_case("PascalCaseName"), "PascalCase");
        assert_eq!(classify_case("SCREAMING_SNAKE"), "SCREAMING_SNAKE");
        assert_eq!(classify_case("MAX_SIZE"), "SCREAMING_SNAKE");
        assert_eq!(classify_case("x"), "snake_case");
    }

    #[test]
    fn test_detect_naming_rust_style() {
        let mut symbols = HashMap::new();
        symbols.insert(
            "src/lib.rs".to_string(),
            FileSymbols {
                exports: vec![],
                imports: vec![],
                definitions: vec![
                    analyzer_core::types::DefinitionEntry {
                        name: "validate_input".to_string(),
                        kind: SymbolKind::Function,
                        line: 1,
                        complexity: 1,
                    },
                    analyzer_core::types::DefinitionEntry {
                        name: "parse_data".to_string(),
                        kind: SymbolKind::Function,
                        line: 10,
                        complexity: 1,
                    },
                    analyzer_core::types::DefinitionEntry {
                        name: "Config".to_string(),
                        kind: SymbolKind::Struct,
                        line: 20,
                        complexity: 1,
                    },
                ],
            },
        );

        let naming = detect_naming(&symbols);
        assert_eq!(naming.functions, "snake_case");
        assert_eq!(naming.types, "PascalCase");
    }
}
