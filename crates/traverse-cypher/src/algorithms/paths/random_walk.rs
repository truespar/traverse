//! Random walks: fixed-length weighted walks from each source.
//!
//! # Algorithm
//!
//! From each active node (or one explicit source), `walksPerNode` walks of
//! `walkLength` steps, each step sampled by outgoing edge weight (uniform when
//! unweighted). A dead end (no out-neighbors) ends the walk early.
//!
//! # Notes
//!
//! * **xorshift64\*** seeded from config (or `nanos()`): as fast as
//!   `rand::thread_rng`, without the `rand` crate.
//! * **Weight prefix sums** built per step via the projection's `for_each_out`;
//!   no HashMap, no allocation besides the reused temp
//!   `Vec<(NodeId, EdgeId, f64)>`.
//! * One `PathRecord` per walk with cumulative weights in `costs`; short walks
//!   just end early.

use std::collections::BTreeMap;

use rayon::prelude::*;
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

const DEFAULT_WALK_LENGTH: i64 = 80;
const DEFAULT_WALKS_PER_NODE: i64 = 10;

/// Fixed-length random walks from one node or every node, stepping by edge weight (uniform when
/// unweighted) (`traverse.randomWalk.*`). Stateless.
pub struct RandomWalk;

/// Parsed `randomWalk` configuration.
pub struct RandomWalkConfig {
    /// Start node (`sourceNodeId`); `None` walks from every projected node.
    pub source: Option<NodeId>,
    /// Walks per source (`walksPerNode`, default 10).
    pub walks_per_node: i64,
    /// Steps per walk (`walkLength`, default 80).
    pub walk_length: i64,
    /// RNG seed (`randomSeed`); `0` uses the wall clock.
    pub seed: u64,
}

impl GraphAlgorithm for RandomWalk {
    type Config = RandomWalkConfig;
    type Output = PathResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "randomWalk",
            display_name: "Random Walk",
            category: Category::Paths,
            output_kind: OutputKind::Path,
            stability: Stability::Beta,
            description: "Sample weighted random walks from each source (or all active nodes if \
                 sourceNodeId is omitted). Transitions are proportional to outgoing \
                 edge weights.",
            params: vec![
                ConfigParam {
                    key: "sourceNodeId",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Optional. If absent, every active node is a source.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "walksPerNode",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_WALKS_PER_NODE),
                    description: "Walks emitted from each source.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "walkLength",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_WALK_LENGTH),
                    description: "Steps per walk.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "randomSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(0),
                    description: "Seed for the xorshift64* PRNG (0 = wall-clock seed).",
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
    ) -> Result<RandomWalkConfig, AlgorithmError> {
        let source = match params.get("sourceNodeId") {
            Some(v) if !matches!(v, CypherValue::Null) => Some(NodeId(
                parse_integer("sourceNodeId", v).map_err(AlgorithmError::from)? as u64,
            )),
            _ => None,
        };
        let walks_per_node = opt_integer(params, "walksPerNode", DEFAULT_WALKS_PER_NODE)
            .map_err(AlgorithmError::from)?;
        let walk_length =
            opt_integer(params, "walkLength", DEFAULT_WALK_LENGTH).map_err(AlgorithmError::from)?;
        let seed_raw = opt_integer(params, "randomSeed", 0).map_err(AlgorithmError::from)?;
        let seed = if seed_raw == 0 {
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xdeadbeef_cafebabe)
        } else {
            seed_raw as u64
        };
        Ok(RandomWalkConfig {
            source,
            walks_per_node,
            walk_length,
            seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &RandomWalkConfig,
        cancel: &CancellationToken,
    ) -> Result<PathResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let sources: Vec<NodeId> = if let Some(s) = cfg.source {
            if (s.0 as usize) < cap && graph.is_active(s) {
                vec![s]
            } else {
                vec![]
            }
        } else {
            graph.active_nodes()
        };

        // parallel over sources (independent walks); a source's own walks stay serial to amortize
        // allocator/RNG state, work stealing absorbs degree skew
        let walks_per_node = cfg.walks_per_node;
        let walk_length = cfg.walk_length;
        let base_seed = cfg.seed.max(1);
        cancel.check()?;

        // no projection filters: read `outgoing_adj` slices directly, no callback overhead. The
        // common case (default projection, unweighted graph)
        let slice_fast_path = sources
            .first()
            .map(|s| graph.outgoing_adj_slice(*s).is_some())
            .unwrap_or(false);

        // unit weights: full-length walks share one costs Vec [0, 1, ..., L] instead of allocating
        // an identical buffer per walk; early-ended walks get their own truncated Vec
        let is_unit_weighted = graph.is_unit_weighted();
        let shared_costs_full: Option<std::sync::Arc<Vec<f64>>> = if is_unit_weighted {
            Some(std::sync::Arc::new(
                (0..=walk_length).map(|i| i as f64).collect(),
            ))
        } else {
            None
        };

        let paths: Vec<PathRecord> = sources
            .par_iter()
            .flat_map_iter(|&source| {
                // stopped: skip the remaining sources; the check below reports it
                if cancel.is_stopped() {
                    return Vec::new();
                }
                let mut rng = XorShift64Star::new(base_seed.wrapping_add(source.0).max(1));
                let mut out: Vec<PathRecord> = Vec::with_capacity(walks_per_node as usize);
                let mut neighbors: Vec<(NodeId, EdgeId, f64)> = Vec::with_capacity(64);
                for _ in 0..walks_per_node {
                    let mut nodes: Vec<NodeId> = Vec::with_capacity(walk_length as usize + 1);
                    let mut edges: Vec<EdgeId> = Vec::with_capacity(walk_length as usize);
                    nodes.push(source);
                    let mut cur = source;
                    let mut cum_cost = 0.0;
                    // count steps (no per-step costs.push()) and note whether a custom costs Vec is
                    // needed
                    let mut steps = 0usize;
                    for _ in 0..walk_length {
                        if slice_fast_path {
                            // closure-free slice path: uniform sampling (unit weights, the common
                            // case), no prefix sum
                            let slice = match graph.outgoing_adj_slice(cur) {
                                Some(s) if !s.is_empty() => s,
                                _ => break,
                            };
                            let idx = (rng.next_u64() as usize) % slice.len();
                            let entry = &slice[idx];
                            let next = NodeId(entry.neighbor().0);
                            nodes.push(next);
                            edges.push(EdgeId(entry.edge_id().0));
                            cum_cost += 1.0;
                            steps += 1;
                            cur = next;
                        } else {
                            // filtered / weighted fallback
                            neighbors.clear();
                            graph.for_each_out(cur, &mut |v, eid, w| {
                                neighbors.push((v, EdgeId(eid), w));
                            });
                            if neighbors.is_empty() {
                                break;
                            }
                            let pick = if neighbors.first().map(|(_, _, w)| *w)
                                == neighbors.last().map(|(_, _, w)| *w)
                            {
                                rng.next_u32() as usize % neighbors.len()
                            } else {
                                let total: f64 = neighbors.iter().map(|(_, _, w)| *w).sum();
                                if total > 0.0 {
                                    let r = rng.next_f64() * total;
                                    let mut acc = 0.0;
                                    let mut chosen = neighbors.len() - 1;
                                    for (i, (_, _, w)) in neighbors.iter().enumerate() {
                                        acc += *w;
                                        if r <= acc {
                                            chosen = i;
                                            break;
                                        }
                                    }
                                    chosen
                                } else {
                                    rng.next_u32() as usize % neighbors.len()
                                }
                            };
                            let (next, eid, w) = neighbors[pick];
                            nodes.push(next);
                            edges.push(eid);
                            cum_cost += w;
                            steps += 1;
                            cur = next;
                        }
                    }
                    let target = *nodes.last().unwrap();
                    // costs: full-length unit walks clone the shared [0..L] vec (cheaper than
                    // building it per step); early-ended unit walks and weighted walks build their
                    // own
                    let costs: Vec<f64> = if let Some(shared) = &shared_costs_full {
                        if steps == walk_length as usize {
                            (**shared).clone()
                        } else {
                            (0..=steps).map(|i| i as f64).collect()
                        }
                    } else {
                        // weighted: cumulative weights from the edges
                        let mut c: Vec<f64> = Vec::with_capacity(steps + 1);
                        c.push(0.0);
                        let mut acc = 0.0;
                        // the weighted fallback doesn't keep per-step weights, only the total, so
                        // costs are spread evenly (the unit-weighted path above is exact)
                        let step_w = if steps > 0 {
                            cum_cost / steps as f64
                        } else {
                            0.0
                        };
                        for _ in 0..steps {
                            acc += step_w;
                            c.push(acc);
                        }
                        c
                    };
                    out.push(PathRecord {
                        source,
                        target,
                        total_cost: cum_cost,
                        node_ids: nodes,
                        edge_ids: edges,
                        costs,
                    });
                }
                out
            })
            .collect();
        cancel.check()?;

        Ok(PathResult { paths })
    }
}

/// xorshift64*: fast, non-cryptographic, well distributed for graph sampling; period 2^64 - 1.
struct XorShift64Star {
    state: u64,
}

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn next_f64(&mut self) -> f64 {
        // top 53 bits -> uniform double in [0, 1)
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(RandomWalk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn walks_match_requested_count_and_length() {
        // 4-cycle: an out-neighbor at every step
        let mut g = Graph::new();
        let n: Vec<_> = (0..4)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            g.create_edge(n[i], n[(i + 1) % 4], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = RandomWalkConfig {
            source: Some(n[0]),
            walks_per_node: 3,
            walk_length: 5,
            seed: 12345,
        };
        let r = RandomWalk
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        assert_eq!(r.paths.len(), 3);
        for p in &r.paths {
            assert_eq!(p.node_ids.len(), 6); // source + 5 steps
            assert_eq!(p.edge_ids.len(), 5);
        }
    }
}
