//! Co-change graph construction.
//!
//! Reads `RepoIntelData.coupling` (already pruned to ≥3 raw co-changes by the
//! aggregator) plus `file_activity[*].changes` and produces a sparse
//! undirected weighted graph where:
//!
//!   weight(a, b) = jaccard = |A ∩ B| / |A ∪ B|
//!
//! with |A ∩ B| = `coupling[a][b].cochanges` and
//! |A ∪ B| = `changes[a] + changes[b] - cochanges`.
//!
//! Edges below `min_jaccard` or `min_cochanges` are dropped. The remaining
//! edges + Louvain partition + betweenness centrality are returned as a
//! [`CochangeGraph`] ready to slot into `RepoIntelData.graph.cochange`.

use std::collections::HashMap;

use petgraph::Undirected;
use petgraph::graph::{Graph, NodeIndex};

use analyzer_core::types::{CochangeEdge, CochangeGraph, CochangeParams, RepoIntelData};

use crate::{centrality, louvain};

/// Build the co-change graph for `map`. Returns `None` when there is not
/// enough data to compute meaningful communities (no surviving edges after
/// thresholding).
pub fn build(map: &RepoIntelData) -> Option<CochangeGraph> {
    let params = CochangeParams::default();
    build_with(map, params)
}

/// Same as [`build`] but with explicit parameters - exposed for tests and
/// future tuning hooks.
pub fn build_with(map: &RepoIntelData, params: CochangeParams) -> Option<CochangeGraph> {
    // Collect file -> total changes for the Jaccard denominator. Only files
    // that survived noise filtering during aggregation appear here.
    let changes: HashMap<&str, u64> = map
        .file_activity
        .iter()
        .map(|(p, a)| (p.as_str(), a.changes))
        .collect();

    // Walk the existing coupling map to build (a, b, cochanges, jaccard).
    let mut edge_records: Vec<CochangeEdge> = Vec::new();
    for (a, neighbours) in &map.coupling {
        let Some(&ca) = changes.get(a.as_str()) else {
            continue;
        };
        for (b, entry) in neighbours {
            // Coupling is stored canonically with a < b - skip the inverse
            // direction so we don't double-count.
            if a >= b {
                continue;
            }
            let Some(&cb) = changes.get(b.as_str()) else {
                continue;
            };
            let cochanges = entry.cochanges;
            if cochanges < params.min_cochanges {
                continue;
            }
            let union = ca + cb - cochanges;
            if union == 0 {
                continue;
            }
            let jaccard = cochanges as f64 / union as f64;
            if jaccard < params.min_jaccard {
                continue;
            }
            edge_records.push(CochangeEdge {
                a: a.clone(),
                b: b.clone(),
                jaccard,
                cochanges,
            });
        }
    }

    if edge_records.is_empty() {
        return None;
    }

    // Sort edges deterministically (a, b) so node insertion order matches the
    // sort - keeps Louvain output stable across runs.
    edge_records.sort_by(|x, y| x.a.cmp(&y.a).then(x.b.cmp(&y.b)));

    // Build petgraph, mapping path -> NodeIndex (insertion-ordered).
    let mut graph: Graph<(), f64, Undirected> = Graph::new_undirected();
    let mut path_to_node: HashMap<String, NodeIndex> = HashMap::new();
    let mut node_to_path: HashMap<NodeIndex, String> = HashMap::new();

    for edge in &edge_records {
        let a_idx = *path_to_node.entry(edge.a.clone()).or_insert_with(|| {
            let n = graph.add_node(());
            node_to_path.insert(n, edge.a.clone());
            n
        });
        let b_idx = *path_to_node.entry(edge.b.clone()).or_insert_with(|| {
            let n = graph.add_node(());
            node_to_path.insert(n, edge.b.clone());
            n
        });
        graph.add_edge(a_idx, b_idx, edge.jaccard);
    }

    // Louvain partition with our resolution + small-community merge.
    let partition = louvain::run(&graph, params.louvain_resolution, params.min_community_size);

    // Communities: u32 -> [paths].
    let mut communities: HashMap<u32, Vec<String>> = HashMap::new();
    let mut file_to_community: HashMap<String, u32> = HashMap::new();
    for (node, comm) in &partition {
        let path = node_to_path[node].clone();
        communities.entry(*comm).or_default().push(path.clone());
        file_to_community.insert(path, *comm);
    }
    for v in communities.values_mut() {
        v.sort();
    }

    // Betweenness for boundary detection. Vector indexed by NodeIndex order.
    // Unweighted Brandes (BFS) - edge weights mean "how related" not "how
    // costly to traverse", so structural distance is the right notion.
    let bc_raw: Vec<f64> = centrality::betweenness(&graph);
    let mut betweenness: HashMap<String, f64> = HashMap::with_capacity(bc_raw.len());
    for (idx, &score) in bc_raw.iter().enumerate() {
        if let Some(path) = node_to_path.get(&NodeIndex::new(idx)) {
            betweenness.insert(path.clone(), score);
        }
    }

    Some(CochangeGraph {
        edges: edge_records,
        communities,
        file_to_community,
        betweenness,
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyzer_core::types::{CouplingEntry, FileActivity};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_map(
        file_changes: &[(&str, u64)],
        coupling_pairs: &[(&str, &str, u64)],
    ) -> RepoIntelData {
        let now = Utc::now();
        let mut data = RepoIntelData {
            version: "1.0".into(),
            generated: now,
            updated: now,
            partial: false,
            git: analyzer_core::types::GitInfo {
                analyzed_up_to: String::new(),
                total_commits_analyzed: 0,
                first_commit_date: String::new(),
                last_commit_date: String::new(),
                scope: None,
                shallow: false,
            },
            contributors: analyzer_core::types::Contributors {
                humans: HashMap::new(),
                bots: HashMap::new(),
            },
            file_activity: HashMap::new(),
            coupling: HashMap::new(),
            conventions: analyzer_core::types::ConventionInfo {
                prefixes: HashMap::new(),
                style: "unknown".into(),
                uses_scopes: false,
                naming_patterns: None,
                test_patterns: None,
            },
            releases: analyzer_core::types::Releases {
                tags: vec![],
                cadence: "unknown".into(),
            },
            renames: vec![],
            deletions: vec![],
            symbols: None,
            import_graph: None,
            project: None,
            doc_refs: None,
            graph: None,
        };

        for (path, changes) in file_changes {
            data.file_activity.insert(
                (*path).to_string(),
                FileActivity {
                    changes: *changes,
                    recent_changes: 0,
                    authors: vec![],
                    created: String::new(),
                    last_changed: String::new(),
                    additions: 0,
                    deletions: 0,
                    bug_fix_changes: 0,
                    refactor_changes: 0,
                    last_bug_fix: String::new(),
                    generated: false,
                },
            );
        }

        for (a, b, cochanges) in coupling_pairs {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            data.coupling.entry((*lo).to_string()).or_default().insert(
                (*hi).to_string(),
                CouplingEntry {
                    cochanges: *cochanges,
                },
            );
        }
        data
    }

    #[test]
    fn returns_none_when_no_edges_pass_threshold() {
        // 2 files, 1 co-change - below min_cochanges=3.
        let map = make_map(&[("a.rs", 5), ("b.rs", 5)], &[("a.rs", "b.rs", 1)]);
        assert!(build(&map).is_none());
    }

    #[test]
    fn drops_low_jaccard_pairs() {
        // a/b co-change 3 times but a is touched 100 times - Jaccard ~= 3/103 = 0.029.
        // Below default 0.05 threshold.
        let map = make_map(&[("a.rs", 100), ("b.rs", 5)], &[("a.rs", "b.rs", 3)]);
        assert!(build(&map).is_none());
    }

    #[test]
    fn keeps_strong_pair() {
        // 4 co-changes, a and b each have 5 total. Jaccard = 4/(5+5-4) = 0.667.
        let map = make_map(&[("a.rs", 5), ("b.rs", 5)], &[("a.rs", "b.rs", 4)]);
        let g = build(&map).expect("graph should exist");
        assert_eq!(g.edges.len(), 1);
        assert!(g.edges[0].jaccard > 0.6);
        assert_eq!(g.communities.len(), 1);
        assert_eq!(g.file_to_community.len(), 2);
    }

    #[test]
    fn discovers_two_clusters() {
        // Tight cluster {a, b, c} + tight cluster {d, e, f} + weak bridge a-d.
        let files = [
            ("a.rs", 6),
            ("b.rs", 6),
            ("c.rs", 6),
            ("d.rs", 6),
            ("e.rs", 6),
            ("f.rs", 6),
        ];
        let coupling = [
            ("a.rs", "b.rs", 5),
            ("a.rs", "c.rs", 5),
            ("b.rs", "c.rs", 5),
            ("d.rs", "e.rs", 5),
            ("d.rs", "f.rs", 5),
            ("e.rs", "f.rs", 5),
            // bridge - low Jaccard but above threshold
            ("a.rs", "d.rs", 3),
        ];
        let map = make_map(&files, &coupling);
        let g = build(&map).expect("graph should exist");

        let comm_a = g.file_to_community["a.rs"];
        let comm_d = g.file_to_community["d.rs"];
        assert_ne!(
            comm_a, comm_d,
            "two triangles should land in distinct communities"
        );
        assert_eq!(g.file_to_community["b.rs"], comm_a);
        assert_eq!(g.file_to_community["c.rs"], comm_a);
        assert_eq!(g.file_to_community["e.rs"], comm_d);
        assert_eq!(g.file_to_community["f.rs"], comm_d);
    }

    #[test]
    fn betweenness_flags_bridge_files() {
        // Same two-triangle setup - the bridge edge endpoints (a, d) should
        // have non-zero betweenness; other nodes should have zero.
        let files = [
            ("a.rs", 6),
            ("b.rs", 6),
            ("c.rs", 6),
            ("d.rs", 6),
            ("e.rs", 6),
            ("f.rs", 6),
        ];
        let coupling = [
            ("a.rs", "b.rs", 5),
            ("a.rs", "c.rs", 5),
            ("b.rs", "c.rs", 5),
            ("d.rs", "e.rs", 5),
            ("d.rs", "f.rs", 5),
            ("e.rs", "f.rs", 5),
            ("a.rs", "d.rs", 3),
        ];
        let map = make_map(&files, &coupling);
        let g = build(&map).expect("graph should exist");
        assert!(g.betweenness["a.rs"] > 0.0);
        assert!(g.betweenness["d.rs"] > 0.0);
    }
}
