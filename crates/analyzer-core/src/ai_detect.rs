use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;

use crate::types::{AiSignal, CommitInfo};

/// AI signature registry, loaded from embedded JSON.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiSignatures {
    trailer_emails: HashMap<String, String>,
    author_emails: HashMap<String, String>,
    author_patterns: HashMap<String, String>,
    #[allow(dead_code)]
    branch_prefixes: HashMap<String, String>,
    message_patterns: HashMap<String, String>,
    trailer_names: HashMap<String, String>,
    bot_authors: HashMap<String, String>,
}

/// Embedded AI signatures JSON.
static AI_SIGNATURES_JSON: &str = include_str!("ai_signatures.json");

/// Load the AI signatures registry from the embedded JSON.
fn load_signatures() -> AiSignatures {
    serde_json::from_str(AI_SIGNATURES_JSON).expect("failed to parse embedded ai_signatures.json")
}

/// Detect AI involvement in a commit using the signature registry.
///
/// Check order: trailers -> author email -> author name patterns ->
/// message patterns -> trailer names -> bot authors.
pub fn detect_ai(commit: &CommitInfo) -> AiSignal {
    let sigs = load_signatures();

    // 1. Check trailer emails (highest confidence)
    for trailer in &commit.trailers {
        for (email, tool) in &sigs.trailer_emails {
            if trailer.contains(email) {
                return AiSignal::detected(tool.clone(), "trailer");
            }
        }
    }

    // 2. Check author email
    for (email, tool) in &sigs.author_emails {
        if commit.author_email.contains(email) {
            return AiSignal::detected(tool.clone(), "author");
        }
    }

    // 3. Check bot authors (exact match, before patterns to avoid false generics)
    if let Some(tool) = sigs.bot_authors.get(&commit.author_name) {
        return AiSignal::detected(tool.clone(), "bot-author");
    }

    // 4. Check author name patterns
    for (pattern, tool) in &sigs.author_patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(&commit.author_name) {
                return AiSignal::detected(tool.clone(), "author-pattern");
            }
        }
    }

    // 5. Check message body patterns
    let full_message = format!("{}\n{}", commit.subject, commit.body);
    for (pattern, tool) in &sigs.message_patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(&full_message) {
                return AiSignal::detected(tool.clone(), "message");
            }
        }
    }

    // 6. Check trailer names (Co-Authored-By: <Name> ...)
    for trailer in &commit.trailers {
        for (name, tool) in &sigs.trailer_names {
            // Match "Co-Authored-By: Name <email>" or "Co-authored-by: Name <email>"
            let name_lower = name.to_lowercase();
            let trailer_lower = trailer.to_lowercase();
            if trailer_lower.contains(&format!(": {name_lower} <"))
                || trailer_lower.contains(&format!(": {name_lower}\n"))
                || trailer_lower.ends_with(&format!(": {name_lower}"))
            {
                return AiSignal::detected(tool.clone(), "trailer-name");
            }
        }
    }

    AiSignal::none()
}

/// Check if an author name is a known bot.
pub fn is_bot(author_name: &str) -> bool {
    let sigs = load_signatures();
    sigs.bot_authors.contains_key(author_name)
}

/// Identify the AI tool from a signal string.
pub fn identify_tool(signal: &str) -> Option<String> {
    let sigs = load_signatures();

    // Check trailer emails
    for (email, tool) in &sigs.trailer_emails {
        if signal.contains(email) {
            return Some(tool.clone());
        }
    }

    // Check author emails
    for (email, tool) in &sigs.author_emails {
        if signal.contains(email) {
            return Some(tool.clone());
        }
    }

    // Check bot authors
    for (name, tool) in &sigs.bot_authors {
        if signal.contains(name) {
            return Some(tool.clone());
        }
    }

    // Check trailer names
    for (name, tool) in &sigs.trailer_names {
        if signal.contains(name) {
            return Some(tool.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_commit(
        author_name: &str,
        author_email: &str,
        subject: &str,
        body: &str,
        trailers: Vec<String>,
    ) -> CommitInfo {
        CommitInfo {
            hash: "abc123".to_string(),
            author_name: author_name.to_string(),
            author_email: author_email.to_string(),
            date: "2026-03-14T10:00:00Z".to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            trailers,
            files: vec![],
        }
    }

    #[test]
    fn test_detect_ai_trailer_email() {
        let commit = make_commit(
            "Alice",
            "alice@example.com",
            "feat: add feature",
            "",
            vec!["Co-Authored-By: Claude <noreply@anthropic.com>".to_string()],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("claude"));
        assert_eq!(signal.method.as_deref(), Some("trailer"));
    }

    #[test]
    fn test_detect_ai_author_email() {
        let commit = make_commit(
            "ReplicUser",
            "no-reply@replit.com",
            "update app",
            "",
            vec![],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("replit"));
        assert_eq!(signal.method.as_deref(), Some("author"));
    }

    #[test]
    fn test_detect_ai_author_pattern() {
        let commit = make_commit(
            "Alice (aider)",
            "alice@example.com",
            "fix: handle null",
            "",
            vec![],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("aider"));
        assert_eq!(signal.method.as_deref(), Some("author-pattern"));
    }

    #[test]
    fn test_detect_ai_message_pattern() {
        let commit = make_commit(
            "Alice",
            "alice@example.com",
            "feat: add feature",
            "Generated with Claude Code",
            vec![],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("claude"));
        assert_eq!(signal.method.as_deref(), Some("message"));
    }

    #[test]
    fn test_detect_ai_bot_author() {
        let commit = make_commit(
            "dependabot[bot]",
            "bot@github.com",
            "chore: bump deps",
            "",
            vec![],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("dependabot"));
        assert_eq!(signal.method.as_deref(), Some("bot-author"));
    }

    #[test]
    fn test_detect_ai_none() {
        let commit = make_commit(
            "Alice",
            "alice@example.com",
            "fix: handle error",
            "Fixed the error handling",
            vec![],
        );
        let signal = detect_ai(&commit);
        assert!(!signal.detected);
        assert!(signal.tool.is_none());
    }

    #[test]
    fn test_is_bot() {
        assert!(is_bot("dependabot[bot]"));
        assert!(is_bot("renovate[bot]"));
        assert!(is_bot("github-actions[bot]"));
        assert!(is_bot("devin-ai-integration[bot]"));
        assert!(!is_bot("alice"));
        assert!(!is_bot("bob"));
    }

    #[test]
    fn test_identify_tool() {
        assert_eq!(
            identify_tool("noreply@anthropic.com"),
            Some("claude".to_string())
        );
        assert_eq!(
            identify_tool("dependabot[bot]"),
            Some("dependabot".to_string())
        );
        assert_eq!(identify_tool("random string"), None);
    }

    #[test]
    fn test_detect_ai_cursor_trailer() {
        let commit = make_commit(
            "Alice",
            "alice@example.com",
            "feat: add feature",
            "",
            vec!["Co-authored-by: Cursor <cursoragent@cursor.com>".to_string()],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("cursor"));
    }

    #[test]
    fn test_detect_ai_aider_message_prefix() {
        let commit = make_commit(
            "Alice",
            "alice@example.com",
            "aider: fix the parser",
            "",
            vec![],
        );
        let signal = detect_ai(&commit);
        assert!(signal.detected);
        assert_eq!(signal.tool.as_deref(), Some("aider"));
        assert_eq!(signal.method.as_deref(), Some("message"));
    }
}
