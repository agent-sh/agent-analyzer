//! Graph-derived analytics for repo-intel.
//!
//! Builds three graphs from the existing `RepoIntelData`:
//!
//! * **co-change graph** (Phase 5.1) - undirected, file-file weighted by Jaccard
//!   similarity over commit co-occurrence; partitioned into communities via
//!   Louvain modularity maximisation.
//! * **import graph centrality** (Phase 5.2 - placeholder) - PageRank, SCC,
//!   blast radius over the directed import/call graph already collected by
//!   `analyzer-repo-map`.
//! * **author-file authority** (Phase 5.3 - placeholder) - HITS-style scoring
//!   over the author-file bipartite graph.
//!
//! Phase 5.1 ships first; the other two have their data slots reserved in
//! `analyzer_core::types::GraphData` so older readers stay compatible.

pub mod centrality;
pub mod cochange;
pub mod louvain;
pub mod queries;

use analyzer_core::types::RepoIntelData;

/// Run every available graph analysis pass and store results in `map.graph`.
///
/// Currently builds: co-change graph + Louvain communities + betweenness.
///
/// Safe to re-run: overwrites `map.graph.cochange` each time. No-op data
/// (e.g. repos with too little history) returns `None` slots rather than
/// stub values.
pub fn finalize(map: &mut RepoIntelData) {
    let mut graph = map.graph.take().unwrap_or_default();

    graph.cochange = cochange::build(map);

    map.graph = Some(graph);
}
