//! Betweenness centrality via Brandes' algorithm (2001).
//!
//! For each source s, run a BFS to compute shortest-path counts σ(s, t) and
//! predecessor sets P(s, t) for every target t. Then accumulate
//! pair-dependencies δ(s, ·) in reverse BFS order:
//!
//!   δ(s, v) = Σ_w:v∈P(s,w) (σ(s,v) / σ(s,w)) * (1 + δ(s, w))
//!
//! Betweenness BC(v) is the sum of δ(s, v) over all source nodes, halved
//! for undirected graphs (each pair counted twice).
//!
//! We use the unweighted variant - edge weights in the co-change graph
//! mean "how related" not "how costly to traverse", so BFS (topological
//! distance) is the correct shortest-path notion. Matches NetworkX's default.

use std::collections::VecDeque;

use petgraph::Undirected;
use petgraph::graph::{Graph, NodeIndex};
use rayon::prelude::*;

/// Per-thread scratch space reused across single-source passes within one
/// rayon worker. Pre-allocating once and clearing between passes avoids
/// the O(V²) allocations the naive Brandes formulation would do.
struct Scratch {
    stack: Vec<usize>,
    predecessors: Vec<Vec<usize>>,
    sigma: Vec<f64>,
    distance: Vec<i64>,
    delta: Vec<f64>,
    queue: VecDeque<usize>,
    bc: Vec<f64>,
}

impl Scratch {
    fn new(n: usize) -> Self {
        Self {
            stack: Vec::with_capacity(n),
            predecessors: vec![Vec::new(); n],
            sigma: vec![0.0; n],
            distance: vec![-1; n],
            delta: vec![0.0; n],
            queue: VecDeque::with_capacity(n),
            bc: vec![0.0; n],
        }
    }

    /// Reset only the per-source state - `bc` accumulates across sources.
    fn reset_for_source(&mut self) {
        self.stack.clear();
        for p in self.predecessors.iter_mut() {
            p.clear();
        }
        self.sigma.fill(0.0);
        self.distance.fill(-1);
        self.delta.fill(0.0);
        self.queue.clear();
    }
}

/// Returns a vector indexed by `NodeIndex` with the unnormalised betweenness
/// score of each node. Endpoints are not included in the count (standard
/// Brandes accumulation).
///
/// Parallelised across source nodes via rayon. Each worker keeps its own
/// scratch space and partial BC accumulator; partials are summed at the end.
/// Output is deterministic - rayon `into_par_iter().fold().reduce()` does
/// not depend on completion order because addition is commutative for the
/// scores we produce.
pub fn betweenness(graph: &Graph<(), f64, Undirected>) -> Vec<f64> {
    let n = graph.node_count();
    if n < 3 {
        return vec![0.0; n];
    }

    let bc = (0..n)
        .into_par_iter()
        .fold(
            || Scratch::new(n),
            |mut s, src| {
                s.reset_for_source();
                single_source_pass(graph, src, &mut s);
                s
            },
        )
        .map(|s| s.bc)
        .reduce(
            || vec![0.0; n],
            |mut acc, partial| {
                for (a, p) in acc.iter_mut().zip(partial.iter()) {
                    *a += *p;
                }
                acc
            },
        );

    // Undirected: each pair (s, t) is counted twice across the two passes.
    bc.into_iter().map(|v| v / 2.0).collect()
}

/// One Brandes single-source pass: BFS for shortest-path counts, then
/// reverse-order accumulation of pair-dependencies.
fn single_source_pass(graph: &Graph<(), f64, Undirected>, src: usize, s: &mut Scratch) {
    s.sigma[src] = 1.0;
    s.distance[src] = 0;
    s.queue.push_back(src);

    while let Some(v) = s.queue.pop_front() {
        s.stack.push(v);
        let v_node: NodeIndex = NodeIndex::new(v);
        for neighbour in graph.neighbors(v_node) {
            let w = neighbour.index();
            if s.distance[w] < 0 {
                s.distance[w] = s.distance[v] + 1;
                s.queue.push_back(w);
            }
            if s.distance[w] == s.distance[v] + 1 {
                s.sigma[w] += s.sigma[v];
                s.predecessors[w].push(v);
            }
        }
    }

    while let Some(w) = s.stack.pop() {
        // Snapshot read once - the mutable borrow on s.delta below would
        // otherwise conflict with the immutable read of s.predecessors[w].
        let preds_len = s.predecessors[w].len();
        for i in 0..preds_len {
            let v = s.predecessors[w][i];
            let contribution = (s.sigma[v] / s.sigma[w]) * (1.0 + s.delta[w]);
            s.delta[v] += contribution;
        }
        if w != src {
            s.bc[w] += s.delta[w];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::Graph;

    #[test]
    fn path_graph_middle_node_has_highest_centrality() {
        // 0 - 1 - 2 - 3 - 4
        let mut g: Graph<(), f64, Undirected> = Graph::new_undirected();
        let nodes: Vec<_> = (0..5).map(|_| g.add_node(())).collect();
        for i in 0..4 {
            g.add_edge(nodes[i], nodes[i + 1], 1.0);
        }

        let bc = betweenness(&g);
        // Middle node should have the highest BC. Endpoints should be zero.
        assert_eq!(bc[0], 0.0);
        assert_eq!(bc[4], 0.0);
        assert!(bc[2] > bc[1]);
        assert!(bc[2] > bc[3]);
    }

    #[test]
    fn star_graph_hub_has_all_centrality() {
        // Hub (0) connected to 4 leaves.
        let mut g: Graph<(), f64, Undirected> = Graph::new_undirected();
        let hub = g.add_node(());
        let leaves: Vec<_> = (0..4).map(|_| g.add_node(())).collect();
        for &leaf in &leaves {
            g.add_edge(hub, leaf, 1.0);
        }

        let bc = betweenness(&g);
        assert!(bc[0] > 0.0);
        for &leaf in &leaves {
            assert_eq!(bc[leaf.index()], 0.0);
        }
    }

    #[test]
    fn disconnected_graph_all_zero() {
        // Two disjoint edges: 0-1 and 2-3.
        let mut g: Graph<(), f64, Undirected> = Graph::new_undirected();
        let n: Vec<_> = (0..4).map(|_| g.add_node(())).collect();
        g.add_edge(n[0], n[1], 1.0);
        g.add_edge(n[2], n[3], 1.0);
        let bc = betweenness(&g);
        for v in bc {
            assert_eq!(v, 0.0);
        }
    }
}
