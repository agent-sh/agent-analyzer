//! Phase 4 - Doc-code cross-reference and sync checking.
//!
//! Parses markdown files for code references, matches them against
//! a symbol table, and detects stale references.

mod checker;
mod matcher;
mod parser;
pub mod queries;
