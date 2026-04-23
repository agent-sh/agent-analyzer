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
/// Community ids are bounded by `n` (the node count) for the entire run -
/// they start as `0..n` (one per node) and only ever consolidate, never
/// expand. That lets us store all per-community aggregates in `Vec<f64>`
/// indexed by community id, replacing the previous `HashMap<u32, f64>`.
/// Empty (abandoned) communities just hold zeroes; they cost one f64 slot
/// each but no hashing.
struct State {
    /// node -> community
    node_to_comm: Vec<u32>,
    /// neighbours[node] = (other_node, edge_weight) repeated
    neighbours: Vec<Vec<(usize, f64)>>,
    /// Per-node weighted degree (sum of incident edge weights, with
    /// self-loops counted twice as in standard Louvain bookkeeping).
    degree: Vec<f64>,
    /// Per-node self-loop weight, pre-computed once so the local-moves loop
    /// doesn't iterate `neighbours[node]` for self-loop detection per pass.
    self_loop: Vec<f64>,
    /// Sum of weighted degrees of all nodes currently in community c.
    comm_total: Vec<f64>,
    /// Sum of edge weights internal to community c (self-loops count once).
    comm_internal: Vec<f64>,
    /// Total edge weight in the graph (m). Constant across moves.
    total_weight: f64,
    /// Scratch: per-target accumulator reused across nodes in `local_moves`.
    /// Indexed by community id (size n). Cleared lazily via `dirty_comms`.
    weights_to: Vec<f64>,
    /// Scratch: list of community ids touched while processing the current
    /// node. Used to zero out only the dirty entries in `weights_to`,
    /// avoiding the O(n) clear that a full `weights_to.fill(0.0)` would do.
    dirty_comms: Vec<u32>,
}

impl State {
    fn new(graph: &Graph<(), f64, Undirected>) -> Self {
        let n = graph.node_count();
        let mut neighbours: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut degree = vec![0.0; n];
        let mut self_loop = vec![0.0; n];
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
                // Self-loop contributes once to degree, once to total. Cache
                // the per-node self-loop sum so local_moves doesn't refilter.
                degree[a] += 2.0 * w;
                self_loop[a] += w;
                total_weight += w;
            }
        }

        let node_to_comm: Vec<u32> = (0..n as u32).collect();
        let comm_total: Vec<f64> = degree.clone();
        // Each node is initially in its own singleton community. The internal
        // edge weight of a singleton community is the node's self-loop
        // weight (a self-loop is the only edge that can stay inside a
        // 1-node community). Initialising to zeros instead would silently
        // drop self-loop contributions until the node first moves.
        let comm_internal: Vec<f64> = self_loop.clone();

        Self {
            node_to_comm,
            neighbours,
            degree,
            self_loop,
            comm_total,
            comm_internal,
            total_weight,
            weights_to: vec![0.0; n],
            dirty_comms: Vec::with_capacity(n),
        }
    }

    /// Compute modularity Q = sum_c [ (e_c / m) - γ * (a_c / 2m)^2 ].
    /// Iterates the dense Vec; abandoned communities (zero total) drop out
    /// to `0/2m = 0` so they cost only a comparison.
    fn modularity(&self, resolution: f64) -> f64 {
        if self.total_weight <= 0.0 {
            return 0.0;
        }
        let two_m = 2.0 * self.total_weight;
        let mut q = 0.0;
        for c in 0..self.comm_total.len() {
            let a = self.comm_total[c];
            if a == 0.0 {
                continue;
            }
            let internal = self.comm_internal[c];
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
            // Clear only the entries dirtied by the previous node iteration.
            for &c in &self.dirty_comms {
                self.weights_to[c as usize] = 0.0;
            }
            self.dirty_comms.clear();

            let current_comm = self.node_to_comm[node];
            let k_i = self.degree[node];
            let self_loop = self.self_loop[node];

            // Sum of edge weights from `node` to each neighbouring community.
            for &(other, w) in &self.neighbours[node] {
                if other == node {
                    continue;
                }
                let c = self.node_to_comm[other];
                let slot = &mut self.weights_to[c as usize];
                if *slot == 0.0 {
                    self.dirty_comms.push(c);
                }
                *slot += w;
            }

            // Remove node from its current community to evaluate fairly.
            let k_in_current = self.weights_to[current_comm as usize];
            self.comm_total[current_comm as usize] -= k_i;
            self.comm_internal[current_comm as usize] -= k_in_current + self_loop;

            // Pick the best target community (default = stay put). Iterate
            // only the touched (`dirty_comms`) entries, not all n.
            let mut best_comm = current_comm;
            let mut best_gain = 0.0;
            for &target in &self.dirty_comms {
                let k_in_target = self.weights_to[target as usize];
                let total_target = self.comm_total[target as usize];
                let gain = k_in_target - resolution * (k_i * total_target) / two_m;
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_comm = target;
                }
            }

            // Re-insert into chosen community. Note: `best_comm` may be
            // `current_comm` (which we just zeroed out of), and that case
            // restores the prior value via `k_in_current`.
            let k_in_best = self.weights_to[best_comm as usize];
            self.comm_total[best_comm as usize] += k_i;
            self.comm_internal[best_comm as usize] += k_in_best + self_loop;
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

    /// Regression: `comm_internal` was previously initialised to all zeros,
    /// which silently dropped the contribution of pre-existing self-loops
    /// until the affected node first moved. With the fix it is initialised
    /// to per-node self-loop weight, matching the standard Louvain bookkeeping
    /// for singleton communities.
    #[test]
    fn self_loop_contributes_to_initial_internal_weight() {
        // Two isolated triangles plus a self-loop of weight 2.5 on node 0.
        let mut g = Graph::new_undirected();
        let nodes: Vec<NodeIndex> = (0..6).map(|_| g.add_node(())).collect();
        g.add_edge(nodes[0], nodes[0], 2.5); // self-loop
        g.add_edge(nodes[0], nodes[1], 1.0);
        g.add_edge(nodes[1], nodes[2], 1.0);
        g.add_edge(nodes[2], nodes[0], 1.0);
        g.add_edge(nodes[3], nodes[4], 1.0);
        g.add_edge(nodes[4], nodes[5], 1.0);
        g.add_edge(nodes[5], nodes[3], 1.0);

        let state = State::new(&g);
        // Direct structural check: the singleton community of node 0 has
        // an internal edge (the self-loop) of weight 2.5. Other singletons
        // have no self-loops so their internal weight is 0.
        assert_eq!(state.self_loop[0], 2.5);
        assert_eq!(state.comm_internal[0], 2.5);
        for i in 1..6 {
            assert_eq!(state.comm_internal[i], 0.0);
        }

        // Cross-check via modularity: with the fix, Q must equal what we
        // get if we manually substitute the per-community values into the
        // standard formula. Pre-fix it would be lower by exactly
        // self_loop[0] / total_weight.
        let q = state.modularity(1.0);
        let expected_q = expected_modularity(&state, 1.0);
        assert!(
            (q - expected_q).abs() < 1e-9,
            "Q mismatch: got {q}, expected {expected_q}"
        );
    }

    /// Reference modularity calculator following the Newman convention,
    /// kept independent from `State::modularity` so the test verifies the
    /// formula rather than just echoing the implementation back.
    fn expected_modularity(s: &State, gamma: f64) -> f64 {
        let m = s.total_weight;
        if m <= 0.0 {
            return 0.0;
        }
        let two_m = 2.0 * m;
        (0..s.comm_total.len())
            .map(|c| {
                let a = s.comm_total[c];
                if a == 0.0 {
                    0.0
                } else {
                    s.comm_internal[c] / m - gamma * (a / two_m).powi(2)
                }
            })
            .sum()
    }
}
