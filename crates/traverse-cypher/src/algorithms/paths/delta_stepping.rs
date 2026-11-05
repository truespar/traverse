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
use super::super::projection::{AlgorithmGraph, DenseAdjacency};
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
    /// Nothing reads the paths' node, edge and cost lists (stats mode, or a stream yielding none of
    /// them), so a run keeps only the distances.
    pub count_only: bool,
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
            count_only: crate::algorithms::procedure::path_lists_unread(params),
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
            return bfs_sssp(graph, cfg.source, cfg.target, cfg.count_only, cap, cancel);
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
            if cfg.count_only {
                out_paths.push(PathRecord {
                    source: cfg.source,
                    target,
                    total_cost: dist[target.0 as usize],
                    node_ids: Vec::new(),
                    edge_ids: Vec::new(),
                    costs: Vec::new(),
                });
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
/// Without a `target`, when nothing reads the path lists (stats mode, or a stream
/// yielding none), [`bfs_sssp_count_only`] skips `pred[]` / `pred_edge[]` writes
/// (2 of 3 stores per neighbor) and uses `u32` distances (4x denser than `f64`).
fn bfs_sssp(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    target: Option<NodeId>,
    count_only: bool,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<PathResult, AlgorithmError> {
    let s_idx = source.0 as usize;
    let slice_path = graph.outgoing_adj_slice(source).is_some();

    if target.is_none() && count_only {
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

    // one path back through pred[]/pred_edge[] per target: the given one, else every reachable
    // node but the source
    let path_to = |t: NodeId| -> Option<PathRecord> {
        if !dist[t.0 as usize].is_finite() {
            return None;
        }
        let mut nodes_rev: Vec<NodeId> = Vec::new();
        let mut edges_rev: Vec<EdgeId> = Vec::new();
        let mut costs_rev: Vec<f64> = Vec::new();
        let mut cur = t;
        loop {
            nodes_rev.push(cur);
            costs_rev.push(dist[cur.0 as usize]);
            if cur == source {
                break;
            }
            let p = pred[cur.0 as usize];
            if p == SENTINEL {
                return None;
            }
            edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
            cur = NodeId(p as u64);
        }
        nodes_rev.reverse();
        edges_rev.reverse();
        costs_rev.reverse();
        Some(PathRecord {
            source,
            target: t,
            total_cost: dist[t.0 as usize],
            node_ids: nodes_rev,
            edge_ids: edges_rev,
            costs: costs_rev,
        })
    };
    let paths = match target {
        Some(t) => path_to(t).into_iter().collect(),
        None => (0..cap)
            .filter(|&i| i != s_idx)
            .filter_map(|i| path_to(NodeId(i as u64)))
            .collect(),
    };
    Ok(PathResult { paths })
}

/// No-target BFS keeping only `dist[]` (no predecessor arrays: 2 of 3 stores per neighbor saved),
/// as `u32` hops, so per-node state drops from 16 B to 4 B. One PathRecord per reachable node, its
/// lists empty: `pathCount` for stats, target and cost for a stream.
pub(super) fn bfs_sssp_count_only(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    cap: usize,
    slice_path: bool,
    cancel: &CancellationToken,
) -> Result<PathResult, AlgorithmError> {
    let s_idx = source.0 as usize;
    let dist = match (graph.out_adjacency(), graph.in_adjacency()) {
        // a GDS snapshot's own arrays
        (Some(out), Some(inn)) => dense_hops(out, inn, s_idx, cap, cancel)?,
        _ => view_hops(graph, source, cap, slice_path, cancel)?,
    };

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

/// A node no BFS level reached.
const UNVISITED: u32 = u32::MAX;

/// A projection's dense lists, the second one if it has two (UNDIRECTED).
type Lists<'a> = (DenseAdjacency<'a>, Option<DenseAdjacency<'a>>);

/// Hops from `source` over a projection's dense CSR, direction-optimizing (Beamer, Asanović and
/// Patterson, SC'12): a level whose frontier holds a large share of the relationships left to
/// explore runs bottom-up, each unvisited node scanning its in-neighbors for one on the frontier
/// and stopping at the first, so the dense middle levels of a small-world graph touch a fraction of
/// their edges. The levels are the ones a plain BFS finds.
fn dense_hops(
    out: Lists<'_>,
    inn: Lists<'_>,
    source: usize,
    n: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u32>, AlgorithmError> {
    // GAP's thresholds: bottom-up once the frontier's edges exceed 1/ALPHA of the unexplored ones,
    // back once the frontier is under 1/BETA of the nodes
    const ALPHA: usize = 15;
    const BETA: usize = 18;
    fn rows<'a>(l: &Lists<'a>, i: usize) -> [&'a [u32]; 2] {
        [l.0.neighbors(i), l.1.map_or(&[][..], |a| a.neighbors(i))]
    }
    let out_degree = |i: usize| rows(&out, i).iter().map(|r| r.len()).sum::<usize>();
    let mut dist = vec![UNVISITED; n];
    dist[source] = 0;
    let mut unexplored = (out.0.targets.len() + out.1.map_or(0, |a| a.targets.len()))
        .saturating_sub(out_degree(source));
    let mut frontier: Vec<u32> = vec![source as u32];
    let mut next: Vec<u32> = Vec::new();
    let mut depth: u32 = 0;
    let mut bottom_up = false;
    while !frontier.is_empty() {
        cancel.check()?;
        depth += 1;
        bottom_up = if bottom_up {
            frontier.len() >= n / BETA
        } else {
            frontier
                .iter()
                .map(|&u| out_degree(u as usize))
                .sum::<usize>()
                > unexplored / ALPHA
        };
        if bottom_up {
            for v in 0..n {
                if dist[v] != UNVISITED {
                    continue;
                }
                let on_frontier = rows(&inn, v)
                    .iter()
                    .any(|r| r.iter().any(|&u| dist[u as usize] == depth - 1));
                if on_frontier {
                    dist[v] = depth;
                    next.push(v as u32);
                }
            }
        } else {
            for &u in &frontier {
                for r in rows(&out, u as usize) {
                    for &v in r {
                        if dist[v as usize] == UNVISITED {
                            dist[v as usize] = depth;
                            next.push(v);
                        }
                    }
                }
            }
        }
        for &v in &next {
            unexplored = unexplored.saturating_sub(out_degree(v as usize));
        }
        frontier.clear();
        std::mem::swap(&mut frontier, &mut next);
    }
    Ok(dist)
}

/// Hops from `source` over the projection's own reads: its packed adjacency when unfiltered, else
/// `for_each_out`.
fn view_hops(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    cap: usize,
    slice_path: bool,
    cancel: &CancellationToken,
) -> Result<Vec<u32>, AlgorithmError> {
    let mut dist: Vec<u32> = vec![UNVISITED; cap];
    dist[source.0 as usize] = 0;

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
    Ok(dist)
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

    /// Direction-optimizing levels on a GDS snapshot equal a plain BFS's on the live graph:
    /// directed and undirected, on a graph dense enough for bottom-up levels, plus a far tail and
    /// nodes nothing reaches.
    #[test]
    fn dense_hops_match_plain_bfs() {
        use crate::algorithms::config::{BaseConfig, Orientation};
        use crate::algorithms::materialized::MaterializedGraph;
        let mut g = Graph::new();
        let n: Vec<_> = (0..300)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        let mut x: u64 = 7;
        for u in 0..240 {
            for _ in 0..12 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = (x >> 33) as usize % 240;
                g.create_edge(n[u], n[v], "T", Default::default()).unwrap();
            }
        }
        // a chain hanging off the dense part (only reachable one way), the rest unreached
        for i in 239..260 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        for (orientation, undirected) in [
            (Orientation::Natural, false),
            (Orientation::Undirected, true),
        ] {
            let live = AlgorithmGraphView::new(
                view,
                &BaseConfig {
                    orientation,
                    ..Default::default()
                },
            )
            .unwrap();
            let mat = MaterializedGraph::build(view, &[], &[], None, undirected).unwrap();
            for source in [0usize, 17, 250] {
                let cap = mat.highest_node_id() as usize;
                let want = view_hops(
                    &live,
                    NodeId(source as u64),
                    cap,
                    false,
                    &CancellationToken::none(),
                )
                .unwrap();
                let got = dense_hops(
                    mat.out_adjacency().unwrap(),
                    mat.in_adjacency().unwrap(),
                    source,
                    cap,
                    &CancellationToken::none(),
                )
                .unwrap();
                assert_eq!(got, want, "{orientation:?} from {source}");
            }
        }
    }

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
                    count_only: false,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.paths.len(), 1);
        assert_eq!(r.paths[0].total_cost, 1.0);
    }
}
