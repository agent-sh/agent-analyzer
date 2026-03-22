use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Full repo-intel JSON schema - the primary output artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoIntelData {
    pub version: String,
    pub generated: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub partial: bool,
    pub git: GitInfo,
    pub contributors: Contributors,
    pub file_activity: HashMap<String, FileActivity>,
    pub coupling: HashMap<String, HashMap<String, CouplingEntry>>,
    pub conventions: ConventionInfo,
    pub ai_attribution: AiAttribution,
    pub releases: Releases,
    pub renames: Vec<RenameEntry>,
    pub deletions: Vec<DeletionEntry>,

    // Phase 2: AST symbol data (optional - populated when AST analysis runs)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub symbols: Option<HashMap<String, FileSymbols>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub import_graph: Option<HashMap<String, Vec<String>>>,

    // Phase 3: Project metadata (optional - populated when collectors run)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project: Option<ProjectMetadata>,

    // Phase 4: Doc-code cross-references (optional - populated when sync-check runs)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub doc_refs: Option<HashMap<String, DocRefEntry>>,
}

/// Git repository metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub analyzed_up_to: String,
    pub total_commits_analyzed: u64,
    pub first_commit_date: String,
    pub last_commit_date: String,
    pub scope: Option<String>,
    pub shallow: bool,
}

/// Top-level contributors container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contributors {
    pub humans: HashMap<String, HumanContributor>,
    pub bots: HashMap<String, BotContributor>,
}

/// A human contributor's aggregated stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanContributor {
    pub commits: u64,
    pub recent_commits: u64,
    pub first_seen: String,
    pub last_seen: String,
    pub ai_assisted_commits: u64,
}

/// A bot contributor's aggregated stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotContributor {
    pub commits: u64,
    pub recent_commits: u64,
    pub first_seen: String,
    pub last_seen: String,
}

/// Per-file activity metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileActivity {
    pub changes: u64,
    pub recent_changes: u64,
    pub authors: Vec<String>,
    pub created: String,
    pub last_changed: String,
    pub additions: u64,
    pub deletions: u64,
    pub ai_changes: u64,
    pub ai_additions: u64,
    pub ai_deletions: u64,
    pub bug_fix_changes: u64,
    pub refactor_changes: u64,
    pub last_bug_fix: String,
}

/// Coupling entry for co-change tracking between files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CouplingEntry {
    pub cochanges: u64,
    pub human_cochanges: u64,
    pub ai_cochanges: u64,
}

/// Commit message convention tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConventionInfo {
    pub prefixes: HashMap<String, u64>,
    pub style: String,
    pub uses_scopes: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub naming_patterns: Option<NamingPatterns>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub test_patterns: Option<TestPatterns>,
}

/// AI attribution statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiAttribution {
    pub attributed: u64,
    pub heuristic: u64,
    pub none: u64,
    pub tools: HashMap<String, u64>,
    pub confidence: String,
}

/// Release tag tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Releases {
    pub tags: Vec<ReleaseTag>,
    pub cadence: String,
}

/// A single release tag entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseTag {
    pub tag: String,
    pub commit: String,
    pub date: String,
    pub commits_since: u64,
}

/// A rename event in the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameEntry {
    pub from: String,
    pub to: String,
    pub commit: String,
    pub date: String,
}

/// A deletion event in the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletionEntry {
    pub path: String,
    pub commit: String,
    pub date: String,
}

// ─── Phase 2: AST Symbol Types ──────────────────────────────────

/// Symbols extracted from a single file via AST parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSymbols {
    pub exports: Vec<SymbolEntry>,
    pub imports: Vec<ImportEntry>,
    pub definitions: Vec<DefinitionEntry>,
}

/// An exported symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
}

/// Kind of symbol extracted from AST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolKind {
    Function,
    Class,
    Struct,
    Trait,
    Interface,
    Enum,
    Constant,
    TypeAlias,
    Module,
    Field,
    EnumVariant,
    Property,
}

/// An import reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEntry {
    pub from: String,
    pub names: Vec<String>,
}

/// A definition with complexity info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionEntry {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    pub complexity: u32,
}

/// Naming convention patterns detected from code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamingPatterns {
    pub functions: String,
    pub types: String,
    pub constants: String,
}

/// Test framework and patterns detected from code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestPatterns {
    pub framework: String,
    pub location: String,
    pub naming: String,
}

// ─── Phase 3: Project Metadata Types ────────────────────────────

/// Project metadata gathered by collectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMetadata {
    pub readme: Option<ReadmeInfo>,
    pub license: Option<String>,
    pub ci: Option<CiInfo>,
    pub package_manager: Option<String>,
    pub languages: Vec<LanguageInfo>,
}

/// README file information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadmeInfo {
    pub exists: bool,
    pub path: String,
    pub sections: Vec<String>,
}

/// CI provider detection result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiInfo {
    pub provider: String,
    pub config_files: Vec<String>,
}

/// Language distribution entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageInfo {
    pub language: String,
    pub file_count: usize,
    pub percentage: f64,
}

// ─── Phase 4: Doc-Code Cross-Reference Types ────────────────────

/// A documentation file's cross-references to code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocRefEntry {
    pub code_refs: Vec<CodeRef>,
    pub last_updated: String,
    pub references_hot_files: bool,
}

/// A single code reference found in a doc file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeRef {
    pub text: String,
    pub symbol: String,
    pub file: Option<String>,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub issue: Option<String>,
}

// ─── AI Detection Types ─────────────────────────────────────────

/// AI detection signal for a single commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSignal {
    pub detected: bool,
    pub tool: Option<String>,
    pub method: Option<String>,
}

impl AiSignal {
    /// Create a signal indicating no AI was detected.
    pub fn none() -> Self {
        Self {
            detected: false,
            tool: None,
            method: None,
        }
    }

    /// Create a signal indicating AI was detected.
    pub fn detected(tool: impl Into<String>, method: impl Into<String>) -> Self {
        Self {
            detected: true,
            tool: Some(tool.into()),
            method: Some(method.into()),
        }
    }
}

/// Raw extraction output - the delta from one extraction pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitDelta {
    pub head: String,
    pub commits: Vec<CommitInfo>,
    pub renames: Vec<RenameEntry>,
    pub deletions: Vec<DeletionEntry>,
}

/// Parsed commit information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub hash: String,
    pub author_name: String,
    pub author_email: String,
    pub date: String,
    pub subject: String,
    pub body: String,
    pub trailers: Vec<String>,
    pub files: Vec<FileChange>,
}

/// A file change within a single commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

/// Commit size classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommitSize {
    Tiny,
    Small,
    Medium,
    Large,
    Huge,
}

impl CommitSize {
    /// Classify a commit by total lines changed (additions + deletions).
    pub fn classify(total_lines: u64) -> Self {
        match total_lines {
            0..10 => CommitSize::Tiny,
            10..50 => CommitSize::Small,
            50..200 => CommitSize::Medium,
            200..500 => CommitSize::Large,
            _ => CommitSize::Huge,
        }
    }
}

/// Extract conventional commit prefix from a subject line.
///
/// Returns the prefix (e.g., "feat", "fix", "chore") if the subject follows
/// conventional commit format, or `None` otherwise.
pub fn extract_conventional_prefix(subject: &str) -> Option<String> {
    let subject = subject.trim();
    // Skip leading emoji if present
    let s = subject.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    // Match pattern: type[(scope)][!]: description
    if let Some(colon_pos) = s.find(':') {
        let before_colon = &s[..colon_pos];
        // Strip optional ! (breaking change marker)
        let before_colon = before_colon.trim_end_matches('!');
        // Strip optional (scope)
        let prefix = if let Some(paren_pos) = before_colon.find('(') {
            &before_colon[..paren_pos]
        } else {
            before_colon
        };
        let prefix = prefix.trim();
        // Validate: must be a single lowercase word
        if !prefix.is_empty()
            && prefix.len() <= 20
            && prefix.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        {
            return Some(prefix.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_size_classify() {
        assert_eq!(CommitSize::classify(0), CommitSize::Tiny);
        assert_eq!(CommitSize::classify(5), CommitSize::Tiny);
        assert_eq!(CommitSize::classify(9), CommitSize::Tiny);
        assert_eq!(CommitSize::classify(10), CommitSize::Small);
        assert_eq!(CommitSize::classify(49), CommitSize::Small);
        assert_eq!(CommitSize::classify(50), CommitSize::Medium);
        assert_eq!(CommitSize::classify(199), CommitSize::Medium);
        assert_eq!(CommitSize::classify(200), CommitSize::Large);
        assert_eq!(CommitSize::classify(499), CommitSize::Large);
        assert_eq!(CommitSize::classify(500), CommitSize::Huge);
        assert_eq!(CommitSize::classify(10000), CommitSize::Huge);
    }

    #[test]
    fn test_extract_conventional_prefix() {
        assert_eq!(
            extract_conventional_prefix("feat: add new feature"),
            Some("feat".to_string())
        );
        assert_eq!(
            extract_conventional_prefix("fix(core): handle null"),
            Some("fix".to_string())
        );
        assert_eq!(
            extract_conventional_prefix("chore!: breaking change"),
            Some("chore".to_string())
        );
        assert_eq!(
            extract_conventional_prefix("docs(api): update readme"),
            Some("docs".to_string())
        );
        assert_eq!(extract_conventional_prefix("random commit message"), None);
        assert_eq!(extract_conventional_prefix("Update README.md"), None);
        assert_eq!(extract_conventional_prefix(""), None);
    }

    #[test]
    fn test_ai_signal_constructors() {
        let none = AiSignal::none();
        assert!(!none.detected);
        assert!(none.tool.is_none());
        assert!(none.method.is_none());

        let detected = AiSignal::detected("claude", "trailer");
        assert!(detected.detected);
        assert_eq!(detected.tool.as_deref(), Some("claude"));
        assert_eq!(detected.method.as_deref(), Some("trailer"));
    }

    #[test]
    fn test_repo_intel_data_serialization() {
        let data = RepoIntelData {
            version: "1.0".to_string(),
            generated: Utc::now(),
            updated: Utc::now(),
            partial: false,
            git: GitInfo {
                analyzed_up_to: "abc123".to_string(),
                total_commits_analyzed: 100,
                first_commit_date: "2024-01-01".to_string(),
                last_commit_date: "2026-03-14".to_string(),
                scope: None,
                shallow: false,
            },
            contributors: Contributors {
                humans: HashMap::new(),
                bots: HashMap::new(),
            },
            file_activity: HashMap::new(),
            coupling: HashMap::new(),
            conventions: ConventionInfo {
                prefixes: HashMap::new(),
                style: "unknown".to_string(),
                uses_scopes: false,
                naming_patterns: None,
                test_patterns: None,
            },
            ai_attribution: AiAttribution {
                attributed: 0,
                heuristic: 0,
                none: 0,
                tools: HashMap::new(),
                confidence: "low".to_string(),
            },
            releases: Releases {
                tags: vec![],
                cadence: "unknown".to_string(),
            },
            renames: vec![],
            deletions: vec![],
            symbols: None,
            import_graph: None,
            project: None,
            doc_refs: None,
        };

        let json = serde_json::to_string(&data).unwrap();
        let roundtrip: RepoIntelData = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.version, "1.0");
        assert_eq!(roundtrip.git.analyzed_up_to, "abc123");
        assert_eq!(roundtrip.ai_attribution.confidence, "low");
    }

    #[test]
    fn test_commit_size_serialization() {
        let size = CommitSize::Tiny;
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "\"tiny\"");

        let roundtrip: CommitSize = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, CommitSize::Tiny);
    }
}
