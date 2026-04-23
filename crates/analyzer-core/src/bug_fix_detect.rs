//! Heuristic bug-fix classification for commit subjects.
//!
//! The original aggregator only counted commits whose Conventional Commit
//! prefix was `fix:`. That misses every project that does not enforce
//! Conventional Commits, plus several repos that *do* use it but file
//! regressions under different verbs (`hotfix`, `revert`, `chore: fix the
//! build`, plain "Fix race in foo", etc.). This module returns true when
//! either:
//!
//!   1. the commit has a Conventional Commit prefix in [`FIX_PREFIXES`], or
//!   2. the commit subject contains a fix-related keyword from
//!      [`SUBJECT_KEYWORDS`], or
//!   3. the commit references an issue closure (`fixes #123`, `closes #45`,
//!      `resolves GH-7`).
//!
//! The keyword list is intentionally case-insensitive and word-boundary
//! aware so that `fix:`, `Fixed`, `FIXES`, and `fixing` all hit, but
//! `prefix`, `affix`, `suffix`, `unfixable` do not.

use crate::types::extract_conventional_prefix;

/// Conventional Commit prefixes that indicate a bug fix.
const FIX_PREFIXES: &[&str] = &["fix", "bugfix", "hotfix", "patch", "revert"];

/// Whole-word keywords (case-insensitive) that indicate a fix when they
/// appear anywhere in the subject. Order is irrelevant; matching is O(n)
/// per subject which is fine for commit-log-sized inputs.
const SUBJECT_KEYWORDS: &[&str] = &[
    "fix",
    "fixed",
    "fixes",
    "fixing",
    "bug",
    "bugfix",
    "hotfix",
    "patch",
    "patched",
    "revert",
    "reverts",
    "reverted",
    "regression",
    "race",
    "deadlock",
    "leak",
    "crash",
    "crashed",
    "oops",
    "typo",
    "mistake",
    "broken",
];

/// Issue-closure verbs that GitHub/GitLab recognize. Followed by `#NNN`
/// or `GH-NNN` in the same subject, this is a strong fix signal.
const CLOSURE_VERBS: &[&str] = &[
    "fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved",
];

/// Returns `true` if the commit subject looks like a bug fix.
pub fn is_bug_fix(subject: &str) -> bool {
    if subject.trim().is_empty() {
        return false;
    }

    if let Some(prefix) = extract_conventional_prefix(subject) {
        if FIX_PREFIXES.contains(&prefix.as_str()) {
            return true;
        }
    }

    let lower = subject.to_ascii_lowercase();

    if has_issue_closure(&lower) {
        return true;
    }

    contains_keyword(&lower)
}

fn contains_keyword(lower_subject: &str) -> bool {
    for kw in SUBJECT_KEYWORDS {
        if find_word(lower_subject, kw) {
            return true;
        }
    }
    false
}

fn has_issue_closure(lower_subject: &str) -> bool {
    for verb in CLOSURE_VERBS {
        let mut start = 0usize;
        while let Some(pos) = lower_subject[start..].find(verb) {
            let abs = start + pos;
            let end = abs + verb.len();
            let left_ok = abs == 0 || !is_word_char(lower_subject.as_bytes()[abs - 1]);
            let right_ok =
                end == lower_subject.len() || !is_word_char(lower_subject.as_bytes()[end]);
            if left_ok && right_ok && followed_by_issue_ref(&lower_subject[end..]) {
                return true;
            }
            start = abs + 1;
        }
    }
    false
}

fn followed_by_issue_ref(rest: &str) -> bool {
    // Allow optional `:` and whitespace, then either `#NNN`, `gh-NNN`, or
    // `<owner>/<repo>#NNN`. We only need a quick check, not a full parser.
    let trimmed = rest.trim_start_matches([':', ' ', '\t']);
    let bytes = trimmed.as_bytes();

    // Skip an optional `<word>/<word>` prefix (cross-repo references).
    let mut idx = 0;
    while idx < bytes.len()
        && (bytes[idx].is_ascii_alphanumeric()
            || bytes[idx] == b'-'
            || bytes[idx] == b'_'
            || bytes[idx] == b'/')
    {
        idx += 1;
    }
    let after_org = if idx > 0 && idx < bytes.len() && bytes[idx - 1] == b'/' {
        &trimmed[idx..]
    } else {
        trimmed
    };
    let bytes = after_org.as_bytes();

    if bytes.starts_with(b"#") && bytes.len() > 1 && bytes[1].is_ascii_digit() {
        return true;
    }
    if (bytes.starts_with(b"gh-") || bytes.starts_with(b"GH-"))
        && bytes.len() > 3
        && bytes[3].is_ascii_digit()
    {
        return true;
    }
    false
}

fn find_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || needle_bytes.len() > bytes.len() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let end = abs + needle.len();
        let left_ok = abs == 0 || !is_word_char(bytes[abs - 1]);
        let right_ok = end == bytes.len() || !is_word_char(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_fix_prefix_hits() {
        assert!(is_bug_fix("fix: handle null pointer"));
        assert!(is_bug_fix("fix(core): retry on timeout"));
        assert!(is_bug_fix("fix!: drop deprecated API"));
        assert!(is_bug_fix("bugfix: off-by-one in loop"));
        assert!(is_bug_fix("hotfix: revert bad migration"));
        assert!(is_bug_fix("patch: clamp out-of-range value"));
        assert!(is_bug_fix("revert: \"feat: thing\""));
    }

    #[test]
    fn non_fix_conventional_prefixes_skip() {
        assert!(!is_bug_fix("feat: add login flow"));
        assert!(!is_bug_fix("docs: update README"));
        assert!(!is_bug_fix("chore(deps): bump tokio to 1.40"));
        assert!(!is_bug_fix("refactor: extract helper"));
        assert!(!is_bug_fix("style: cargo fmt"));
        assert!(!is_bug_fix("test: cover edge case"));
    }

    #[test]
    fn freeform_keywords_hit() {
        assert!(is_bug_fix("Fix crash when parsing empty input"));
        assert!(is_bug_fix("Fixed a race in the worker pool"));
        assert!(is_bug_fix("Fixes deadlock during shutdown"));
        assert!(is_bug_fix("FIXES the broken handler"));
        assert!(is_bug_fix("Bug in token refresh loop"));
        assert!(is_bug_fix("Hotfix for prod outage"));
        assert!(is_bug_fix("revert sketchy commit"));
        assert!(is_bug_fix("regression: pagination off-by-one"));
        assert!(is_bug_fix("Memory leak in cache eviction"));
        assert!(is_bug_fix("Oops, wrong path"));
        assert!(is_bug_fix("typo in error message"));
    }

    #[test]
    fn fix_inside_other_words_does_not_hit() {
        // The classic false-positive set: substring matches that are not fixes.
        assert!(!is_bug_fix("Add prefix support to parser"));
        assert!(!is_bug_fix("Refactor suffix handling"));
        assert!(!is_bug_fix("Affix label to the message"));
        assert!(!is_bug_fix("Document unfixable edge cases"));
        assert!(!is_bug_fix("feat: postfix evaluation"));
    }

    #[test]
    fn issue_closure_phrases_hit() {
        assert!(is_bug_fix("Fixes #123"));
        assert!(is_bug_fix("Closes #42 via the new retry path"));
        assert!(is_bug_fix("Resolves GH-7"));
        assert!(is_bug_fix("Fix: resolves agent-sh/agnix#900"));
        assert!(is_bug_fix("close #1"));
    }

    #[test]
    fn empty_or_whitespace_subject_is_not_a_fix() {
        assert!(!is_bug_fix(""));
        assert!(!is_bug_fix("   "));
        assert!(!is_bug_fix("\t\n"));
    }

    #[test]
    fn mixed_case_keywords_hit() {
        assert!(is_bug_fix("FIX broken handler"));
        assert!(is_bug_fix("BUG in pagination"));
        assert!(is_bug_fix("REGRESSION from yesterday"));
    }

    #[test]
    fn close_without_issue_ref_does_not_hit() {
        // "close" alone is too generic - must be followed by an issue ref
        // to count as a fix. (We do not want "close the modal" to register.)
        assert!(!is_bug_fix("Close the modal on ESC"));
        assert!(!is_bug_fix("Resolves the open question"));
    }
}
