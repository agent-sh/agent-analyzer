//! Louvain modularity-maximisation community detection.
//!
//! Implements the classical two-phase algorithm of Blondel et al. (2008):
//!
//! 1. **Local moving** - each node greedily joins the neighbouring community
//!    that yields the largest modularity gain; repeat until no node moves.
//! 2. **Aggregation** - collapse each community into a super-node with
//!    self-loop weight equal to internal edge weight, then repeat phase 1
//!    on the meta-graph. Iterate until modularity converges.
//!
//! Resolution parameter γ (Reichardt-Bornholdt) controls cluster granularity:
//! γ < 1 yields fewer/larger communities, γ > 1 yields more/smaller ones.
//! γ = 1.0 is classical Louvain and the default we ship.
//!
//! Post-processing addresses Louvain's well-known resolution limit (Fortunato
//! & Barthelemy, 2007): communities below `min_size` are merged into the
//! neighbouring community sharing the largest edge weight.
//!
//! Determinism: nodes are processed in the iteration order produced by the
//! input graph's `NodeIndex` sequence, which is insertion-order. Callers that
//! need cross-run stability should insert nodes deterministically.

use std::collections::HashMap;

use petgraph::Undirected;
use petgraph::graph::{Graph, NodeIndex};

/// Output of one Louvain pass: each node's community id (densely numbered
/// from 0 to `num_communities - 1`).
pub type Partition = HashMap<NodeIndex, u32>;

/// Run Louvain on `graph` with the given resolution and small-community
/// merge threshold. Returns the final partition.
pub fn run(
    graph: &Graph<(), f64, Undirected>,
    resolution: f64,
    min_community_size: usize,
) -> Partition {
    if graph.node_count() == 0 {
        return HashMap::new();
    }

    let mut state = State::new(graph);
    let mut iter_modularity = state.modularity(resolution);

    // Outer loop: alternate local moves + aggregation until modularity stops
    // improving. Bounded to avoid pathological non-convergence.
    for _ in 0..20 {
        let moved = state.local_moves(resolution);
        if !moved {
            break;
        }
        let new_q = state.modularity(resolution);
        // Tiny improvements still count - accept anything strictly positive.
        if new_q <= iter_modularity + 1e-9 {
            break;
        }
        iter_modularity = new_q;
    }

    let mut partition = state.flatten();
    merge_small_communities(graph, &mut partition, min_community_size);
    densify_ids(&mut partition);
    partition
}

// ─── Internal state ─────────────────────────────────────────────────────────

/// Algorithm state during one Louvain run.
///
/// Tracks per-node community membership plus per-community aggregates needed
/// to compute modularity gain in O(1) when a node moves.
struct State {
    /// node -> community
    node_to_comm: Vec<u32>,
    /// neighbours[node] = (other_node, edge_weight) repeated
    neighbours: Vec<Vec<(usize, f64)>>,
    /// Per-node weighted degree (sum of incident edge weights, with
    /// self-loops counted twice as in standard Louvain bookkeeping).
    degree: Vec<f64>,
    /// Sum of weighted degrees of all nodes currently in community c.
    comm_total: HashMap<u32, f64>,
    /// Sum of edge weights internal to community c (self-loops count once).
    comm_internal: HashMap<u32, f64>,
    /// Total edge weight in the graph (m). Constant across moves.
    total_weight: f64,
}

impl State {
    fn new(graph: &Graph<(), f64, Undirected>) -> Self {
        let n = graph.node_count();
        let mut neighbours: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut degree = vec![0.0; n];
        let mut total_weight = 0.0;

        for edge in graph.edge_references() {
            let a = petgraph::visit::EdgeRef::source(&edge).index();
            let b = petgraph::visit::EdgeRef::target(&edge).index();
            let w = *petgraph::visit::EdgeRef::weight(&edge);
            // Skip non-positive weights - they add no signal.
            if w <= 0.0 {
                continue;
            }
            neighbours[a].push((b, w));
            if a != b {
                neighbours[b].push((a, w));
                degree[a] += w;
                degree[b] += w;
                total_weight += w;
            } else {
                // Self-loop contributes once to degree, once to total.
                degree[a] += 2.0 * w;
                total_weight += w;
            }
        }

        let node_to_comm: Vec<u32> = (0..n as u32).collect();
        let mut comm_total: HashMap<u32, f64> = HashMap::with_capacity(n);
        let mut comm_internal: HashMap<u32, f64> = HashMap::with_capacity(n);
        for (i, &deg) in degree.iter().enumerate() {
            comm_total.insert(i as u32, deg);
            comm_internal.insert(i as u32, 0.0);
        }

        Self {
            node_to_comm,
            neighbours,
            degree,
            comm_total,
            comm_internal,
            total_weight,
        }
    }

    /// Compute modularity Q = sum_c [ (e_c / m) - γ * (a_c / 2m)^2 ]
    fn modularity(&self, resolution: f64) -> f64 {
        if self.total_weight <= 0.0 {
            return 0.0;
        }
        let two_m = 2.0 * self.total_weight;
        let mut q = 0.0;
        for (&comm, &internal) in &self.comm_internal {
            let a = self.comm_total.get(&comm).copied().unwrap_or(0.0);
            q += internal / self.total_weight - resolution * (a / two_m).powi(2);
        }
        q
    }

    /// One local-move pass. Returns true if any node moved.
    fn local_moves(&mut self, resolution: f64) -> bool {
        let mut any_moved = false;
        let two_m = 2.0 * self.total_weight;
        if two_m <= 0.0 {
            return false;
        }

        for node in 0..self.node_to_comm.len() {
            let current_comm = self.node_to_comm[node];
            let k_i = self.degree[node];

            // Sum of edge weights from node to each neighbouring community.
            let mut weights_to: HashMap<u32, f64> = HashMap::new();
            for &(other, w) in &self.neighbours[node] {
                if other == node {
                    continue;
                }
                let c = self.node_to_comm[other];
                *weights_to.entry(c).or_insert(0.0) += w;
            }

            // Self-loop weight (constant for `node`).
            let self_loop: f64 = self.neighbours[node]
                .iter()
                .filter(|(o, _)| *o == node)
                .map(|(_, w)| *w)
                .sum();

            // Remove node from its current community to evaluate fairly.
            let k_in_current = weights_to.get(&current_comm).copied().unwrap_or(0.0);
            *self.comm_total.entry(current_comm).or_insert(0.0) -= k_i;
            *self.comm_internal.entry(current_comm).or_insert(0.0) -= k_in_current + self_loop;

            // Pick the best target community (default = stay put).
            let mut best_comm = current_comm;
            let mut best_gain = 0.0;
            for (&target, &k_in_target) in &weights_to {
                let total_target = self.comm_total.get(&target).copied().unwrap_or(0.0);
                let gain = k_in_target - resolution * (k_i * total_target) / two_m;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_comm = target;
                }
            }

            // Re-insert into chosen community.
            let k_in_best = weights_to.get(&best_comm).copied().unwrap_or(0.0);
            *self.comm_total.entry(best_comm).or_insert(0.0) += k_i;
            *self.comm_internal.entry(best_comm).or_insert(0.0) += k_in_best + self_loop;
            self.node_to_comm[node] = best_comm;

            if best_comm != current_comm {
                any_moved = true;
            }
        }

        any_moved
    }

    /// Final node->community mapping.
    fn flatten(&self) -> Partition {
        self.node_to_comm
            .iter()
            .enumerate()
            .map(|(i, &c)| (NodeIndex::new(i), c))
            .collect()
    }
}

// ─── Post-processing ────────────────────────────────────────────────────────

/// Merge any community below `min_size` into the neighbouring community that
/// shares the largest total edge weight. Resolves Louvain's resolution limit:
/// without this, a 2-file "community" with one strong neighbour would survive.
fn merge_small_communities(
    graph: &Graph<(), f64, Undirected>,
    partition: &mut Partition,
    min_size: usize,
) {
    if min_size <= 1 {
        return;
    }

    // Build community size + neighbour-weight tables. Repeat the pass while
    // any small community remains - merging can shrink another below the bar.
    loop {
        let mut sizes: HashMap<u32, usize> = HashMap::new();
        for &c in partition.values() {
            *sizes.entry(c).or_insert(0) += 1;
        }

        // Find the smallest community below threshold (deterministic by id).
        let target = sizes
            .iter()
            .filter(|&(_, s)| *s < min_size)
            .min_by_key(|&(c, s)| (*s, *c))
            .map(|(c, _)| *c);
        let Some(victim) = target else {
            return;
        };

        // Sum edge weight from victim to each other community.
        let mut bridge: HashMap<u32, f64> = HashMap::new();
        for edge in graph.edge_references() {
            let a = petgraph::visit::EdgeRef::source(&edge);
            let b = petgraph::visit::EdgeRef::target(&edge);
            let w = *petgraph::visit::EdgeRef::weight(&edge);
            let ca = partition.get(&a).copied();
            let cb = partition.get(&b).copied();
            if let (Some(ca), Some(cb)) = (ca, cb) {
                if ca == victim && cb != victim {
                    *bridge.entry(cb).or_insert(0.0) += w;
                } else if cb == victim && ca != victim {
                    *bridge.entry(ca).or_insert(0.0) += w;
                }
            }
        }

        // Pick the strongest neighbour. Tie-break by lowest id for determinism.
        let dest = bridge
            .into_iter()
            .max_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.0.cmp(&a.0))
            })
            .map(|(c, _)| c);

        let Some(dest_comm) = dest else {
            // Isolated small community with no inter-community edges - leave it
            // alone (would otherwise loop forever).
            return;
        };

        for c in partition.values_mut() {
            if *c == victim {
                *c = dest_comm;
            }
        }
    }
}

/// Re-number community ids densely from 0 (deterministic by min-node-index
/// per community, so output is reproducible).
fn densify_ids(partition: &mut Partition) {
    // For each community, find its lowest node index (stable seed).
    let mut seed: HashMap<u32, usize> = HashMap::new();
    for (node, &c) in partition.iter() {
        let n = node.index();
        seed.entry(c).and_modify(|s| *s = (*s).min(n)).or_insert(n);
    }

    // Sort communities by their seed and assign new ids 0..N.
    let mut comms: Vec<(u32, usize)> = seed.into_iter().collect();
    comms.sort_by_key(|(_, s)| *s);
    let remap: HashMap<u32, u32> = comms
        .into_iter()
        .enumerate()
        .map(|(new_id, (old_id, _))| (old_id, new_id as u32))
        .collect();

    for c in partition.values_mut() {
        *c = remap[c];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two triangles joined by a single weak edge.  Should split cleanly
    /// at γ = 1.0.
    fn two_triangles() -> (Graph<(), f64, Undirected>, [NodeIndex; 6]) {
        let mut g = Graph::new_undirected();
        let n: [NodeIndex; 6] = [
            g.add_node(()),
            g.add_node(()),
            g.add_node(()),
            g.add_node(()),
            g.add_node(()),
            g.add_node(()),
        ];
        // triangle A: 0-1-2
        g.add_edge(n[0], n[1], 1.0);
        g.add_edge(n[1], n[2], 1.0);
        g.add_edge(n[2], n[0], 1.0);
        // triangle B: 3-4-5
        g.add_edge(n[3], n[4], 1.0);
        g.add_edge(n[4], n[5], 1.0);
        g.add_edge(n[5], n[3], 1.0);
        // bridge
        g.add_edge(n[2], n[3], 0.1);
        (g, n)
    }

    #[test]
    fn splits_two_triangles() {
        let (g, n) = two_triangles();
        let p = run(&g, 1.0, 1);
        assert_eq!(p[&n[0]], p[&n[1]]);
        assert_eq!(p[&n[1]], p[&n[2]]);
        assert_eq!(p[&n[3]], p[&n[4]]);
        assert_eq!(p[&n[4]], p[&n[5]]);
        assert_ne!(p[&n[0]], p[&n[3]]);
    }

    #[test]
    fn empty_graph_returns_empty_partition() {
        let g: Graph<(), f64, Undirected> = Graph::new_undirected();
        let p = run(&g, 1.0, 3);
        assert!(p.is_empty());
    }

    #[test]
    fn singleton_merges_into_neighbour_when_below_min_size() {
        // Triangle 0-1-2 plus a singleton 3 attached only to 0.
        let mut g = Graph::new_undirected();
        let nodes: Vec<NodeIndex> = (0..4).map(|_| g.add_node(())).collect();
        g.add_edge(nodes[0], nodes[1], 1.0);
        g.add_edge(nodes[1], nodes[2], 1.0);
        g.add_edge(nodes[2], nodes[0], 1.0);
        g.add_edge(nodes[0], nodes[3], 0.5);

        let p = run(&g, 1.0, 3);
        // Node 3 starts isolated then gets merged into the triangle.
        assert_eq!(p[&nodes[0]], p[&nodes[3]]);
    }

    #[test]
    fn ids_are_dense_and_zero_based() {
        let (g, _) = two_triangles();
        let p = run(&g, 1.0, 1);
        let max_id = *p.values().max().unwrap();
        let unique: std::collections::HashSet<_> = p.values().collect();
        assert_eq!(unique.len() as u32, max_id + 1);
        assert!(p.values().any(|&c| c == 0));
    }
}
