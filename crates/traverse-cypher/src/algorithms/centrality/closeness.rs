//! Closeness centrality.
//!
//! # Definition
//!
//! Unweighted:
//!
//! ```text
//!   C(v) = (R(v) - 1) / sum_{t in R(v)} d(v, t)        (per-component)
//! ```
//!
//! `R(v)` = nodes reachable from `v`, `d` = shortest hop distance. For
//! disconnected graphs, Wasserman-Faust (`useWassermanFaust`):
//!
//! ```text
//!   WF(v) = (R(v) - 1)^2 / ((n - 1) * sum d(v, t))
//! ```
//!
//! scaling each component by its share of the graph, so small isolated
//! components don't dominate.
//!
//! # Implementation
//!
//! * **Parallel per source**: independent BFS per source on rayon, one score
//!   slot written per source; no shared mutable state during BFS.
//! * **Two-frontier BFS** (`Vec<NodeId>` current + next), buffers reused, no
//!   `VecDeque` per step (Ligra's `BFS_kernel` layout).
//! * **Out-degree 0** sources get `C = 0` without a BFS (sink-heavy graphs).

use std::collections::BTreeMap;

use rayon::prelude::*;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_bool, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeScalarResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Closeness centrality: reciprocal mean shortest-path distance, unweighted BFS from each source
/// (`traverse.closeness.*`). Stateless.
pub struct Closeness;

/// Parsed `closeness` configuration.
pub struct ClosenessConfig {
    /// Wasserman-Faust normalisation for disconnected components (`useWassermanFaust`, default
    /// false).
    pub wasserman_faust: bool,
    /// BFS from this many sampled sources instead of all nodes (`samplingSize`); `None` if unset or
    /// <= 0.
    pub sampling_size: Option<i64>,
    /// Sampler seed (`samplingSeed`); `0` uses the wall clock.
    pub sampling_seed: u64,
}

impl GraphAlgorithm for Closeness {
    type Config = ClosenessConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "closeness",
            display_name: "Closeness Centrality",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Reciprocal of average shortest-path distance from each node, \
                 computed via parallel BFS from every source. Unweighted.",
            params: vec![
                ConfigParam {
                    key: "useWassermanFaust",
                    ty: ConfigParamType::Bool,
                    default: CypherValue::Bool(false),
                    description: "Apply Wasserman-Faust normalization (handles disconnected components).",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "samplingSize",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Eppstein-Wang sampling: number of random source nodes \
                         to use as estimators. If unset or <= 0, runs full all-pairs BFS.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "samplingSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(0),
                    description: "Seed for the source sampler (0 = wall-clock).",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: OutputKind::NodeScalar.default_columns().unwrap(),
            stats_columns: OutputKind::NodeScalar.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeScalar.default_write_columns().unwrap(),
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<ClosenessConfig, AlgorithmError> {
        let sampling_raw = opt_integer(params, "samplingSize", 0).map_err(AlgorithmError::from)?;
        let sampling_size = if sampling_raw > 0 {
            Some(sampling_raw)
        } else {
            None
        };
        let seed_raw = opt_integer(params, "samplingSeed", 0).map_err(AlgorithmError::from)?;
        let sampling_seed = if seed_raw == 0 {
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xC0FF_EEBA_DBED_F00D)
        } else {
            seed_raw as u64
        };
        Ok(ClosenessConfig {
            wasserman_faust: opt_bool(params, "useWassermanFaust", false)
                .map_err(AlgorithmError::from)?,
            sampling_size,
            sampling_seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &ClosenessConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }
        cancel.check()?;
        let n_minus_one = (n_active.saturating_sub(1)) as f64;

        // optional reservoir sampling of k sources: only they get scores (others NaN); O(k*n)
        // instead of O(n^2)
        let sources = if let Some(k) = cfg.sampling_size {
            let k = (k as usize).min(n_active).max(1);
            let mut sampled: Vec<NodeId> = Vec::with_capacity(k);
            let mut rng_state = cfg.sampling_seed.max(1);
            for (i, &v) in nodes.iter().enumerate() {
                if i < k {
                    sampled.push(v);
                } else {
                    rng_state ^= rng_state >> 12;
                    rng_state ^= rng_state << 25;
                    rng_state ^= rng_state >> 27;
                    let r = rng_state.wrapping_mul(0x2545F4914F6CDD1D);
                    let idx = (r as usize) % (i + 1);
                    if idx < k {
                        sampled[idx] = v;
                    }
                }
            }
            sampled
        } else {
            nodes.clone()
        };

        // source-parallel BFS on the read-only snapshot, one (idx, score) per source. `map_init`
        // gives each worker reusable scratch (visited epochs, current, next); a u32 epoch per BFS
        // (visited[i] == epoch) means no reset between sources at all
        let pairs: Vec<(usize, f64)> = sources
            .par_iter()
            .map_init(
                || {
                    (
                        vec![0u32; cap],
                        Vec::<NodeId>::with_capacity(64),
                        Vec::<NodeId>::with_capacity(64),
                        0u32,
                    )
                },
                |scratch, &source| {
                    // stopped: skip the remaining sources; the check below reports it
                    if cancel.is_stopped() {
                        return (source.0 as usize, 0.0);
                    }
                    let (visited, current, next, epoch) = scratch;
                    *epoch = epoch.wrapping_add(1);
                    if *epoch == 0 {
                        // epoch wrapped around: zero once to be safe
                        for v in visited.iter_mut() {
                            *v = 0;
                        }
                        *epoch = 1;
                    }
                    let (reachable, sum_dist) =
                        bfs_sum_distances_epoch(graph, source, cap, visited, current, next, *epoch);
                    let score = if sum_dist == 0 {
                        0.0
                    } else if cfg.wasserman_faust {
                        let r = reachable as f64;
                        (r * r) / (n_minus_one * sum_dist as f64)
                    } else {
                        reachable as f64 / sum_dist as f64
                    };
                    (source.0 as usize, score)
                },
            )
            .collect();
        cancel.check()?;

        let mut out = vec![f64::NAN; cap];
        for (idx, val) in pairs {
            out[idx] = val;
        }
        Ok(NodeScalarResult::new("score", out))
    }
}

/// BFS from `source`: (reachable count excluding self, sum of distances). The caller bumps `epoch`
/// per call, so `visited` never needs clearing between sources; that O(cap) memset per source would
/// otherwise dominate all-pairs closeness on dense graphs.
fn bfs_sum_distances_epoch(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    cap: usize,
    visited: &mut [u32],
    current: &mut Vec<NodeId>,
    next: &mut Vec<NodeId>,
    epoch: u32,
) -> (u64, u64) {
    current.clear();
    next.clear();
    let src_idx = source.0 as usize;
    if src_idx >= cap {
        return (0, 0);
    }
    visited[src_idx] = epoch;
    current.push(source);

    let mut depth: u64 = 0;
    let mut reachable: u64 = 0; // excludes the source
    let mut sum_dist: u64 = 0;

    while !current.is_empty() {
        depth += 1;
        for &u in current.iter() {
            if let Some(slice) = graph.outgoing_adj_slice(u) {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi < cap && visited[vi] != epoch {
                        visited[vi] = epoch;
                        reachable += 1;
                        sum_dist += depth;
                        next.push(NodeId(vi as u64));
                    }
                }
            } else {
                graph.for_each_out(u, &mut |v, _eid, _w| {
                    let vi = v.0 as usize;
                    if vi < cap && visited[vi] != epoch {
                        visited[vi] = epoch;
                        reachable += 1;
                        sum_dist += depth;
                        next.push(v);
                    }
                });
            }
        }
        current.clear();
        std::mem::swap(current, next);
    }
    (reachable, sum_dist)
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Closeness);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn middle_of_path_has_highest_closeness() {
        // a -> b -> c -> d -> e
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
        let r = Closeness
            .run(
                &proj,
                &ClosenessConfig {
                    wasserman_faust: false,
                    sampling_size: None,
                    sampling_seed: 1,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // BFS follows direction, so each node only reaches forward; closeness grows toward the sink
        assert!(r.values[n[3].0 as usize] > r.values[n[0].0 as usize]);
    }
}
