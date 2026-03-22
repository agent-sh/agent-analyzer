//! Cyclomatic complexity calculation from tree-sitter AST nodes.

use crate::parser::Language;

/// Count decision points within a tree-sitter node subtree.
/// Base complexity is 1, plus 1 for each branch/decision point.
pub fn cyclomatic_complexity(
    node: &tree_sitter::Node,
    source: &[u8],
    lang: Language,
) -> u32 {
    let branch_kinds = branch_node_kinds(lang);
    let operator_patterns = branch_operator_patterns(lang);

    let mut complexity: u32 = 1;
    let mut cursor = node.walk();

    // Walk all descendant nodes
    walk_tree(&mut cursor, &mut |n| {
        let kind = n.kind();
        if branch_kinds.contains(&kind) {
            complexity += 1;
        }
        // Check for logical operators (&& and ||) in binary expressions
        if is_binary_expression(kind, lang) {
            if let Some(op_node) = n.child_by_field_name("operator") {
                let op_text = op_node
                    .utf8_text(source)
                    .unwrap_or("");
                if operator_patterns.contains(&op_text) {
                    complexity += 1;
                }
            }
        }
    });

    complexity
}

/// Node kinds that represent branch/decision points per language.
fn branch_node_kinds(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Rust => &[
            "if_expression",
            "else_clause",
            "match_arm",
            "while_expression",
            "for_expression",
            "loop_expression",
            "try_expression",
        ],
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => &[
            "if_statement",
            "else_clause",
            "switch_case",
            "while_statement",
            "for_statement",
            "for_in_statement",
            "do_statement",
            "catch_clause",
            "ternary_expression",
        ],
        Language::Python => &[
            "if_statement",
            "elif_clause",
            "else_clause",
            "while_statement",
            "for_statement",
            "except_clause",
            "conditional_expression",
        ],
        Language::Go => &[
            "if_statement",
            "expression_case",
            "for_statement",
            "type_case",
        ],
        Language::Java => &[
            "if_statement",
            "switch_block_statement_group",
            "while_statement",
            "for_statement",
            "enhanced_for_statement",
            "do_statement",
            "catch_clause",
            "ternary_expression",
        ],
    }
}

/// Logical operators that add to complexity.
fn branch_operator_patterns(_lang: Language) -> &'static [&'static str] {
    &["&&", "||", "and", "or"]
}

/// Check if a node kind represents a binary expression.
fn is_binary_expression(kind: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => kind == "binary_expression",
        Language::TypeScript
        | Language::Tsx
        | Language::JavaScript
        | Language::Jsx => kind == "binary_expression",
        Language::Python => kind == "boolean_operator",
        Language::Go => kind == "binary_expression",
        Language::Java => kind == "binary_expression",
    }
}

/// Recursively walk a tree-sitter cursor, calling f for each node.
fn walk_tree<F>(cursor: &mut tree_sitter::TreeCursor, f: &mut F)
where
    F: FnMut(&tree_sitter::Node),
{
    f(&cursor.node());
    if cursor.goto_first_child() {
        loop {
            walk_tree(cursor, f);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_source;
    use crate::extractor::extract_file_symbols;

    /// Get complexity of the first function in source via the extractor.
    fn complexity_of(source: &str, lang: Language) -> u32 {
        let syms = extract_file_symbols(source.as_bytes(), lang).unwrap();
        syms.definitions
            .first()
            .map(|d| d.complexity)
            .expect("no function found")
    }

    #[test]
    fn test_linear_rust_function() {
        let c = complexity_of("fn foo() { let x = 1; }", Language::Rust);
        assert_eq!(c, 1);
    }

    #[test]
    fn test_rust_if() {
        let c = complexity_of(
            "fn foo(x: bool) { if x { return; } }",
            Language::Rust,
        );
        assert_eq!(c, 2); // base + if
    }

    #[test]
    fn test_rust_if_else() {
        let c = complexity_of(
            "fn foo(x: bool) { if x { 1 } else { 2 } }",
            Language::Rust,
        );
        assert_eq!(c, 3); // base + if + else
    }

    #[test]
    fn test_rust_match() {
        let c = complexity_of(
            r#"fn foo(x: i32) { match x { 1 => {}, 2 => {}, _ => {} } }"#,
            Language::Rust,
        );
        assert_eq!(c, 4); // base + 3 arms
    }

    #[test]
    fn test_python_linear() {
        let c = complexity_of("def foo():\n    x = 1\n", Language::Python);
        assert_eq!(c, 1);
    }

    #[test]
    fn test_python_if_elif() {
        let c = complexity_of(
            "def foo(x):\n    if x > 0:\n        return 1\n    elif x < 0:\n        return -1\n    else:\n        return 0\n",
            Language::Python,
        );
        assert_eq!(c, 4); // base + if + elif + else
    }

    #[test]
    fn test_js_ternary() {
        let c = complexity_of(
            "function foo(x) { return x ? 1 : 0; }",
            Language::JavaScript,
        );
        assert_eq!(c, 2); // base + ternary
    }
}
