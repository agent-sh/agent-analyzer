//! Parse markdown files to extract code references.

use std::path::Path;

use std::sync::LazyLock;

use anyhow::Result;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use regex::Regex;

static API_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z_]\w*(?:::\w+|\.\w+)+)\b").unwrap());

/// A raw code reference found in a markdown file.
#[derive(Debug, Clone)]
pub struct RawCodeRef {
    pub text: String,
    pub line: usize,
    #[allow(dead_code)]
    pub context: RefContext,
}

/// Where the reference was found.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RefContext {
    InlineCode,
    ApiMention,
}

/// Noise words that should not be treated as symbol references.
static NOISE_WORDS: &[&str] = &[
    // Literals and keywords
    "true", "false", "null", "None", "nil", "self", "this",
    "ok", "err", "OK", "ERROR", "TODO", "FIXME", "NOTE", "WARN",
    // Format/language names
    "bash", "json", "yaml", "toml", "xml", "html", "css",
    "http", "https", "localhost", "stdin", "stdout", "stderr",
    // Common doc/config field names (often in backticks)
    "name", "version", "description", "author", "license",
    "type", "main", "scripts", "dependencies",
    // Common short words from docs
    "any", "all", "not", "and", "the", "for", "with",
    "add", "fix", "new", "run", "set", "get", "put", "use",
    "cd", "ls", "rm", "cp", "mv", "gh", "git", "npm",
    "src", "bin", "lib", "pkg", "env", "api", "url",
    // Conventional commit types
    "feat", "fix", "docs", "chore", "refactor", "style", "test", "ci", "perf",
    // Changelog headings
    "Added", "Changed", "Deprecated", "Removed", "Fixed", "Security",
];

/// Common file extensions - references ending in these are file paths, not symbols.
static FILE_EXTENSIONS: &[&str] = &[
    ".md", ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java",
    ".json", ".yml", ".yaml", ".toml", ".lock", ".txt", ".sh", ".bash",
    ".css", ".scss", ".html", ".svg", ".png", ".jpg", ".gif",
    ".exe", ".dll", ".so", ".wasm", ".cfg", ".ini", ".env",
    ".sum", ".mod", ".log", ".xml", ".csv",
    ".csproj", ".gemspec", ".swift", ".gradle", ".sln", ".plist",
    ".scm", ".el", ".vim", ".ps1",
];

/// Common TLDs - dot-separated words ending in these are likely URLs.
static URL_TLDS: &[&str] = &[
    ".com", ".org", ".net", ".io", ".dev", ".co", ".ai", ".app",
    ".rs", ".js", ".py", ".sh",
];

/// Extract code references from a markdown file.
pub fn extract_code_refs(doc_path: &Path) -> Result<Vec<RawCodeRef>> {
    let content = std::fs::read_to_string(doc_path)?;
    let mut refs = Vec::new();

    // Track line numbers by character offset
    let line_offsets = compute_line_offsets(&content);

    let parser = Parser::new(&content);
    let mut in_code_block = false;
    for (event, range) in parser.into_offset_iter() {
        let line = offset_to_line(&line_offsets, range.start);

        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
            }
            Event::Code(text) => {
                let text = text.to_string();
                if is_symbol_like(&text) {
                    refs.push(RawCodeRef {
                        text,
                        line,
                        context: RefContext::InlineCode,
                    });
                }
            }
            // Skip code blocks - they contain full programs and produce too much noise.
            // Inline code backticks and prose API mentions are higher-quality signals.
            Event::Text(_) if in_code_block => {}
            Event::Text(text) if !in_code_block => {
                // Look for API-like mentions in prose: module::function or Class.method
                for cap in API_RE.captures_iter(&text) {
                    let matched = cap[1].to_string();
                    if is_symbol_like(&matched) {
                        refs.push(RawCodeRef {
                            text: matched,
                            line,
                            context: RefContext::ApiMention,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    Ok(refs)
}

fn compute_line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, c) in content.char_indices() {
        if c == '\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

fn offset_to_line(line_offsets: &[usize], offset: usize) -> usize {
    match line_offsets.binary_search(&offset) {
        Ok(i) => i + 1,
        Err(i) => i, // line number (1-based)
    }
}

/// Language-agnostic well-known types/globals that aren't project symbols.
static WELL_KNOWN: &[&str] = &[
    // Rust primitives and stdlib
    "usize", "isize", "u8", "u16", "u32", "u64", "u128",
    "i8", "i16", "i32", "i64", "i128", "f32", "f64",
    "bool", "str", "char", "String", "Vec", "Option", "Result",
    "HashMap", "HashSet", "BTreeMap", "Box", "Arc", "Rc",
    "None", "Some", "Ok", "Err", "Self",
    // JS/TS globals
    "undefined", "NaN", "Infinity",
    "Promise", "Error", "TypeError", "Map", "Set", "Array",
    "Function", "Symbol", "RegExp", "Date", "Buffer",
    "require", "module", "exports", "import", "export",
    "async", "await", "const", "let", "var", "function",
    "return", "throw", "catch", "finally", "typeof", "instanceof",
    // Python builtins
    "print", "range", "list", "dict", "tuple", "set", "int", "float",
    "type", "class", "super", "lambda", "yield", "from",
    // Go
    "nil", "make", "append", "len", "cap", "new", "func", "chan",
    "interface", "struct", "defer", "select",
];

/// JS/language stdlib dot-method prefixes that aren't project symbols.
static STDLIB_PREFIXES: &[&str] = &[
    "Object", "Array", "JSON", "Math", "Date", "Promise", "String",
    "Number", "Boolean", "RegExp", "Error", "Map", "Set", "Symbol",
    "console", "process", "Buffer", "global", "window", "document",
    "fs", "path", "os", "util", "http", "https", "crypto",
];

/// Check if a code reference looks like a real project-specific code symbol.
fn is_symbol_like(text: &str) -> bool {
    let len = text.len();

    // ── Length guards ──
    if !(2..=100).contains(&len) {
        return false;
    }

    // ── Character-class filters ──

    // Must contain at least one letter
    if !text.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    // Pure number or version string (0.23.3, v1.0)
    if text.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'v') {
        return false;
    }

    // ── Prefix/suffix filters ──

    // Starts with . (method chain: .map, .filter, .unwrap, .clone)
    if text.starts_with('.') {
        return false;
    }
    // Starts with $ (shell/template variable: $VAR, ${VAR}, $command)
    if text.starts_with('$') {
        return false;
    }
    // Starts with # or ``` (markdown artifact)
    if text.starts_with('#') || text.starts_with("```") {
        return false;
    }
    // Starts with - (CLI flag: --map-file, -D)
    if text.starts_with('-') {
        return false;
    }
    // Starts with ! or : or @ (negation, CSS pseudo, decorator/package)
    if text.starts_with('!') || text.starts_with(':') || text.starts_with('@') {
        return false;
    }
    // Surrounded by quotes ('flag', "acp")
    if (text.starts_with('\'') && text.ends_with('\''))
        || (text.starts_with('"') && text.ends_with('"'))
    {
        return false;
    }

    // ── Contains filters ──

    // Contains brackets: [OK], [ERROR], dependabot[bot]
    if text.contains('[') || text.contains(']') {
        return false;
    }
    // Contains backslash: regex patterns \(aider\)$
    if text.contains('\\') {
        return false;
    }
    // Contains =: assignment syntax (NEEDS_COMMIT=true)
    if text.contains('=') {
        return false;
    }
    // Contains space: shell commands (git log, npm test)
    if text.contains(' ') {
        return false;
    }
    // Contains .. : git range syntax (analyzedUpTo..HEAD)
    if text.contains("..") {
        return false;
    }
    // Contains !() or ![: Rust macros (include_str!(), vec![])
    if text.contains("!()") || text.contains("![") {
        return false;
    }

    // ── Word-list filters ──

    if NOISE_WORDS.contains(&text) {
        return false;
    }
    if WELL_KNOWN.contains(&text) {
        return false;
    }

    // ── Path/URL filters ──

    // Contains / without :: : file path
    if text.contains('/') && !text.contains("::") {
        return false;
    }
    // Ends with known file extension
    if FILE_EXTENSIONS.iter().any(|ext| text.ends_with(ext)) {
        return false;
    }
    // Contains TLD pattern without :: : URL
    if URL_TLDS.iter().any(|tld| text.contains(tld)) && !text.contains("::") {
        return false;
    }

    // ── Pattern filters ──

    // Kebab-case with no :: : CLI names (repo-intel, tree-sitter)
    if text.contains('-') && !text.contains("::") {
        return false;
    }
    // Build target triples
    if text.contains("-unknown-") || text.contains("-apple-") || text.contains("-pc-") {
        return false;
    }
    // ALL_CAPS_SNAKE without :: : env vars / constants (CLAUDE_PROJECT_DIR, SESSION_ID)
    // Must have at least one underscore and be all uppercase
    if text.contains('_')
        && text
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
    {
        return false;
    }

    // Version placeholders (X.Y.Z, X.Y)
    if text == "X.Y.Z" || text == "X.Y" {
        return false;
    }
    // Glob patterns (*.tsx, Notebook.*)
    if text.contains('*') {
        return false;
    }
    // Ends with : (conventional commit prefixes: feat:, fix:, docs:)
    if text.ends_with(':') {
        return false;
    }
    // Keyboard shortcuts (Ctrl+T, Cmd+Esc, Alt+Enter)
    if text.contains('+') && text.chars().any(|c| c.is_ascii_uppercase()) {
        return false;
    }
    // Abbreviations (e.g, i.e, vs, etc)
    if text == "e.g" || text == "i.e" || text == "vs" || text == "etc" {
        return false;
    }
    // HTML/XML tags: <enforcement>, <path> (but not generics like Vec<T>)
    if text.starts_with('<') && text.ends_with('>') {
        return false;
    }
    // Single colon without :: (plugin:command references: ship:ship, new:plugin)
    if text.contains(':') && !text.contains("::") {
        return false;
    }
    // Version-like with x (v1.x, v3.x, vX.Y.Z, rc.1)
    if text.contains(".x") || text.starts_with("rc.") {
        return false;
    }
    // Parenthesized with string literal args: Skill('consult'), require("foo")
    if text.contains("('") || text.contains("(\"") {
        return false;
    }

    // ── Dot-notation filters ──

    if text.contains('.') && !text.contains("::") {
        let first = text.split('.').next().unwrap_or("");

        // JS/language stdlib calls: Object.entries, JSON.parse, console.log, process.env
        if STDLIB_PREFIXES.contains(&first) {
            return false;
        }

        // Config paths: profile.release, git.analyzedUpTo, package.json
        let config_ns = [
            "profile", "git", "workspace", "package", "build",
            "dev", "test", "bench", "dependencies", "features",
            "author", "repository", "homepage",
        ];
        if config_ns.contains(&first) {
            return false;
        }

        // Starts with dot-prefixed directory: .claude, .cursor, .github
        if first.starts_with('.') {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_symbol_like() {
        // Valid symbols
        assert!(is_symbol_like("validate()"));
        assert!(is_symbol_like("Config"));
        assert!(is_symbol_like("std::collections::HashMap"));

        // Noise words
        assert!(!is_symbol_like("x"));
        assert!(!is_symbol_like("42"));
        assert!(!is_symbol_like("true"));
        assert!(!is_symbol_like("null"));
        assert!(!is_symbol_like("bash"));
    }

    #[test]
    fn test_filter_file_paths() {
        assert!(!is_symbol_like("CLAUDE.md"));
        assert!(!is_symbol_like("README.md"));
        assert!(!is_symbol_like("config.json"));
        assert!(!is_symbol_like("ci.yml"));
        assert!(!is_symbol_like("main.rs"));
        assert!(!is_symbol_like("app.py"));
    }

    #[test]
    fn test_filter_urls() {
        assert!(!is_symbol_like("github.com"));
        assert!(!is_symbol_like("docs.rs"));
        assert!(!is_symbol_like("crates.io"));
        assert!(!is_symbol_like("agent-sh.dev"));
    }

    #[test]
    fn test_filter_build_targets() {
        assert!(!is_symbol_like("x86_64-unknown-linux-gnu"));
        assert!(!is_symbol_like("aarch64-apple-darwin"));
        assert!(!is_symbol_like("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn test_filter_cli_flags() {
        assert!(!is_symbol_like("--map-file"));
        assert!(!is_symbol_like("--top"));
        assert!(!is_symbol_like("-D"));
    }

    #[test]
    fn test_filter_versions() {
        assert!(!is_symbol_like("v1.0"));
        assert!(!is_symbol_like("0.23.3"));
    }

    #[test]
    fn test_filter_brackets_and_regex() {
        assert!(!is_symbol_like("[OK]"));
        assert!(!is_symbol_like("[ERROR]"));
        assert!(!is_symbol_like("dependabot[bot]"));
        assert!(!is_symbol_like(r"\(aider\)$"));
        assert!(!is_symbol_like(r"\[bot\]$"));
    }

    #[test]
    fn test_filter_kebab_case_cli_names() {
        assert!(!is_symbol_like("repo-intel"));
        assert!(!is_symbol_like("git-map"));
        assert!(!is_symbol_like("sync-docs"));
        assert!(!is_symbol_like("tree-sitter"));
        assert!(!is_symbol_like("agent-analyzer"));
    }

    #[test]
    fn test_filter_rust_primitives() {
        assert!(!is_symbol_like("usize"));
        assert!(!is_symbol_like("String"));
        assert!(!is_symbol_like("Vec"));
        assert!(!is_symbol_like("HashMap"));
        assert!(!is_symbol_like("Option"));
    }

    #[test]
    fn test_filter_macros() {
        assert!(!is_symbol_like("include_str!()"));
        assert!(!is_symbol_like("vec![]"));
    }

    #[test]
    fn test_extract_inline_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        std::fs::write(
            &path,
            "Use `validate()` to check input.\nAlso try `Config::new()`.\n",
        )
        .unwrap();

        let refs = extract_code_refs(&path).unwrap();
        assert!(refs.iter().any(|r| r.text == "validate()"));
        assert!(refs.iter().any(|r| r.text == "Config::new()"));
    }

    #[test]
    fn test_filter_noise() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        std::fs::write(&path, "Set `true` or `false` or `null`.\n").unwrap();

        let refs = extract_code_refs(&path).unwrap();
        assert!(refs.is_empty());
    }
}
