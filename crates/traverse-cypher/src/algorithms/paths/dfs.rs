//! Depth-first search: single-source DFS with predecessor paths.
//!
//! # Algorithm
//!
//! Iterative DFS (explicit work stack; recursion would overflow on chain-shaped
//! graphs). Each node records its discovery predecessor and edge; paths are
//! rebuilt by walking predecessors back to the source.
//!
//! # Path optimality
//!
//! Unlike BFS, DFS paths are **not** shortest: they follow the discovery tree.
//! Use BFS or Dijkstra for shortest paths, DFS for tree traversal / exploration.

use std::collections::BTreeMap;

use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_integer, parse_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::{PathRecord, PathResult};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Depth-first traversal from a source node (`traverse.dfs.*`). Stateless.
pub struct Dfs;

/// Parsed `dfs` config.
pub struct DfsConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// If set, stop as soon as this node is reached (`targetNodeId`).
    pub target: Option<NodeId>,
    /// Deepest level to descend to (`maxDepth`); `None` or negative = unbounded.
    pub max_depth: Option<i64>,
}

impl GraphAlgorithm for Dfs {
    type Config = DfsConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "dfs",
            display_name: "Depth-First Search",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Single-source iterative DFS. Returns one path per discovered node.",
            params: vec![
                ConfigParam {
                    key: "sourceNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Required. Source node for the search.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "targetNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Optional. DFS halts as soon as the target is discovered.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "maxDepth",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Optional depth cap.",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: OutputKind::Path.default_columns().unwrap(),
            stats_columns: OutputKind::Path.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<DfsConfig, AlgorithmError> {
        let source_val = params
            .get("sourceNodeId")
            .ok_or_else(|| AlgorithmError::Config("sourceNodeId is required".into()))?;
        let source = parse_integer("sourceNodeId", source_val).map_err(AlgorithmError::from)?;
        let target = match params.get("targetNodeId") {
            Some(v) if !matches!(v, CypherValue::Null) => Some(NodeId(
                parse_integer("targetNodeId", v).map_err(AlgorithmError::from)? as u64,
            )),
            _ => None,
        };
        let max_depth = if params.contains_key("maxDepth") {
            let v = opt_integer(params, "maxDepth", -1).map_err(AlgorithmError::from)?;
            if v < 0 { None } else { Some(v) }
        } else {
            None
        };
        Ok(DfsConfig {
            source: NodeId(source as u64),
            target,
            max_depth,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &DfsConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        if cfg.source.0 as usize >= cap || !graph.is_active(cfg.source) {
            return Ok(PathResult { paths: Vec::new() });
        }
        const SENTINEL: u32 = u32::MAX;
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
        let mut depth: Vec<i32> = vec![-1; cap];
        depth[cfg.source.0 as usize] = 0;
        let cap_depth = cfg.max_depth.unwrap_or(i64::MAX);

        // explicit work stack of (node, position in its out-neighbors); out-edges are buffered on
        // first push, `pos` advances as we descend
        struct Frame {
            v: NodeId,
            edges: Vec<(NodeId, EdgeId)>,
            pos: usize,
        }
        let buffer_edges = |v: NodeId| -> Vec<(NodeId, EdgeId)> {
            if let Some(slice) = graph.outgoing_adj_slice(v) {
                return slice
                    .iter()
                    .map(|ep| (NodeId(ep.neighbor().0), EdgeId(ep.edge_id().0)))
                    .collect();
            }
            let mut out: Vec<(NodeId, EdgeId)> = Vec::new();
            graph.for_each_out(v, &mut |w, eid, _weight| out.push((w, EdgeId(eid))));
            out
        };
        let mut stack: Vec<Frame> = vec![Frame {
            v: cfg.source,
            edges: buffer_edges(cfg.source),
            pos: 0,
        }];
        let mut order: Vec<NodeId> = vec![cfg.source];
        let found_target = cfg.target == Some(cfg.source);

        'outer: while let Some(top) = stack.last_mut() {
            if found_target {
                break;
            }
            if top.pos < top.edges.len() {
                let (w, eid) = top.edges[top.pos];
                top.pos += 1;
                let wi = w.0 as usize;
                if wi >= cap || depth[wi] >= 0 {
                    continue;
                }
                let new_depth = depth[top.v.0 as usize] + 1;
                if (new_depth as i64) > cap_depth {
                    continue;
                }
                depth[wi] = new_depth;
                pred[wi] = top.v.0 as u32;
                pred_edge[wi] = eid.0;
                order.push(w);
                if let Some(t) = cfg.target
                    && w == t
                {
                    break 'outer;
                }
                cancel.check()?;
                let edges = buffer_edges(w);
                stack.push(Frame {
                    v: w,
                    edges,
                    pos: 0,
                });
            } else {
                stack.pop();
            }
        }

        // reconstruct
        let mut out_paths: Vec<PathRecord> = Vec::new();
        let mut emit = |target: NodeId| {
            let mut nodes_rev: Vec<NodeId> = Vec::new();
            let mut edges_rev: Vec<EdgeId> = Vec::new();
            let mut cur = target;
            loop {
                nodes_rev.push(cur);
                if cur == cfg.source {
                    break;
                }
                let p = pred[cur.0 as usize];
                if p == SENTINEL {
                    return;
                }
                edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
                cur = NodeId(p as u64);
            }
            nodes_rev.reverse();
            edges_rev.reverse();
            let costs: Vec<f64> = (0..nodes_rev.len()).map(|i| i as f64).collect();
            out_paths.push(PathRecord {
                source: cfg.source,
                target,
                total_cost: (nodes_rev.len() - 1) as f64,
                node_ids: nodes_rev,
                edge_ids: edges_rev,
                costs,
            });
        };
        if let Some(t) = cfg.target {
            if depth[t.0 as usize] >= 0 {
                emit(t);
            }
        } else {
            for &v in &order {
                if v == cfg.source {
                    continue;
                }
                emit(v);
            }
        }
        Ok(PathResult { paths: out_paths })
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Dfs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn dfs_discovers_chain() {
        let mut g = Graph::new();
        let n: Vec<_> = (0..4)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..3 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = DfsConfig {
            source: n[0],
            target: None,
            max_depth: None,
        };
        let r = Dfs.run(&proj, &cfg, &CancellationToken::none()).unwrap();
        assert_eq!(r.paths.len(), 3);
    }
}
