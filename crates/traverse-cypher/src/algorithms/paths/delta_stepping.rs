//! Delta-stepping single-source shortest paths (Meyer & Sanders, 2003).
//!
//! # Algorithm
//!
//! Nodes go into buckets by `floor(dist / delta)`, processed in order. Within a
//! bucket, "light" edges (weight <= delta) are relaxed until the bucket is stable
//! (they may re-add nodes to it), then "heavy" edges (weight > delta) once (always
//! into a later bucket). Between Dijkstra (delta -> 0, a priority queue) and
//! Bellman-Ford (delta -> inf, relax everything).
//!
//! # Notes
//!
//! - **Bucket array**: `Vec<Vec<NodeId>>` by integer bucket id, reused across
//!   phases, trimmed to the smallest non-empty bucket each phase.
//! - **delta heuristic**: default max_weight / 16. Too small approaches Dijkstra,
//!   too large Bellman-Ford.
//! - **Light/heavy split**: each adjacency list is scanned once per relaxation,
//!   light and heavy requests queued apart so heavy edges relax once per bucket.
//! - **Single-threaded**: parallel buckets would need a CAS-based `dist` array;
//!   the bucket structure is ready for it.

use std::collections::BTreeMap;

use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, parse_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::{PathRecord, PathResult};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Single-source shortest paths by delta-stepping; weights must be non-negative
/// (`traverse.deltaStepping.*`). Stateless.
pub struct DeltaStepping;

/// Parsed `deltaStepping` configuration.
pub struct DeltaSteppingConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// If set, only the path to this node is returned (`targetNodeId`).
    pub target: Option<NodeId>,
    /// Bucket width delta (`delta`, default 0 = derived from the largest weight).
    pub delta: f64,
}

impl GraphAlgorithm for DeltaStepping {
    type Config = DeltaSteppingConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "deltaStepping",
            display_name: "Delta-Stepping Shortest Paths",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Delta-stepping single-source shortest paths (Meyer-Sanders 2003). Bucketizes \
                 nodes by floor(dist/delta), splitting edge relaxations into light (<=delta) and heavy \
                 (>delta) phases. Bridges Dijkstra (small delta) and Bellman-Ford (large delta).",
            params: vec![
                ConfigParam {
                    key: "sourceNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Required. Source node.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "targetNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Optional. Restrict output to this target.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "delta",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(0.0),
                    description: "Bucket width. 0 -> auto (max_weight / 16). Smaller delta -> closer to \
                         Dijkstra; larger delta -> closer to Bellman-Ford.",
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
    ) -> Result<DeltaSteppingConfig, AlgorithmError> {
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
        let delta = opt_float(params, "delta", 0.0).map_err(AlgorithmError::from)?;
        Ok(DeltaSteppingConfig {
            source: NodeId(source as u64),
            target,
            delta,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &DeltaSteppingConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let s_idx = cfg.source.0 as usize;
        if s_idx >= cap || !graph.is_active(cfg.source) {
            return Ok(PathResult { paths: Vec::new() });
        }

        // unit weights: distances are hop counts and delta-stepping degenerates to BFS, so the buckets
        // are pure overhead; use queue BFS
        if graph.is_unit_weighted() {
            return bfs_sssp(graph, cfg.source, cfg.target, cap, cancel);
        }

        // no delta given: derive it from the max weight
        let delta = if cfg.delta > 0.0 {
            cfg.delta
        } else {
            let mut max_w = 0.0f64;
            for v in graph.active_nodes() {
                graph.for_each_out(v, &mut |_u, _eid, w| {
                    if w > max_w {
                        max_w = w;
                    }
                });
            }
            (max_w / 16.0).max(1e-3).max(1.0)
        };

        const SENTINEL: u32 = u32::MAX;
        let mut dist: Vec<f64> = vec![f64::INFINITY; cap];
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
        dist[s_idx] = 0.0;

        // buckets[k] holds nodes with tentative distance in [k*delta, (k+1)*delta); vectors reused across
        // phases
        let mut buckets: Vec<Vec<NodeId>> = vec![Vec::new()];
        let mut in_bucket: Vec<u32> = vec![u32::MAX; cap];
        buckets[0].push(cfg.source);
        in_bucket[s_idx] = 0;

        let bucket_of = |d: f64| -> usize { (d / delta) as usize };

        let mut current = 0usize;
        loop {
            // next non-empty bucket
            while current < buckets.len() && buckets[current].is_empty() {
                current += 1;
            }
            if current >= buckets.len() {
                break;
            }
            cancel.check()?;

            // nodes settled in this bucket's light phase, so the heavy phase walks just them
            // (O(bucket size)) instead of all n nodes in [lo, hi)
            let mut settled_this_phase: Vec<NodeId> = Vec::with_capacity(buckets[current].len());

            // light phase: relax light edges from the bucket; nodes may land back in it, so loop
            // until empty
            let mut light_relaxations = Vec::new();
            while !buckets[current].is_empty() {
                let frontier = std::mem::take(&mut buckets[current]);
                for &u in &frontier {
                    let ui = u.0 as usize;
                    in_bucket[ui] = u32::MAX;
                    settled_this_phase.push(u);
                    let d_u = dist[ui];
                    let mut err: Option<AlgorithmError> = None;
                    graph.for_each_out(u, &mut |v, eid, w| {
                        if err.is_some() {
                            return;
                        }
                        if w < 0.0 {
                            err = Some(AlgorithmError::Other(format!(
                                "Delta-stepping requires non-negative edge weights; saw {w}"
                            )));
                            return;
                        }
                        if w <= delta {
                            let nd = d_u + w;
                            if nd < dist[v.0 as usize] {
                                light_relaxations.push((v, nd, u.0 as u32, eid));
                            }
                        }
                    });
                    if let Some(e) = err {
                        return Err(e);
                    }
                }
                for &(v, nd, p, eid) in &light_relaxations {
                    let vi = v.0 as usize;
                    if nd >= dist[vi] {
                        continue;
                    }
                    dist[vi] = nd;
                    pred[vi] = p;
                    pred_edge[vi] = eid;
                    let new_b = bucket_of(nd);
                    while buckets.len() <= new_b {
                        buckets.push(Vec::new());
                    }
                    if in_bucket[vi] != new_b as u32 {
                        in_bucket[vi] = new_b as u32;
                        buckets[new_b].push(v);
                    }
                }
                light_relaxations.clear();
            }

            // heavy phase: relax heavy edges of nodes settled in the light phase. A node listed
            // twice (re-added by a light relaxation) is harmless: the extra pass finds a shorter
            // distance or fails the `nd < dist[vi]` check
            for u in &settled_this_phase {
                let ui = u.0 as usize;
                let d_u = dist[ui];
                // the light phase already rejected negative weights on these edges
                graph.for_each_out(*u, &mut |v, eid, w| {
                    if w <= delta {
                        return; // light edge, done
                    }
                    let nd = d_u + w;
                    let vi = v.0 as usize;
                    if nd < dist[vi] {
                        dist[vi] = nd;
                        pred[vi] = u.0 as u32;
                        pred_edge[vi] = eid;
                        let new_b = bucket_of(nd);
                        while buckets.len() <= new_b {
                            buckets.push(Vec::new());
                        }
                        if in_bucket[vi] != new_b as u32 {
                            in_bucket[vi] = new_b as u32;
                            buckets[new_b].push(v);
                        }
                    }
                });
            }
            current += 1;
        }

        // reconstruct
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

/// Unit-weighted shortest paths by plain BFS (cost +1 per step): two swapped
/// `Vec<NodeId>` frontiers, dense pred + cost arrays. What delta-stepping
/// degenerates to with equal weights, so unit projections go straight here.
///
/// Without a `target`, [`bfs_sssp_count_only`] skips `pred[]` / `pred_edge[]`
/// writes (2 of 3 stores per neighbor) and uses `u32` distances (4x denser
/// than `f64`).
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

/// No-target BFS keeping only `dist[]` (no predecessor arrays: 2 of 3 stores per neighbor saved),
/// as `u32` hops, so per-node state drops from 16 B to 4 B. One placeholder PathRecord per
/// reachable node keeps `pathCount` right for stats.
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
    super::super::procedure::register(DeltaStepping);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn matches_dijkstra_on_small_graph() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(a, c, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = DeltaStepping
            .run(
                &proj,
                &DeltaSteppingConfig {
                    source: a,
                    target: Some(c),
                    delta: 0.0,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.paths.len(), 1);
        assert_eq!(r.paths[0].total_cost, 1.0);
    }
}
