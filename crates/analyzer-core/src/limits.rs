//! Shared size/length caps for defense-in-depth bounds checking.
//!
//! These constants are centralized so analyzer-embed and analyzer-graph
//! (and future crates) agree on the thresholds. The rationale for each
//! cap is inline below.

/// Maximum size, in bytes, of a single file we are willing to read into
/// memory during a repo walk.
///
/// The slop/embed analyzers read the whole file to parse with tree-sitter
/// or hash for embeddings, so unbounded sizes are a DoS vector. 5 MiB
/// comfortably covers the legitimate long tail of source files; generated
/// or vendored files past this are almost always noise for our analyses.
pub const MAX_WALK_FILE_SIZE: u64 = 5 * 1024 * 1024;

/// Cap on symbol/name strings stored in the embed sidecar. Names here
/// are function / type identifiers, not paths.
///
/// 1 KiB is already generous; real identifiers are far shorter. Used to
/// bound memory when reading attacker-controlled length-prefixed strings
/// out of the sidecar binary format.
pub const MAX_NAME_LEN: usize = 1024;

/// Cap on filesystem path strings stored in the embed sidecar.
///
/// Paths can be genuinely long in deeply nested monorepos (workspace
/// packages, node_modules-style layouts), so 1 KiB (the `MAX_NAME_LEN`
/// used for identifiers) is too tight. 4096 matches PATH_MAX on Linux
/// and is far larger than any real code path we would embed.
pub const MAX_PATH_LEN: usize = 4096;
