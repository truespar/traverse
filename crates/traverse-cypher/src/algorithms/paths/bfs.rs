//! Breadth-first search: single-source unweighted shortest paths.
//!
//! # Algorithm
//!
//! Two-frontier BFS from `sourceNodeId`, layer by layer; each newly found node
//! records its predecessor and distance. Then one shortest path per reachable
//! target is rebuilt by walking predecessors back to the source.
//!
//! # Notes
//!
//! * **Two `Vec<NodeId>` frontiers** swapped each layer, no per-step `VecDeque`
//!   allocations.
//! * **Dense state** by `NodeId.0` (`predecessor`, `predecessor_edge`, `dist`),
//!   no HashMap.
//! * Optional `targetNodeId` stops once the target is found.
//! * Optional `maxDepth` caps the layers, so a huge component can't run forever.

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

/// Breadth-first traversal from a source node (`traverse.bfs.*`). Stateless.
pub struct Bfs;

/// Parsed `bfs` config.
pub struct BfsConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// If set, stop as soon as this node is reached (`targetNodeId`).
    pub target: Option<NodeId>,
    /// Deepest layer to visit (`maxDepth`); `None` or negative = unbounded.
    pub max_depth: Option<i64>,
}

impl GraphAlgorithm for Bfs {
    type Config = BfsConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "bfs",
            display_name: "Breadth-First Search",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Single-source unweighted shortest paths via layered BFS. Returns one \
                 PathRecord per reachable target.",
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
                    description: "Optional. If set, BFS stops as soon as the target is reached.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "maxDepth",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Optional layer cap.",
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
    ) -> Result<BfsConfig, AlgorithmError> {
        let source_val = params
            .get("sourceNodeId")
            .ok_or_else(|| AlgorithmError::Config("sourceNodeId is required".into()))?;
        let source = parse_integer("sourceNodeId", source_val).map_err(AlgorithmError::from)?;
        let target = if let Some(v) = params.get("targetNodeId") {
            if matches!(v, CypherValue::Null) {
                None
            } else {
                Some(NodeId(
                    parse_integer("targetNodeId", v).map_err(AlgorithmError::from)? as u64,
                ))
            }
        } else {
            None
        };
        let max_depth = if params.contains_key("maxDepth") {
            let v = opt_integer(params, "maxDepth", -1).map_err(AlgorithmError::from)?;
            if v < 0 { None } else { Some(v) }
        } else {
            None
        };
        Ok(BfsConfig {
            source: NodeId(source as u64),
            target,
            max_depth,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &BfsConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let s_idx = cfg.source.0 as usize;
        if s_idx >= cap || !graph.is_active(cfg.source) {
            return Ok(PathResult { paths: Vec::new() });
        }

        const SENTINEL: u32 = u32::MAX;
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
        let mut depth: Vec<i32> = vec![-1; cap];
        depth[s_idx] = 0;

        let mut current: Vec<NodeId> = vec![cfg.source];
        let mut next: Vec<NodeId> = Vec::new();
        let mut visited_order: Vec<NodeId> = vec![cfg.source];

        let mut found_target = cfg.target == Some(cfg.source);
        let cap_depth = cfg.max_depth.unwrap_or(i64::MAX);

        while !current.is_empty() && !found_target {
            let cur_d = depth[current[0].0 as usize] as i64;
            if cur_d >= cap_depth {
                break;
            }
            cancel.check()?;
            for &u in &current {
                let u_d = depth[u.0 as usize];
                if let Some(slice) = graph.outgoing_adj_slice(u) {
                    for ep in slice {
                        let vi = ep.neighbor().0 as usize;
                        if vi >= cap || depth[vi] >= 0 {
                            continue;
                        }
                        let v = NodeId(vi as u64);
                        depth[vi] = u_d + 1;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = ep.edge_id().0;
                        next.push(v);
                        visited_order.push(v);
                        if let Some(t) = cfg.target
                            && v == t
                        {
                            found_target = true;
                            break;
                        }
                    }
                } else {
                    graph.for_each_out(u, &mut |v, eid, _w| {
                        let vi = v.0 as usize;
                        if vi >= cap || depth[vi] >= 0 {
                            return;
                        }
                        depth[vi] = u_d + 1;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = eid;
                        next.push(v);
                        visited_order.push(v);
                        if let Some(t) = cfg.target
                            && v == t
                        {
                            found_target = true;
                        }
                    });
                }
                if found_target {
                    break;
                }
            }
            current.clear();
            std::mem::swap(&mut current, &mut next);
        }

        // rebuild paths: just the target's if one is set, else one per discovered node (discovery
        // order)
        let mut out_paths: Vec<PathRecord> = Vec::new();
        let emit = |target: NodeId, out: &mut Vec<PathRecord>| {
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
                    return; // unreachable (can't happen with depth >= 0)
                }
                edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
                cur = NodeId(p as u64);
            }
            nodes_rev.reverse();
            edges_rev.reverse();
            let costs: Vec<f64> = (0..nodes_rev.len()).map(|i| i as f64).collect();
            out.push(PathRecord {
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
                emit(t, &mut out_paths);
            }
        } else {
            for &v in &visited_order {
                if v == cfg.source {
                    continue;
                }
                emit(v, &mut out_paths);
            }
        }

        Ok(PathResult { paths: out_paths })
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Bfs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn path_distances_are_correct() {
        // a -> b -> c -> d
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = BfsConfig {
            source: a,
            target: None,
            max_depth: None,
        };
        let r = Bfs.run(&proj, &cfg, &CancellationToken::none()).unwrap();
        assert_eq!(r.paths.len(), 3, "should reach b, c, d");
        let costs: Vec<f64> = r.paths.iter().map(|p| p.total_cost).collect();
        assert!(costs.contains(&1.0));
        assert!(costs.contains(&2.0));
        assert!(costs.contains(&3.0));
    }

    #[test]
    fn target_short_circuits() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = BfsConfig {
            source: a,
            target: Some(c),
            max_depth: None,
        };
        let r = Bfs.run(&proj, &cfg, &CancellationToken::none()).unwrap();
        assert_eq!(r.paths.len(), 1);
        assert_eq!(r.paths[0].target, c);
        assert_eq!(r.paths[0].total_cost, 2.0);
    }
}
