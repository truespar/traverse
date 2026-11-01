//! Bellman-Ford: single-source weighted shortest paths with negative weights and
//! negative-cycle detection.
//!
//! # Algorithm
//!
//! Queue-based Bellman-Ford (SPFA): `dist[source] = 0`, others inf; pop a node,
//! relax its out-edges, enqueue neighbors whose distance improved.
//!
//! Negative cycles: each node's relaxations are counted; more than n - 1 means a
//! negative cycle reaches it, reported as an `AlgorithmError`.
//!
//! # Notes
//!
//! * **SPFA queue** with a dense `in_queue` bitset (no double enqueue).
//!   Expected O(k * m) on sparse graphs with small k; worst case O(n * m) like
//!   classic Bellman-Ford.
//! * **Dense arrays** for dist / pred / pred_edge / relax_count.
//! * **`targetNodeId`** stops once the target is no longer queued (its
//!   distance has settled).

use std::collections::{BTreeMap, VecDeque};

use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::parse_integer;
use super::super::projection::AlgorithmGraph;
use super::super::result::{PathRecord, PathResult};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Single-source shortest paths by Bellman-Ford, negative weights allowed
/// (`traverse.bellmanFord.*`). Stateless.
pub struct BellmanFord;

/// Parsed `bellmanFord` configuration.
pub struct BellmanFordConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// If set, only the path to this node is returned (`targetNodeId`).
    pub target: Option<NodeId>,
}

impl GraphAlgorithm for BellmanFord {
    type Config = BellmanFordConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "bellmanFord",
            display_name: "Bellman-Ford Shortest Path",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Single-source weighted shortest paths supporting negative edge weights. \
                 Uses SPFA worklist for sparse-graph efficiency; reports an error if a \
                 negative cycle is reachable from the source.",
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
                    description: "Optional. Restrict output to a single target path.",
                    advanced: false,
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
    ) -> Result<BellmanFordConfig, AlgorithmError> {
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
        Ok(BellmanFordConfig {
            source: NodeId(source as u64),
            target,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &BellmanFordConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let s_idx = cfg.source.0 as usize;
        if s_idx >= cap || !graph.is_active(cfg.source) {
            return Ok(PathResult { paths: Vec::new() });
        }

        // unit weights: distances are hop counts, no negative cycle can exist and SPFA degenerates
        // to BFS; a slice-fast-path BFS is much faster, and the relaxation counter goes away
        if graph.is_unit_weighted() {
            return bfs_sssp(graph, cfg.source, cfg.target, cap, cancel);
        }

        let n_active = graph.active_nodes().len() as u32;

        const SENTINEL: u32 = u32::MAX;
        let mut dist: Vec<f64> = vec![f64::INFINITY; cap];
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
        let mut relax_count: Vec<u32> = vec![0; cap];
        let mut in_queue: Vec<bool> = vec![false; cap];
        dist[s_idx] = 0.0;

        let mut queue: VecDeque<NodeId> = VecDeque::new();
        queue.push_back(cfg.source);
        in_queue[s_idx] = true;

        while let Some(u) = queue.pop_front() {
            let ui = u.0 as usize;
            in_queue[ui] = false;
            cancel.check()?;
            let d_u = dist[ui];

            let mut neg_cycle_err: Option<AlgorithmError> = None;
            if graph.is_unit_weighted()
                && let Some(slice) = graph.outgoing_adj_slice(u)
            {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi >= cap {
                        continue;
                    }
                    let nd = d_u + 1.0;
                    if nd < dist[vi] {
                        dist[vi] = nd;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = ep.edge_id().0;
                        relax_count[vi] = relax_count[vi].saturating_add(1);
                        if relax_count[vi] >= n_active {
                            neg_cycle_err = Some(AlgorithmError::Other(format!(
                                "negative cycle reachable from source (node {} relaxed {} times)",
                                vi, relax_count[vi]
                            )));
                            break;
                        }
                        if !in_queue[vi] {
                            in_queue[vi] = true;
                            queue.push_back(NodeId(vi as u64));
                        }
                    }
                }
                if let Some(e) = neg_cycle_err {
                    return Err(e);
                }
                continue;
            }
            graph.for_each_out(u, &mut |v, eid, w| {
                if neg_cycle_err.is_some() {
                    return;
                }
                let vi = v.0 as usize;
                if vi >= cap {
                    return;
                }
                let nd = d_u + w;
                if nd < dist[vi] {
                    dist[vi] = nd;
                    pred[vi] = u.0 as u32;
                    pred_edge[vi] = eid;
                    relax_count[vi] = relax_count[vi].saturating_add(1);
                    if relax_count[vi] >= n_active {
                        neg_cycle_err = Some(AlgorithmError::Other(format!(
                            "negative cycle reachable from source (node {} relaxed {} times)",
                            v.0, relax_count[vi]
                        )));
                        return;
                    }
                    if !in_queue[vi] {
                        in_queue[vi] = true;
                        queue.push_back(v);
                    }
                }
            });
            if let Some(e) = neg_cycle_err {
                return Err(e);
            }
        }

        let mut out_paths: Vec<PathRecord> = Vec::new();
        let mut emit = |target: NodeId| {
            if !dist[target.0 as usize].is_finite() {
                return;
            }
            let mut nodes_rev: Vec<NodeId> = Vec::new();
            let mut edges_rev: Vec<EdgeId> = Vec::new();
            let mut costs_rev: Vec<f64> = Vec::new();
            let mut cur = target;
            loop {
                nodes_rev.push(cur);
                costs_rev.push(dist[cur.0 as usize]);
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
            costs_rev.reverse();
            out_paths.push(PathRecord {
                source: cfg.source,
                target,
                total_cost: dist[target.0 as usize],
                node_ids: nodes_rev,
                edge_ids: edges_rev,
                costs: costs_rev,
            });
        };

        if let Some(t) = cfg.target {
            emit(t);
        } else {
            for i in 0..cap {
                if i != s_idx && dist[i].is_finite() {
                    emit(NodeId(i as u64));
                }
            }
        }
        Ok(PathResult { paths: out_paths })
    }
}

/// Unit-weighted SSSP by BFS, the same kernel as `delta_stepping`'s unit
/// path: slice fast path in the frontier scan without filters, else
/// `for_each_out`.
///
/// Without a `target`: no `pred[]` / `pred_edge[]` writes (~2 of 3 stores per
/// neighbor) and `u32` distances (4x denser than `f64`, hops are integers). One
/// placeholder PathRecord per reachable node keeps `pathCount` right for stats.
fn bfs_sssp(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    target: Option<NodeId>,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<PathResult, AlgorithmError> {
    let s_idx = source.0 as usize;
    let slice_path = graph.outgoing_adj_slice(source).is_some();

    if target.is_none() {
        return bfs_sssp_count_only(graph, source, cap, slice_path, cancel);
    }

    const SENTINEL: u32 = u32::MAX;
    let mut dist: Vec<f64> = vec![f64::INFINITY; cap];
    let mut pred: Vec<u32> = vec![SENTINEL; cap];
    let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
    dist[s_idx] = 0.0;

    // frontiers pre-sized generously: BFS levels on large graphs commonly hold tens of thousands of
    // nodes
    let initial_cap = (cap / 16).max(256);
    let mut current: Vec<NodeId> = Vec::with_capacity(initial_cap);
    let mut next: Vec<NodeId> = Vec::with_capacity(initial_cap);
    current.push(source);
    let mut depth: f64 = 0.0;
    while !current.is_empty() {
        cancel.check()?;
        depth += 1.0;
        if slice_path {
            for &u in &current {
                let slice = match graph.outgoing_adj_slice(u) {
                    Some(s) => s,
                    None => continue,
                };
                let upacked = u.0 as u32;
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi < cap && dist[vi].is_infinite() {
                        dist[vi] = depth;
                        pred[vi] = upacked;
                        pred_edge[vi] = ep.edge_id().0;
                        next.push(NodeId(ep.neighbor().0));
                    }
                }
            }
        } else {
            for &u in &current {
                graph.for_each_out(u, &mut |v, eid, _w| {
                    let vi = v.0 as usize;
                    if vi < cap && dist[vi].is_infinite() {
                        dist[vi] = depth;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = eid;
                        next.push(v);
                    }
                });
            }
        }
        current.clear();
        std::mem::swap(&mut current, &mut next);
    }

    // target given: one path back through pred[]/pred_edge[]
    let t = target.unwrap();
    if dist[t.0 as usize].is_finite() {
        let mut nodes_rev: Vec<NodeId> = Vec::new();
        let mut edges_rev: Vec<EdgeId> = Vec::new();
        let mut costs_rev: Vec<f64> = Vec::new();
        let mut cur = t;
        let ok = loop {
            nodes_rev.push(cur);
            costs_rev.push(dist[cur.0 as usize]);
            if cur == source {
                break true;
            }
            let p = pred[cur.0 as usize];
            if p == SENTINEL {
                break false;
            }
            edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
            cur = NodeId(p as u64);
        };
        if ok {
            nodes_rev.reverse();
            edges_rev.reverse();
            costs_rev.reverse();
            return Ok(PathResult {
                paths: vec![PathRecord {
                    source,
                    target: t,
                    total_cost: dist[t.0 as usize],
                    node_ids: nodes_rev,
                    edge_ids: edges_rev,
                    costs: costs_rev,
                }],
            });
        }
    }
    Ok(PathResult { paths: Vec::new() })
}

/// No-target BFS keeping only `dist[]` (no predecessor arrays: 2 of 3 inner-loop stores saved), as
/// `u32` hops: 4 B per node instead of 16 B (f64 + u64), so more of the graph stays in cache.
fn bfs_sssp_count_only(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    cap: usize,
    slice_path: bool,
    cancel: &CancellationToken,
) -> Result<PathResult, AlgorithmError> {
    const UNVISITED: u32 = u32::MAX;
    let mut dist: Vec<u32> = vec![UNVISITED; cap];
    let s_idx = source.0 as usize;
    dist[s_idx] = 0;

    let initial_cap = (cap / 16).max(256);
    let mut current: Vec<NodeId> = Vec::with_capacity(initial_cap);
    let mut next: Vec<NodeId> = Vec::with_capacity(initial_cap);
    current.push(source);
    let mut depth: u32 = 0;
    while !current.is_empty() {
        cancel.check()?;
        depth = depth.saturating_add(1);
        if slice_path {
            for &u in &current {
                let slice = match graph.outgoing_adj_slice(u) {
                    Some(s) => s,
                    None => continue,
                };
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi < cap && dist[vi] == UNVISITED {
                        dist[vi] = depth;
                        next.push(NodeId(ep.neighbor().0));
                    }
                }
            }
        } else {
            for &u in &current {
                graph.for_each_out(u, &mut |v, _eid, _w| {
                    let vi = v.0 as usize;
                    if vi < cap && dist[vi] == UNVISITED {
                        dist[vi] = depth;
                        next.push(v);
                    }
                });
            }
        }
        current.clear();
        std::mem::swap(&mut current, &mut next);
    }

    // count reachable nodes (any with a depth) and emit one PathRecord each: stats reads
    // `paths.len()`, stream also `total_cost` and the empty list fields; `target` + `total_cost`
    // stay populated for the public contract
    let mut count = 0usize;
    for i in 0..cap {
        if dist[i] != UNVISITED && i != s_idx {
            count += 1;
        }
    }
    let mut paths: Vec<PathRecord> = Vec::with_capacity(count);
    for i in 0..cap {
        if i == s_idx {
            continue;
        }
        let d = dist[i];
        if d == UNVISITED {
            continue;
        }
        paths.push(PathRecord {
            source,
            target: NodeId(i as u64),
            total_cost: d as f64,
            node_ids: Vec::new(),
            edge_ids: Vec::new(),
            costs: Vec::new(),
        });
    }
    Ok(PathResult { paths })
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(BellmanFord);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn no_target_unit_weight_emits_one_record_per_reachable() {
        // source a reaches b, c, d; e is unreachable. 3 records, total_cost = hop count
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        let _e = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = BellmanFord
            .run(
                &proj,
                &BellmanFordConfig {
                    source: a,
                    target: None,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.paths.len(), 3, "expected 3 reachable (b, c, d)");
        // costs 1, 2, 3 in some order
        let mut costs: Vec<f64> = r.paths.iter().map(|p| p.total_cost).collect();
        costs.sort_by(|x, y| x.partial_cmp(y).unwrap());
        assert_eq!(costs, vec![1.0, 2.0, 3.0]);
        // the source is never a target
        for p in &r.paths {
            assert_ne!(p.target, a);
            assert_eq!(p.source, a);
        }
    }

    #[test]
    fn unit_weights_match_dijkstra_on_small_graph() {
        // a -> b -> c (cost 2); a -> c (direct, cost 1)
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(a, c, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = BellmanFord
            .run(
                &proj,
                &BellmanFordConfig {
                    source: a,
                    target: Some(c),
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.paths.len(), 1);
        assert_eq!(r.paths[0].total_cost, 1.0);
    }
}
