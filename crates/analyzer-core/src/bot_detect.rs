//! Identify automation/bot author names so the aggregator can split bot
//! contributors from human contributors.
//!
//! This is intentionally narrow - it only detects whether an author name
//! looks like a bot. It does NOT classify "AI-ness" (that bucket got
//! deprecated as a meaningful signal because:
//!   - inline AI-assisted commits leave no signature in author/trailers
//!   - automation bots like dependabot are not "AI" in any useful sense
//!   - the resulting `aiRatio` was both over- and under-counting depending
//!     on the question being asked, with no fix that doesn't require
//!     instrumentation we don't have).
//!
//! Bot detection survives because it serves a structural purpose:
//! `dependabot[bot]` would otherwise inflate the human bus-factor count
//! and pollute ownership queries.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Author names recognised as automation bots. Keep narrow - the trailing
/// `[bot]` pattern catches most cases anyway; explicit entries cover the
/// few that don't follow that convention.
const KNOWN_BOTS: &[&str] = &[
    "dependabot[bot]",
    "renovate[bot]",
    "github-actions[bot]",
    "devin-ai-integration[bot]",
    "copilot-swe-agent[bot]",
    "Copilot",
    "agent-core-bot",
];

fn known_bot_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| KNOWN_BOTS.iter().copied().collect())
}

/// True if the author name belongs to a known automation bot.
///
/// Matches by exact name first (covers explicit entries) then falls back
/// to the `[bot]` suffix convention used by GitHub-integrated automation.
pub fn is_bot(author_name: &str) -> bool {
    if known_bot_set().contains(author_name) {
        return true;
    }
    author_name.ends_with("[bot]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_bots() {
        assert!(is_bot("dependabot[bot]"));
        assert!(is_bot("github-actions[bot]"));
        assert!(is_bot("agent-core-bot"));
        assert!(is_bot("Copilot"));
    }

    #[test]
    fn matches_bot_suffix_convention() {
        assert!(is_bot("some-other-tool[bot]"));
    }

    #[test]
    fn rejects_humans() {
        assert!(!is_bot("Avi Fenesh"));
        assert!(!is_bot("alice"));
        assert!(!is_bot("[bot]starts but no end"));
    }
}
