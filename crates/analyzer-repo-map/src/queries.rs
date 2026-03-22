//! Query functions for AST symbol data.

use std::collections::HashMap;

use serde::Serialize;

use analyzer_core::types::{FileSymbols, ImportEntry};

/// Result of a symbols query for a single file.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSymbolsResult {
    pub path: String,
    pub exports: Vec<SymbolResult>,
    pub imports: Vec<ImportEntry>,
    pub definitions: Vec<DefinitionResult>,
    pub imported_by: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SymbolResult {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Serialize)]
pub struct DefinitionResult {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub complexity: u32,
}

/// Query symbols for a specific file.
pub fn symbols(
    symbols_map: &HashMap<String, FileSymbols>,
    import_graph: &HashMap<String, Vec<String>>,
    file: &str,
) -> Option<FileSymbolsResult> {
    let syms = symbols_map.get(file)?;

    // Find files that import this file
    let imported_by: Vec<String> = import_graph
        .iter()
        .filter(|(_, deps)| deps.iter().any(|d| d == file || d.ends_with(file)))
        .map(|(path, _)| path.clone())
        .collect();

    Some(FileSymbolsResult {
        path: file.to_string(),
        exports: syms
            .exports
            .iter()
            .map(|e| SymbolResult {
                name: e.name.clone(),
                kind: format!("{:?}", e.kind).to_lowercase(),
                line: e.line,
            })
            .collect(),
        imports: syms.imports.clone(),
        definitions: syms
            .definitions
            .iter()
            .map(|d| DefinitionResult {
                name: d.name.clone(),
                kind: format!("{:?}", d.kind).to_lowercase(),
                line: d.line,
                complexity: d.complexity,
            })
            .collect(),
        imported_by,
    })
}

/// Result of a dependents query.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependentsResult {
    pub symbol: String,
    pub defined_in: Option<String>,
    pub used_by: Vec<DependentEntry>,
}

#[derive(Debug, Serialize)]
pub struct DependentEntry {
    pub file: String,
    pub import_from: String,
}

/// Find all files that import/use a given symbol.
pub fn dependents(
    symbols_map: &HashMap<String, FileSymbols>,
    symbol: &str,
    file_filter: Option<&str>,
) -> DependentsResult {
    // Find where the symbol is defined
    let defined_in = if let Some(filter) = file_filter {
        if symbols_map.contains_key(filter) {
            Some(filter.to_string())
        } else {
            None
        }
    } else {
        symbols_map
            .iter()
            .find(|(_, syms)| {
                syms.definitions.iter().any(|d| d.name == symbol)
                    || syms.exports.iter().any(|e| e.name == symbol)
            })
            .map(|(path, _)| path.clone())
    };

    // Find all files that import this symbol
    let mut used_by = Vec::new();
    for (path, syms) in symbols_map {
        for imp in &syms.imports {
            if imp.names.iter().any(|n| n == symbol) {
                used_by.push(DependentEntry {
                    file: path.clone(),
                    import_from: imp.from.clone(),
                });
            }
        }
    }

    DependentsResult {
        symbol: symbol.to_string(),
        defined_in,
        used_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{
        DefinitionEntry, FileSymbols, ImportEntry, SymbolEntry, SymbolKind,
    };

    fn sample_symbols() -> (HashMap<String, FileSymbols>, HashMap<String, Vec<String>>) {
        let mut map = HashMap::new();
        map.insert(
            "src/core.rs".to_string(),
            FileSymbols {
                exports: vec![SymbolEntry {
                    name: "validate".to_string(),
                    kind: SymbolKind::Function,
                    line: 10,
                }],
                imports: vec![ImportEntry {
                    from: "src/types".to_string(),
                    names: vec!["Config".to_string()],
                }],
                definitions: vec![DefinitionEntry {
                    name: "validate".to_string(),
                    kind: SymbolKind::Function,
                    line: 10,
                    complexity: 3,
                }],
            },
        );
        map.insert(
            "src/main.rs".to_string(),
            FileSymbols {
                exports: vec![],
                imports: vec![ImportEntry {
                    from: "src/core".to_string(),
                    names: vec!["validate".to_string()],
                }],
                definitions: vec![],
            },
        );

        let mut graph = HashMap::new();
        graph.insert("src/core.rs".to_string(), vec!["src/types".to_string()]);
        graph.insert("src/main.rs".to_string(), vec!["src/core".to_string()]);

        (map, graph)
    }

    #[test]
    fn test_symbols_query() {
        let (map, graph) = sample_symbols();
        let result = symbols(&map, &graph, "src/core.rs").unwrap();
        assert_eq!(result.exports.len(), 1);
        assert_eq!(result.exports[0].name, "validate");
        assert_eq!(result.definitions.len(), 1);
    }

    #[test]
    fn test_symbols_not_found() {
        let (map, graph) = sample_symbols();
        assert!(symbols(&map, &graph, "nonexistent.rs").is_none());
    }

    #[test]
    fn test_dependents_query() {
        let (map, _graph) = sample_symbols();
        let result = dependents(&map, "validate", None);
        assert_eq!(result.symbol, "validate");
        assert!(result.defined_in.is_some());
        assert_eq!(result.used_by.len(), 1);
        assert_eq!(result.used_by[0].file, "src/main.rs");
    }
}
