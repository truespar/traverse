//! Steiner tree: minimum-weight tree connecting a set of terminals.
//!
//! # Algorithm
//!
//! Exact Steiner trees are NP-hard; this is **Mehlhorn's (1988) shortest-path
//! heuristic**, a 2-approximation:
//!
//! 1. multi-source Dijkstra from all terminals at once, labelling each node with
//!    its nearest terminal;
//! 2. a "distance graph" between terminals, edge (t_i, t_j) weighted by their
//!    shortest-path distance, built from every original edge (u, v) whose
//!    endpoints have *different* nearest terminals (a superset of the needed
//!    edges);
//! 3. its MST (Kruskal);
//! 4. each MST edge expanded back into its shortest path; the union of nodes +
//!    edges is the tree.
//!
//! Within 2 x OPT, usually much closer in practice.
//!
//! # Notes
//!
//! - Multi-source Dijkstra amortizes heap work versus n single runs.
//! - The "Voronoi edge" walk in step 2 builds the k^2 terminal distance graph in
//!   one pass over the m original edges.
//! - Union-find for Kruskal.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::EntitySetResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Approximate minimum Steiner tree over a terminal set, by Mehlhorn's shortest-path
/// 2-approximation (`traverse.steinerTree.*`). Stateless.
pub struct SteinerTree;

/// Parsed `steinerTree` configuration.
pub struct SteinerConfig {
    /// Root terminal (`sourceNodeId`, required).
    pub source: NodeId,
    /// Other terminals to connect (`targetNodeIds`, required); duplicates removed.
    pub targets: Vec<NodeId>,
}

impl GraphAlgorithm for SteinerTree {
    type Config = SteinerConfig;
    type Output = EntitySetResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "steinerTree",
            display_name: "Steiner Tree (Mehlhorn 2-approx)",
            category: Category::Paths,
            output_kind: OutputKind::EntitySet,
            stability: Stability::Beta,
            description: "Minimum-weight tree connecting `sourceNodeId` and `targetNodeIds` via Mehlhorn's \
                 shortest-path heuristic: multi-source Dijkstra labels each node with its \
                 nearest terminal, then MST on the induced inter-terminal distance graph. \
                 2-approximation of the NP-hard exact solution.",
            params: vec![
                ConfigParam {
                    key: "sourceNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Required. Source / root terminal.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "targetNodeIds",
                    ty: ConfigParamType::IntegerList,
                    default: CypherValue::Null,
                    description: "Required. Other terminals to connect (list of node ids).",
                    advanced: false,
                    required: true,
                },
            ],
            output_columns: OutputKind::EntitySet.default_columns().unwrap(),
            stats_columns: OutputKind::EntitySet.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<SteinerConfig, AlgorithmError> {
        let source_val = params
            .get("sourceNodeId")
            .ok_or_else(|| AlgorithmError::Config("sourceNodeId is required".into()))?;
        let source = match source_val {
            CypherValue::Integer(i) => *i as u64,
            _ => {
                return Err(AlgorithmError::Config(
                    "sourceNodeId must be an integer".into(),
                ));
            }
        };
        let targets_val = params
            .get("targetNodeIds")
            .ok_or_else(|| AlgorithmError::Config("targetNodeIds is required".into()))?;
        let targets = match targets_val {
            CypherValue::List(l) => l
                .iter()
                .map(|v| match v {
                    CypherValue::Integer(i) => Ok(NodeId(*i as u64)),
                    _ => Err(AlgorithmError::Config(
                        "targetNodeIds entries must be integers".into(),
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(AlgorithmError::Config(
                    "targetNodeIds must be a list".into(),
                ));
            }
        };
        if targets.is_empty() {
            return Err(AlgorithmError::Config(
                "targetNodeIds must contain at least one node".into(),
            ));
        }
        Ok(SteinerConfig {
            source: NodeId(source),
            targets,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &SteinerConfig,
        cancel: &CancellationToken,
    ) -> Result<EntitySetResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;

        // terminal set + assignment
        let mut terminals: Vec<NodeId> = Vec::with_capacity(cfg.targets.len() + 1);
        terminals.push(cfg.source);
        terminals.extend(&cfg.targets);
        terminals.sort_by_key(|n| n.0);
        terminals.dedup();
        let n_term = terminals.len();
        if n_term == 1 {
            return Ok(EntitySetResult {
                nodes: terminals,
                edges: Vec::new(),
            });
        }

        // phase 1: multi-source Dijkstra; each node ends with (nearest_terminal_idx,
        // dist_to_nearest, pred_in_path, pred_edge)
        const SENTINEL: u32 = u32::MAX;
        let mut nearest: Vec<u32> = vec![SENTINEL; cap];
        let mut dist: Vec<f64> = vec![f64::INFINITY; cap];
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];

        let mut heap: BinaryHeap<Reverse<(OrderedF64, NodeId)>> = BinaryHeap::new();
        for (ti, &t) in terminals.iter().enumerate() {
            let idx = t.0 as usize;
            if idx >= cap {
                continue;
            }
            dist[idx] = 0.0;
            nearest[idx] = ti as u32;
            heap.push(Reverse((OrderedF64(0.0), t)));
        }
        while let Some(Reverse((OrderedF64(d_u), u))) = heap.pop() {
            let ui = u.0 as usize;
            if d_u > dist[ui] {
                continue;
            }
            cancel.check()?;
            let near_u = nearest[ui];
            if graph.is_unit_weighted()
                && let Some(slice) = graph.outgoing_adj_slice(u)
            {
                for ep in slice {
                    let nd = d_u + 1.0;
                    let vi = ep.neighbor().0 as usize;
                    if vi < cap && nd < dist[vi] {
                        dist[vi] = nd;
                        nearest[vi] = near_u;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = ep.edge_id().0;
                        heap.push(Reverse((OrderedF64(nd), NodeId(vi as u64))));
                    }
                }
                continue;
            }
            let mut err: Option<AlgorithmError> = None;
            graph.for_each_out(u, &mut |v, eid, w| {
                if err.is_some() {
                    return;
                }
                if w < 0.0 {
                    err = Some(AlgorithmError::Other(format!(
                        "Steiner tree requires non-negative edge weights; saw {w}"
                    )));
                    return;
                }
                let nd = d_u + w;
                let vi = v.0 as usize;
                if vi < cap && nd < dist[vi] {
                    dist[vi] = nd;
                    nearest[vi] = near_u;
                    pred[vi] = u.0 as u32;
                    pred_edge[vi] = eid;
                    heap.push(Reverse((OrderedF64(nd), v)));
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }

        // phase 2: terminal distance graph: each original edge (u, v) with different nearest
        // terminals proposes an edge weighted dist[u] + w + dist[v]
        let mut term_edges: Vec<(f64, u32, u32, Vec<NodeId>, Vec<EdgeId>)> = Vec::new();
        // per (t_a, t_b) keep the lightest proposal (several original edges may propose the same
        // pair)
        let mut best_pair: FxHashMap<(u32, u32), usize> = FxHashMap::default();

        for u in graph.active_nodes() {
            let ui = u.0 as usize;
            let near_u = nearest[ui];
            if near_u == SENTINEL || !dist[ui].is_finite() {
                continue;
            }
            cancel.check()?;
            graph.for_each_out(u, &mut |v, eid, w| {
                let vi = v.0 as usize;
                let near_v = nearest[vi];
                if near_v == SENTINEL || near_v == near_u || !dist[vi].is_finite() {
                    return;
                }
                let total = dist[ui] + w + dist[vi];
                // canonical (a, b) order
                let (a, b) = if near_u < near_v {
                    (near_u, near_v)
                } else {
                    (near_v, near_u)
                };
                // path terminal_a -> ... -> u -> v -> ... -> terminal_b
                let path_u = path_to_terminal(u, &nearest, &pred, &pred_edge, &terminals);
                let path_v = path_to_terminal(v, &nearest, &pred, &pred_edge, &terminals);
                // path_to_terminal gives terminal -> node, combined into the node/edge lists below
                let (nodes_a, edges_a) = path_u;
                let (nodes_b, edges_b) = path_v;
                // the u-v edge connects the halves
                let mut node_set: Vec<NodeId> = nodes_a;
                node_set.extend(nodes_b.iter().rev());
                let mut edge_set: Vec<EdgeId> = edges_a;
                edge_set.push(EdgeId(eid));
                edge_set.extend(edges_b.iter().rev());

                match best_pair.get(&(a, b)) {
                    Some(&idx) if term_edges[idx].0 <= total => {}
                    _ => {
                        let new_idx = term_edges.len();
                        term_edges.push((total, a, b, node_set, edge_set));
                        best_pair.insert((a, b), new_idx);
                    }
                }
            });
        }

        // phase 3: Kruskal MST on the terminal distance graph
        term_edges.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut parent: Vec<u32> = (0..n_term as u32).collect();
        let mut rank: Vec<u8> = vec![0; n_term];
        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                parent[x as usize] = parent[parent[x as usize] as usize];
                x = parent[x as usize];
            }
            x
        }

        let mut tree_nodes: FxHashSet<NodeId> = FxHashSet::default();
        let mut tree_edges: FxHashSet<EdgeId> = FxHashSet::default();
        for t in &terminals {
            tree_nodes.insert(*t);
        }
        let mut accepted = 0;
        for (_w, a, b, nodes, edges) in term_edges {
            if accepted + 1 >= n_term {
                break;
            }
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra == rb {
                continue;
            }
            // union by rank
            if rank[ra as usize] < rank[rb as usize] {
                parent[ra as usize] = rb;
            } else if rank[ra as usize] > rank[rb as usize] {
                parent[rb as usize] = ra;
            } else {
                parent[rb as usize] = ra;
                rank[ra as usize] += 1;
            }
            for n in nodes {
                tree_nodes.insert(n);
            }
            for e in edges {
                tree_edges.insert(e);
            }
            accepted += 1;
        }

        let mut nodes_vec: Vec<NodeId> = tree_nodes.into_iter().collect();
        nodes_vec.sort_by_key(|n| n.0);
        let mut edges_vec: Vec<EdgeId> = tree_edges.into_iter().collect();
        edges_vec.sort_by_key(|e| e.0);

        Ok(EntitySetResult {
            nodes: nodes_vec,
            edges: edges_vec,
        })
    }
}

/// Walks pred / pred_edge from `node` to its nearest terminal: (nodes terminal -> node inclusive,
/// edges along the walk).
fn path_to_terminal(
    node: NodeId,
    nearest: &[u32],
    pred: &[u32],
    pred_edge: &[u64],
    terminals: &[NodeId],
) -> (Vec<NodeId>, Vec<EdgeId>) {
    let mut nodes_rev: Vec<NodeId> = Vec::new();
    let mut edges_rev: Vec<EdgeId> = Vec::new();
    let mut cur = node;
    loop {
        nodes_rev.push(cur);
        let ci = cur.0 as usize;
        if nearest[ci] as usize >= terminals.len() {
            break;
        }
        if cur == terminals[nearest[ci] as usize] {
            break;
        }
        let p = pred[ci];
        if p == u32::MAX {
            break;
        }
        edges_rev.push(EdgeId(pred_edge[ci]));
        cur = NodeId(p as u64);
    }
    nodes_rev.reverse();
    edges_rev.reverse();
    (nodes_rev, edges_rev)
}

#[derive(Debug, Clone, Copy)]
struct OrderedF64(f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(SteinerTree);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn connects_three_terminals_in_chain() {
        // a -> b -> c -> d -> e, terminals a, c, e: the optimal tree is the whole chain (4 edges)
        let mut g = Graph::new();
        let n: Vec<_> = (0..5)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = SteinerTree
            .run(
                &proj,
                &SteinerConfig {
                    source: n[0],
                    targets: vec![n[2], n[4]],
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // all 4 edges in the tree
        assert_eq!(r.edges.len(), 4);
        assert_eq!(r.nodes.len(), 5);
    }
}
