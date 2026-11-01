//! All-pairs shortest paths (Dijkstra).
//!
//! # Algorithm
//!
//! A Dijkstra per source, in parallel (sources share no mutable state),
//! collecting the shortest path to every reachable target.
//!
//! # Practical limits
//!
//! Up to n^2 records, impractical on large graphs. `topK` keeps each source's
//! cheapest K targets (heap selection after Dijkstra); use `topK` <= 10 on big
//! graphs, or single-source Dijkstra.
//!
//! # Notes
//!
//! * **Source-parallel** (rayon), each source's top-K scattered into one flat vector.
//! * **Bounded top-K heap** instead of sorting every distance.
//! * **Negative weights rejected**, as in Dijkstra; use Bellman-Ford for those.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use rayon::prelude::*;
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

/// Shortest paths between all node pairs, by parallel Dijkstra from every source
/// (`traverse.allShortestPaths.*`). Stateless.
pub struct AllPairsShortestPath;

/// Parsed `allShortestPaths` configuration.
pub struct AllPairsConfig {
    /// Keep each source's `top_k` cheapest targets (`topK`, default 10); `0` keeps all.
    pub top_k: usize,
}

impl GraphAlgorithm for AllPairsShortestPath {
    type Config = AllPairsConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "allShortestPaths",
            display_name: "All-Pairs Shortest Paths",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Parallel Dijkstra from every node, returning the top-K cheapest reachable \
                 targets per source. Edge weights from the projection's relationshipWeightProperty.",
            params: vec![ConfigParam {
                key: "topK",
                ty: ConfigParamType::Integer,
                default: CypherValue::Integer(10),
                description: "Limit each source to its K cheapest reachable targets. Set 0 for unbounded \
                     (n^2 output - use with care).",
                advanced: false,
                required: false,
            }],
            output_columns: OutputKind::Path.default_columns().unwrap(),
            stats_columns: OutputKind::Path.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<AllPairsConfig, AlgorithmError> {
        let top_k = match params.get("topK") {
            Some(v) => parse_integer("topK", v).map_err(AlgorithmError::from)? as usize,
            None => 10,
        };
        Ok(AllPairsConfig { top_k })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &AllPairsConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let sources = graph.active_nodes().to_vec();
        cancel.check()?;

        // per-source Dijkstras in parallel; each worker reuses (dist, pred, pred_edge) via
        // `map_init`, resetting only what the previous source touched (no O(cap) memset per source)
        let results: Result<Vec<Vec<PathRecord>>, AlgorithmError> = sources
            .par_iter()
            .map_init(
                || {
                    (
                        vec![f64::INFINITY; cap],
                        vec![u32::MAX; cap],
                        vec![u64::MAX; cap],
                        Vec::<usize>::with_capacity(64),
                    )
                },
                |scratch, &src| {
                    let (dist, pred, pred_edge, touched) = scratch;
                    // reset only entries the previous source touched
                    for &i in touched.iter() {
                        dist[i] = f64::INFINITY;
                        pred[i] = u32::MAX;
                        pred_edge[i] = u64::MAX;
                    }
                    touched.clear();
                    single_source_topk(
                        graph, src, cap, cfg.top_k, cancel, dist, pred, pred_edge, touched,
                    )
                },
            )
            .collect();
        let by_source = results?;
        let flat = by_source.into_iter().flatten().collect();
        Ok(PathResult { paths: flat })
    }
}

fn single_source_topk(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    cap: usize,
    top_k: usize,
    cancel: &CancellationToken,
    dist: &mut [f64],
    pred: &mut [u32],
    pred_edge: &mut [u64],
    touched: &mut Vec<usize>,
) -> Result<Vec<PathRecord>, AlgorithmError> {
    let s_idx = source.0 as usize;
    if s_idx >= cap {
        return Ok(Vec::new());
    }
    const SENTINEL: u32 = u32::MAX;
    dist[s_idx] = 0.0;
    touched.push(s_idx);

    let mut heap: BinaryHeap<Reverse<(OrderedF64, NodeId)>> = BinaryHeap::new();
    heap.push(Reverse((OrderedF64(0.0), source)));

    while let Some(Reverse((OrderedF64(d_u), u))) = heap.pop() {
        let ui = u.0 as usize;
        if d_u > dist[ui] {
            continue;
        }
        cancel.check()?;
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
                    if dist[vi] == f64::INFINITY {
                        touched.push(vi);
                    }
                    dist[vi] = nd;
                    pred[vi] = u.0 as u32;
                    pred_edge[vi] = ep.edge_id().0;
                    heap.push(Reverse((OrderedF64(nd), NodeId(vi as u64))));
                }
            }
            continue;
        }
        let mut err: Option<AlgorithmError> = None;
        // the weighted fallback also needs touched-list pushes; nested mutable borrows through
        // closures don't fly, so collect new touches in a local Vec and merge after
        let mut local_touches: Vec<usize> = Vec::new();
        graph.for_each_out(u, &mut |v, eid, w| {
            if err.is_some() {
                return;
            }
            if w < 0.0 {
                err = Some(AlgorithmError::Other(format!(
                    "All-pairs Dijkstra requires non-negative edge weights; saw {w}"
                )));
                return;
            }
            let vi = v.0 as usize;
            if vi >= cap {
                return;
            }
            let nd = d_u + w;
            if nd < dist[vi] {
                if dist[vi] == f64::INFINITY {
                    local_touches.push(vi);
                }
                dist[vi] = nd;
                pred[vi] = u.0 as u32;
                pred_edge[vi] = eid;
                heap.push(Reverse((OrderedF64(nd), v)));
            }
        });
        for t in local_touches.drain(..) {
            touched.push(t);
        }
        if let Some(e) = err {
            return Err(e);
        }
    }

    // targets with finite distance
    let mut candidates: Vec<(f64, NodeId)> = (0..cap)
        .filter(|&i| i != s_idx && dist[i].is_finite())
        .map(|i| (dist[i], NodeId(i as u64)))
        .collect();

    // top-K by cost
    if top_k > 0 && candidates.len() > top_k {
        // partial sort: the top_k smallest
        candidates.select_nth_unstable_by(top_k, |a, b| a.0.total_cmp(&b.0));
        candidates.truncate(top_k);
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    } else {
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    let mut paths: Vec<PathRecord> = Vec::with_capacity(candidates.len());
    for (cost, target) in candidates {
        let mut nodes_rev: Vec<NodeId> = Vec::new();
        let mut edges_rev: Vec<EdgeId> = Vec::new();
        let mut costs_rev: Vec<f64> = Vec::new();
        let mut cur = target;
        loop {
            nodes_rev.push(cur);
            costs_rev.push(dist[cur.0 as usize]);
            if cur == source {
                break;
            }
            let p = pred[cur.0 as usize];
            if p == SENTINEL {
                nodes_rev.clear();
                break;
            }
            edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
            cur = NodeId(p as u64);
        }
        if nodes_rev.is_empty() {
            continue;
        }
        nodes_rev.reverse();
        edges_rev.reverse();
        costs_rev.reverse();
        paths.push(PathRecord {
            source,
            target,
            total_cost: cost,
            node_ids: nodes_rev,
            edge_ids: edges_rev,
            costs: costs_rev,
        });
    }
    Ok(paths)
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
    super::super::procedure::register(AllPairsShortestPath);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn small_graph_returns_all_pairs() {
        // a -> b -> c (chain), sources a, b, c: from a b (1) and c (2), from b c (1), from c nothing
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = AllPairsShortestPath
            .run(
                &proj,
                &AllPairsConfig { top_k: 0 },
                &CancellationToken::none(),
            )
            .unwrap();
        // 2 from a + 1 from b = 3 paths
        assert_eq!(r.paths.len(), 3);
    }
}
