//! Yen's k shortest loopless paths.
//!
//! # Algorithm
//!
//! For a pair (s, t), path 1 is the plain Dijkstra shortest path. Each next path:
//!
//! 1. For each prefix node `v` of the previous path (the spur node), forbid the
//!    edges other accepted paths with the same prefix take out of `v`.
//! 2. Dijkstra from `v` to `t` on what's left; prepend the root prefix.
//! 3. Push the candidate on a min-heap by total cost; the best unique candidate
//!    becomes the next path.
//!
//! # Notes
//!
//! * **Edge-mask Dijkstra**: a `Vec<bool>` of forbidden edge ids instead of a
//!   graph copy, so the spur search allocates nothing topology-side.
//! * **Loopless**: each spur search also forbids the root prefix's nodes (bar
//!   the spur node), so concatenated paths have no loops.
//! * **Min-heap candidates**: `BinaryHeap<Reverse>` keyed by
//!   (total_cost, candidate_id), O(log m) per pop.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashSet};

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

/// The `k` shortest loopless paths between two nodes, by Yen's algorithm (`traverse.yens.*`).
/// Stateless.
pub struct Yens;

/// Parsed `yens` config.
pub struct YensConfig {
    /// Start node (`sourceNodeId`, required).
    pub source: NodeId,
    /// End node of every path (`targetNodeId`, required).
    pub target: NodeId,
    /// Number of paths (`k`, default 3, at least 1).
    pub k: usize,
}

impl GraphAlgorithm for Yens {
    type Config = YensConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "yens",
            display_name: "Yen's K-Shortest Paths",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Top-k loopless shortest paths between a single source and target via Yen's \
                 algorithm. Path 1 is the Dijkstra shortest path; each next path is found \
                 by running spur Dijkstras with previously-used edges forbidden.",
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
                    description: "Required. Target node.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "k",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(3),
                    description: "Number of paths to return (default 3).",
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
    ) -> Result<YensConfig, AlgorithmError> {
        let source_val = params
            .get("sourceNodeId")
            .ok_or_else(|| AlgorithmError::Config("sourceNodeId is required".into()))?;
        let target_val = params
            .get("targetNodeId")
            .ok_or_else(|| AlgorithmError::Config("targetNodeId is required".into()))?;
        let source = parse_integer("sourceNodeId", source_val).map_err(AlgorithmError::from)?;
        let target = parse_integer("targetNodeId", target_val).map_err(AlgorithmError::from)?;
        let k = match params.get("k") {
            Some(v) => parse_integer("k", v).map_err(AlgorithmError::from)? as usize,
            None => 3,
        };
        if k == 0 {
            return Err(AlgorithmError::Config("k must be >= 1".into()));
        }
        Ok(YensConfig {
            source: NodeId(source as u64),
            target: NodeId(target as u64),
            k,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &YensConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        if cfg.source.0 as usize >= cap || cfg.target.0 as usize >= cap {
            return Ok(PathResult { paths: Vec::new() });
        }
        if cfg.source == cfg.target {
            return Ok(PathResult {
                paths: vec![PathRecord {
                    source: cfg.source,
                    target: cfg.target,
                    total_cost: 0.0,
                    node_ids: vec![cfg.source],
                    edge_ids: Vec::new(),
                    costs: vec![0.0],
                }],
            });
        }

        // path 1: plain Dijkstra
        let mut accepted: Vec<PathRecord> = Vec::with_capacity(cfg.k);
        let forbidden_nodes: Vec<bool> = vec![false; cap];
        let forbidden_edges: HashSet<EdgeId> = HashSet::new();
        let first = match constrained_dijkstra(
            graph,
            cfg.source,
            cfg.target,
            cap,
            &forbidden_nodes,
            &forbidden_edges,
            cancel,
        )? {
            Some(p) => p,
            None => return Ok(PathResult { paths: Vec::new() }),
        };
        accepted.push(first);

        // candidate heap keyed by total cost, ties broken by insertion order (a monotonic counter
        // in the key)
        let mut candidates: BinaryHeap<Reverse<CandidateKey>> = BinaryHeap::new();
        let mut candidates_pool: Vec<PathRecord> = Vec::new();
        let mut seen: HashSet<Vec<NodeId>> = HashSet::new();
        seen.insert(accepted[0].node_ids.clone());

        while accepted.len() < cfg.k {
            cancel.check()?;
            let last = &accepted[accepted.len() - 1];

            // spur from each prefix node of `last`
            for i in 0..last.node_ids.len().saturating_sub(1) {
                let spur_node = last.node_ids[i];
                let root_path_nodes = &last.node_ids[..=i];
                let root_path_edges = &last.edge_ids[..i];
                let root_cost = last.costs[i];

                // forbid edges of accepted paths sharing this prefix
                let mut fe: HashSet<EdgeId> = HashSet::new();
                for p in &accepted {
                    if p.node_ids.len() > i
                        && p.node_ids[..=i] == *root_path_nodes
                        && p.edge_ids.len() > i
                    {
                        fe.insert(p.edge_ids[i]);
                    }
                }

                // forbid revisiting prefix nodes (except the spur)
                let mut fn_mask: Vec<bool> = vec![false; cap];
                for &n in &root_path_nodes[..root_path_nodes.len() - 1] {
                    if (n.0 as usize) < cap {
                        fn_mask[n.0 as usize] = true;
                    }
                }

                let spur = match constrained_dijkstra(
                    graph, spur_node, cfg.target, cap, &fn_mask, &fe, cancel,
                )? {
                    Some(p) => p,
                    None => continue,
                };

                // root prefix + spur
                let mut combined_nodes: Vec<NodeId> =
                    root_path_nodes[..root_path_nodes.len() - 1].to_vec();
                combined_nodes.extend(&spur.node_ids);
                let mut combined_edges: Vec<EdgeId> = root_path_edges.to_vec();
                combined_edges.extend(&spur.edge_ids);
                let mut combined_costs: Vec<f64> = last.costs[..i].to_vec();
                let spur_offset = root_cost;
                for c in &spur.costs {
                    combined_costs.push(c + spur_offset);
                }
                let total = combined_costs[combined_costs.len() - 1];

                if seen.contains(&combined_nodes) {
                    continue;
                }
                seen.insert(combined_nodes.clone());

                let cand = PathRecord {
                    source: cfg.source,
                    target: cfg.target,
                    total_cost: total,
                    node_ids: combined_nodes,
                    edge_ids: combined_edges,
                    costs: combined_costs,
                };
                let key = CandidateKey {
                    cost: OrderedF64(cand.total_cost),
                    idx: candidates_pool.len() as u64,
                };
                candidates_pool.push(cand);
                candidates.push(Reverse(key));
            }

            // next best candidate
            match candidates.pop() {
                Some(Reverse(k)) => {
                    let path = std::mem::replace(
                        &mut candidates_pool[k.idx as usize],
                        PathRecord {
                            source: cfg.source,
                            target: cfg.target,
                            total_cost: 0.0,
                            node_ids: Vec::new(),
                            edge_ids: Vec::new(),
                            costs: Vec::new(),
                        },
                    );
                    accepted.push(path);
                }
                None => break,
            }
        }

        Ok(PathResult { paths: accepted })
    }
}

fn constrained_dijkstra(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    target: NodeId,
    cap: usize,
    forbidden_nodes: &[bool],
    forbidden_edges: &HashSet<EdgeId>,
    cancel: &CancellationToken,
) -> Result<Option<PathRecord>, AlgorithmError> {
    const SENTINEL: u32 = u32::MAX;
    let mut dist: Vec<f64> = vec![f64::INFINITY; cap];
    let mut pred: Vec<u32> = vec![SENTINEL; cap];
    let mut pred_edge: Vec<u64> = vec![u64::MAX; cap];
    let s_idx = source.0 as usize;
    if s_idx >= cap {
        return Ok(None);
    }
    dist[s_idx] = 0.0;

    let mut heap: BinaryHeap<Reverse<(OrderedF64, NodeId)>> = BinaryHeap::new();
    heap.push(Reverse((OrderedF64(0.0), source)));

    while let Some(Reverse((OrderedF64(d_u), u))) = heap.pop() {
        let ui = u.0 as usize;
        if d_u > dist[ui] {
            continue;
        }
        if u == target {
            break;
        }
        cancel.check()?;
        if graph.is_unit_weighted()
            && let Some(slice) = graph.outgoing_adj_slice(u)
        {
            for ep in slice {
                let vi = ep.neighbor().0 as usize;
                let eid = ep.edge_id().0;
                if vi >= cap || forbidden_nodes[vi] {
                    continue;
                }
                if forbidden_edges.contains(&EdgeId(eid)) {
                    continue;
                }
                let nd = d_u + 1.0;
                if nd < dist[vi] {
                    dist[vi] = nd;
                    pred[vi] = u.0 as u32;
                    pred_edge[vi] = eid;
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
                    "Yen's requires non-negative edge weights; saw {w}"
                )));
                return;
            }
            let vi = v.0 as usize;
            if vi >= cap || forbidden_nodes[vi] {
                return;
            }
            if forbidden_edges.contains(&EdgeId(eid)) {
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

    if !dist[target.0 as usize].is_finite() {
        return Ok(None);
    }
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
            return Ok(None);
        }
        edges_rev.push(EdgeId(pred_edge[cur.0 as usize]));
        cur = NodeId(p as u64);
    }
    nodes_rev.reverse();
    edges_rev.reverse();
    costs_rev.reverse();
    Ok(Some(PathRecord {
        source,
        target,
        total_cost: dist[target.0 as usize],
        node_ids: nodes_rev,
        edge_ids: edges_rev,
        costs: costs_rev,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CandidateKey {
    cost: OrderedF64,
    idx: u64,
}

impl PartialOrd for CandidateKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CandidateKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.cost.cmp(&other.cost).then(self.idx.cmp(&other.idx))
    }
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
    super::super::procedure::register(Yens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn two_paths_in_diamond_graph() {
        // a -> b -> d, a -> c -> d; k=2 returns both, cost 2
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(a, c, "T", Default::default()).unwrap();
        g.create_edge(b, d, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Yens
            .run(
                &proj,
                &YensConfig {
                    source: a,
                    target: d,
                    k: 2,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.paths.len(), 2);
        assert!(r.paths.iter().all(|p| p.total_cost == 2.0));
    }
}
