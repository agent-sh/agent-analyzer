//! Match code references against the symbol table.

use std::collections::HashMap;

use analyzer_core::types::{CodeRef, FileSymbols};

use crate::parser::RawCodeRef;

/// Match raw code references against symbols to produce CodeRef entries.
pub fn match_refs(
    refs: &[RawCodeRef],
    symbols: &HashMap<String, FileSymbols>,
) -> Vec<CodeRef> {
    refs.iter()
        .map(|raw| {
            let candidates = extract_candidates(&raw.text);
            // Try each candidate until one matches
            for candidate in &candidates {
                let (exists, file) = find_symbol(candidate, symbols);
                if exists {
                    return CodeRef {
                        text: raw.text.clone(),
                        symbol: candidate.clone(),
                        file,
                        exists: true,
                        line: Some(raw.line),
                        issue: None,
                    };
                }
            }
            // None matched
            let primary = candidates.into_iter().next().unwrap_or_default();
            CodeRef {
                text: raw.text.clone(),
                symbol: primary,
                file: None,
                exists: false,
                line: Some(raw.line),
                issue: None,
            }
        })
        .collect()
}

/// Clean up a code reference text to extract the symbol name.
/// Strips trailing (), leading &/*, type parameters, etc.
fn clean_symbol(text: &str) -> String {
    let mut s = text.to_string();
    // Strip trailing parenthesized args: areas(map) -> areas
    if let Some(paren_pos) = s.find('(') {
        s.truncate(paren_pos);
    }
    // For generic types like Vec<AreaEntry>, Option<&FileActivity>:
    // try to extract the inner type since that's more useful
    if let (Some(open), Some(close)) = (s.find('<'), s.rfind('>')) {
        let outer = &s[..open];
        let inner = &s[open + 1..close];
        // If the outer type is a common wrapper, use the inner type
        let wrappers = ["Vec", "Option", "Result", "Box", "Arc", "Rc", "HashSet"];
        if wrappers.iter().any(|w| outer.trim() == *w) {
            s = inner
                .trim_start_matches('&')
                .trim_start_matches("mut ")
                .to_string();
            // If inner still has angle brackets or commas, take the first type
            if let Some(comma) = s.find(',') {
                s.truncate(comma);
            }
            s = s.trim().to_string();
        } else {
            // Otherwise just strip the type params
            s.truncate(open);
        }
    }
    // Strip leading & or *
    s = s.trim_start_matches('&').trim_start_matches('*').to_string();
    s
}

/// Extract multiple candidate symbols from a code ref text.
/// Generates variants to try: primary, dot-suffix, and camelCase-to-snake_case.
fn extract_candidates(text: &str) -> Vec<String> {
    let primary = clean_symbol(text);
    let mut candidates = vec![primary.clone()];
    // For dot-notation: binary.ensureBinary -> try "ensureBinary"
    if let Some(pos) = primary.rfind('.') {
        candidates.push(primary[pos + 1..].to_string());
    }
    // camelCase -> snake_case (serde rename_all = "camelCase" is common in Rust)
    let snake = camel_to_snake(&primary);
    if snake != primary {
        candidates.push(snake);
    }
    candidates
}

/// Convert camelCase to snake_case.
fn camel_to_snake(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            result.push('_');
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c.to_ascii_lowercase());
        }
    }
    result
}

/// Search the symbol table for a matching symbol.
/// Returns (exists, defining_file).
fn find_symbol(
    symbol: &str,
    symbols: &HashMap<String, FileSymbols>,
) -> (bool, Option<String>) {
    // Try exact name match across all files
    let last_segment = symbol.rsplit("::").next().unwrap_or(symbol);
    let last_segment = last_segment.rsplit('.').next().unwrap_or(last_segment);

    for (path, syms) in symbols {
        // Check exports
        if syms.exports.iter().any(|e| e.name == last_segment) {
            return (true, Some(path.clone()));
        }
        // Check definitions
        if syms.definitions.iter().any(|d| d.name == last_segment) {
            return (true, Some(path.clone()));
        }
    }

    (false, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{DefinitionEntry, SymbolEntry, SymbolKind};
    use crate::parser::RefContext;

    fn sample_symbols() -> HashMap<String, FileSymbols> {
        let mut map = HashMap::new();
        map.insert(
            "src/core.rs".to_string(),
            FileSymbols {
                exports: vec![SymbolEntry {
                    name: "validate".to_string(),
                    kind: SymbolKind::Function,
                    line: 10,
                }],
                imports: vec![],
                definitions: vec![DefinitionEntry {
                    name: "validate".to_string(),
                    kind: SymbolKind::Function,
                    line: 10,
                    complexity: 3,
                }],
            },
        );
        map
    }

    #[test]
    fn test_match_found() {
        let symbols = sample_symbols();
        let refs = vec![RawCodeRef {
            text: "validate()".to_string(),
            line: 5,
            context: RefContext::InlineCode,
        }];
        let result = match_refs(&refs, &symbols);
        assert_eq!(result.len(), 1);
        assert!(result[0].exists);
        assert_eq!(result[0].file, Some("src/core.rs".to_string()));
    }

    #[test]
    fn test_match_not_found() {
        let symbols = sample_symbols();
        let refs = vec![RawCodeRef {
            text: "nonexistent()".to_string(),
            line: 5,
            context: RefContext::InlineCode,
        }];
        let result = match_refs(&refs, &symbols);
        assert_eq!(result.len(), 1);
        assert!(!result[0].exists);
        assert!(result[0].file.is_none());
    }

    #[test]
    fn test_clean_symbol() {
        assert_eq!(clean_symbol("validate()"), "validate");
        assert_eq!(clean_symbol("&Config"), "Config");
        assert_eq!(clean_symbol("core::validate"), "core::validate");
        // Generic wrapper extraction
        assert_eq!(clean_symbol("Vec<AreaEntry>"), "AreaEntry");
        assert_eq!(clean_symbol("Option<&FileActivity>"), "FileActivity");
        assert_eq!(clean_symbol("Result<Value, Error>"), "Value");
        // Function call with args
        assert_eq!(clean_symbol("areas(map)"), "areas");
        assert_eq!(clean_symbol("norms(map)"), "norms");
        // Non-wrapper generic keeps outer
        assert_eq!(clean_symbol("MyType<String>"), "MyType");
    }
}
