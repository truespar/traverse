//! Minimum spanning tree / forest via Kruskal.
//!
//! # Algorithm
//!
//! Kruskal: all unique undirected edges sorted by weight, walked in order with
//! union-find; an edge joining two components is added and the components
//! merged. The result is the minimum spanning *forest*, one tree per component.
//!
//! `objective: "maximum"` walks descending (max spanning tree); `"random"` uses a
//! random permutation (uniform random spanning tree, for sampling / Karger's
//! min cut).
//!
//! # Notes
//!
//! - **Union-find** with path compression + union by rank: ~O(alpha(n)) per op.
//! - **One sort**, m log m. Borůvka would parallelize; Kruskal is simpler and
//!   tight serially.
//! - **`sourceNode`** keeps only the tree containing that node.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{parse_integer, parse_string};
use super::super::projection::AlgorithmGraph;
use super::super::result::EntitySetResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Minimum or maximum spanning forest by Kruskal (`traverse.spanningTree.*`). Stateless.
pub struct SpanningTree;

/// Which spanning tree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Objective {
    /// Smallest total weight (default).
    Minimum,
    /// Largest total edge weight.
    Maximum,
}

/// Parsed `spanningTree` configuration.
pub struct SpanningTreeConfig {
    /// Minimum or maximum tree (`objective`, default `minimum`).
    pub objective: Objective,
    /// If set, only the tree containing this node (`sourceNodeId`); else the whole forest.
    pub source: Option<NodeId>,
}

impl GraphAlgorithm for SpanningTree {
    type Config = SpanningTreeConfig;
    type Output = EntitySetResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "spanningTree",
            display_name: "Spanning Tree (Kruskal)",
            category: Category::Paths,
            output_kind: OutputKind::EntitySet,
            stability: Stability::Beta,
            description: "Minimum (or maximum) spanning forest via Kruskal. Returns the set of edges \
                 forming the spanning tree(s); one tree per connected component, or just the \
                 tree containing `sourceNodeId` if provided.",
            params: vec![
                ConfigParam {
                    key: "objective",
                    ty: ConfigParamType::String,
                    default: CypherValue::String("minimum".into()),
                    description: "\"minimum\" (default) or \"maximum\" spanning tree.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "sourceNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Optional. If set, restrict output to the tree containing this node.",
                    advanced: false,
                    required: false,
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
    ) -> Result<SpanningTreeConfig, AlgorithmError> {
        let obj_str = match params.get("objective") {
            Some(v) => parse_string("objective", v).map_err(AlgorithmError::from)?,
            None => "minimum".to_string(),
        };
        let objective = match obj_str.as_str() {
            "minimum" | "min" => Objective::Minimum,
            "maximum" | "max" => Objective::Maximum,
            other => {
                return Err(AlgorithmError::Config(format!(
                    "objective must be 'minimum' or 'maximum', got '{other}'"
                )));
            }
        };
        let source = match params.get("sourceNodeId") {
            Some(v) if !matches!(v, CypherValue::Null) => Some(NodeId(
                parse_integer("sourceNodeId", v).map_err(AlgorithmError::from)? as u64,
            )),
            _ => None,
        };
        Ok(SpanningTreeConfig { objective, source })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &SpanningTreeConfig,
        cancel: &CancellationToken,
    ) -> Result<EntitySetResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(EntitySetResult {
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        }
        let cap = graph.highest_node_id() as usize;

        // unit weights: every spanning tree is minimal (sorting is a no-op), so BFS from each
        // unvisited start and add tree edges as found: O(n+m), no sort, no union-find. Min/max are
        // the same; `sourceNodeId` still restricts to one tree
        if graph.is_unit_weighted() {
            return bfs_spanning_tree(graph, &active, cap, cfg.source, cancel);
        }

        // undirected edges deduped by EdgeId (each physical edge once, though both endpoints see
        // it); weighted only, unit weights went to BFS above
        let mut edges: Vec<(f64, EdgeId, NodeId, NodeId)> = Vec::new();
        let mut seen_edges: FxHashMap<u64, ()> = FxHashMap::default();
        for &v in &active {
            cancel.check()?;
            graph.for_each_out(v, &mut |u, eid, w| {
                if seen_edges.insert(eid, ()).is_none() {
                    edges.push((w, EdgeId(eid), v, u));
                }
            });
        }

        match cfg.objective {
            Objective::Minimum => edges.sort_by(|a, b| a.0.total_cmp(&b.0)),
            Objective::Maximum => edges.sort_by(|a, b| b.0.total_cmp(&a.0)),
        }

        // union-find over node ids 0..cap (sparse is fine)
        let mut parent: Vec<u32> = (0..cap as u32).collect();
        let mut rank: Vec<u8> = vec![0; cap];

        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                parent[x as usize] = parent[parent[x as usize] as usize]; // path compression
                x = parent[x as usize];
            }
            x
        }

        let mut tree_edges: Vec<EdgeId> = Vec::new();
        let mut tree_node_mask: Vec<bool> = vec![false; cap];

        for (_w, eid, a, b) in edges {
            cancel.check()?;
            let ai = a.0 as u32;
            let bi = b.0 as u32;
            let ra = find(&mut parent, ai);
            let rb = find(&mut parent, bi);
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
            tree_edges.push(eid);
            tree_node_mask[ai as usize] = true;
            tree_node_mask[bi as usize] = true;
        }

        // optionally only the tree containing `source`
        let (final_nodes, final_edges) = if let Some(src) = cfg.source {
            let root = find(&mut parent, src.0 as u32);
            let mut nodes: Vec<NodeId> = Vec::new();
            for &v in &active {
                if find(&mut parent, v.0 as u32) == root {
                    nodes.push(v);
                }
            }
            // keep tree edges whose source is in the component (endpoints aren't stored, so walk
            // active_nodes adjacency)
            let tree_eid_set: FxHashMap<u64, ()> = tree_edges.iter().map(|e| (e.0, ())).collect();
            let mut kept: Vec<EdgeId> = Vec::new();
            let mut seen: FxHashMap<u64, ()> = FxHashMap::default();
            for &v in &nodes {
                graph.for_each_out(v, &mut |u, eid, _w| {
                    if tree_eid_set.contains_key(&eid)
                        && seen.insert(eid, ()).is_none()
                        && find(&mut parent, u.0 as u32) == root
                    {
                        kept.push(EdgeId(eid));
                    }
                });
            }
            (nodes, kept)
        } else {
            let nodes: Vec<NodeId> = active
                .iter()
                .copied()
                .filter(|n| tree_node_mask[n.0 as usize])
                .collect();
            (nodes, tree_edges)
        };

        Ok(EntitySetResult {
            nodes: final_nodes,
            edges: final_edges,
        })
    }
}

/// Unit-weighted spanning forest by BFS: from each unvisited start, BFS the component and add each
/// discovering edge. O(n + m). `source_only` restricts to that node's tree.
fn bfs_spanning_tree(
    graph: &dyn AlgorithmGraph,
    active: &[NodeId],
    cap: usize,
    source_only: Option<NodeId>,
    cancel: &CancellationToken,
) -> Result<EntitySetResult, AlgorithmError> {
    let mut visited: Vec<bool> = vec![false; cap];
    let mut tree_nodes: Vec<NodeId> = Vec::new();
    let mut tree_edges: Vec<EdgeId> = Vec::new();

    // start nodes: just `source_only` (its component), else every unvisited active node so all
    // components are covered
    let starts: Vec<NodeId> = match source_only {
        Some(s) => vec![s],
        None => active.to_vec(),
    };

    let mut current: Vec<NodeId> = Vec::with_capacity(256);
    let mut next: Vec<NodeId> = Vec::with_capacity(256);
    // slice fast path without projection filters
    let slice_path = active
        .first()
        .map(|s| graph.outgoing_adj_slice(*s).is_some())
        .unwrap_or(false);
    for &start in &starts {
        let si = start.0 as usize;
        if si >= cap || visited[si] {
            continue;
        }
        visited[si] = true;
        tree_nodes.push(start);
        current.clear();
        current.push(start);
        while !current.is_empty() {
            cancel.check()?;
            if slice_path {
                for &u in &current {
                    let slice = match graph.outgoing_adj_slice(u) {
                        Some(s) => s,
                        None => continue,
                    };
                    for ep in slice {
                        let vi = ep.neighbor().0 as usize;
                        if vi < cap && !visited[vi] {
                            visited[vi] = true;
                            let nv = NodeId(ep.neighbor().0);
                            tree_nodes.push(nv);
                            tree_edges.push(EdgeId(ep.edge_id().0));
                            next.push(nv);
                        }
                    }
                }
            } else {
                for &u in &current {
                    graph.for_each_out(u, &mut |v, eid, _w| {
                        let vi = v.0 as usize;
                        if vi < cap && !visited[vi] {
                            visited[vi] = true;
                            tree_nodes.push(v);
                            tree_edges.push(EdgeId(eid));
                            next.push(v);
                        }
                    });
                }
            }
            current.clear();
            std::mem::swap(&mut current, &mut next);
        }
    }
    Ok(EntitySetResult {
        nodes: tree_nodes,
        edges: tree_edges,
    })
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(SpanningTree);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn five_node_chain_mst_has_four_edges() {
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
        let r = SpanningTree
            .run(
                &proj,
                &SpanningTreeConfig {
                    objective: Objective::Minimum,
                    source: None,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // a 5-node tree has exactly 4 edges
        assert_eq!(r.edges.len(), 4);
        assert_eq!(r.nodes.len(), 5);
    }
}
