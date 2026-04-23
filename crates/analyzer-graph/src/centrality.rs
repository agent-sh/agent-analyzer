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

/// Returns a vector indexed by `NodeIndex` with the unnormalised betweenness
/// score of each node. Endpoints are not included in the count (standard
/// Brandes accumulation).
pub fn betweenness(graph: &Graph<(), f64, Undirected>) -> Vec<f64> {
    let n = graph.node_count();
    let mut bc = vec![0.0f64; n];
    if n < 3 {
        return bc;
    }

    for src in 0..n {
        // Brandes' single-source pass.
        let mut stack: Vec<usize> = Vec::with_capacity(n);
        let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut sigma = vec![0.0f64; n];
        let mut distance = vec![-1i64; n];

        sigma[src] = 1.0;
        distance[src] = 0;

        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(src);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            let v_node: NodeIndex = NodeIndex::new(v);
            for neighbour in graph.neighbors(v_node) {
                let w = neighbour.index();
                if distance[w] < 0 {
                    distance[w] = distance[v] + 1;
                    queue.push_back(w);
                }
                if distance[w] == distance[v] + 1 {
                    sigma[w] += sigma[v];
                    predecessors[w].push(v);
                }
            }
        }

        // Accumulation in reverse BFS order.
        let mut delta = vec![0.0f64; n];
        while let Some(w) = stack.pop() {
            for &v in &predecessors[w] {
                delta[v] += (sigma[v] / sigma[w]) * (1.0 + delta[w]);
            }
            if w != src {
                bc[w] += delta[w];
            }
        }
    }

    // Undirected: each pair (s, t) is counted twice across the two passes.
    for v in bc.iter_mut() {
        *v /= 2.0;
    }

    bc
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
