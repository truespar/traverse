//! Dijkstra: single-source weighted shortest paths.
//!
//! # Algorithm
//!
//! Binary min-heap Dijkstra from `sourceNodeId`: pop the cheapest frontier node,
//! settle it, relax its out-neighbors. Negative weights are rejected (use
//! Bellman-Ford for those).
//!
//! # Notes
//!
//! * **`BinaryHeap<Reverse<(OrderedF64, NodeId)>>`** with lazy deletion: stale
//!   entries are skipped on pop, no decrease-key.
//! * **Dense `dist` / `pred` / `pred_edge` arrays**, no HashMap; settled is
//!   `dist[idx] < unsettled`, no separate visited bitset.
//! * **Optional `targetNodeId`** stops as soon as the target settles.
//! * **Weights** per the projection's `WeightConfig`: unit or property-driven.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

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

/// Single-source shortest paths by Dijkstra's algorithm (`traverse.dijkstra.*`). Stateless.
pub struct Dijkstra;

/// Parsed `dijkstra` config.
pub struct DijkstraConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// If set, stop once this node settles and return only its path (`targetNodeId`).
    pub target: Option<NodeId>,
}

impl GraphAlgorithm for Dijkstra {
    type Config = DijkstraConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "dijkstra",
            display_name: "Dijkstra Shortest Path",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Single-source weighted shortest paths via binary-heap Dijkstra. Edge \
                 weights come from the projection's relationshipWeightProperty.",
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
                    description: "Optional. Search halts as soon as the target is settled.",
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
    ) -> Result<DijkstraConfig, AlgorithmError> {
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
        Ok(DijkstraConfig {
            source: NodeId(source as u64),
            target,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &DijkstraConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let s_idx = cfg.source.0 as usize;
        if s_idx >= cap || !graph.is_active(cfg.source) {
            return Ok(PathResult { paths: Vec::new() });
        }

        const SENTINEL: u32 = u32::MAX;
        let mut dist: Vec<f64> = vec![f64::INFINITY; cap];
        let mut pred: Vec<u32> = vec![SENTINEL; cap];
        let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
        dist[s_idx] = 0.0;

        let mut heap: BinaryHeap<Reverse<(OrderedF64, NodeId)>> = BinaryHeap::new();
        heap.push(Reverse((OrderedF64(0.0), cfg.source)));

        while let Some(Reverse((OrderedF64(d_u), u))) = heap.pop() {
            let ui = u.0 as usize;
            if d_u > dist[ui] {
                // stale heap entry, already settled
                continue;
            }
            if let Some(t) = cfg.target
                && u == t
            {
                break;
            }
            cancel.check()?;
            // unit-weight slice fast path: no dyn FnMut, no per-edge negative check (unit weights
            // are 1.0); weighted graphs still go through for_each_out for the weight
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
                        "Dijkstra requires non-negative edge weights; saw {w}"
                    )));
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
                    heap.push(Reverse((OrderedF64(nd), v)));
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
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

/// `f64` wrapper with a total order (NaN largest) for the `BinaryHeap`; heap-internal only.
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
    super::super::procedure::register(Dijkstra);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn finds_shortest_among_alternatives_unit_weights() {
        // a -> b -> c    (cost 2)
        // a -> c        (cost 1, direct)
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(a, c, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = DijkstraConfig {
            source: a,
            target: Some(c),
        };
        let r = Dijkstra
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        assert_eq!(r.paths.len(), 1);
        assert_eq!(r.paths[0].total_cost, 1.0);
        assert_eq!(r.paths[0].node_ids.len(), 2);
    }
}
