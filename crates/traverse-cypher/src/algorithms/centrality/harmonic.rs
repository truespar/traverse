//! Harmonic closeness centrality.
//!
//! # Definition
//!
//! ```text
//!   H(v) = sum_{u != v}  1 / d(v, u)            (with 1/inf = 0)
//! ```
//!
//! Unlike plain closeness it's well defined on disconnected graphs: unreachable
//! nodes contribute 0. Normalized form divides by `n - 1`, landing in [0, 1].
//! Boldi & Vigna (2014) argue it's the natural extension of closeness to
//! disconnected graphs.
//!
//! # Implementation
//!
//! Same as `Closeness` (parallel BFS per source via rayon, frontier swaps,
//! dense visited bitset); only the accumulator differs: `1/d` instead of `d`.

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

/// Harmonic closeness: sum of reciprocal shortest-path distances, well defined on disconnected
/// graphs (`traverse.harmonicCloseness.*`). Stateless.
pub struct HarmonicCloseness;

/// Parsed `harmonicCloseness` config.
pub struct HarmonicConfig {
    /// Divide by `n - 1` so scores fall in `[0, 1]` (`normalize`, default true).
    pub normalize: bool,
    /// If set and > 0, BFS from this many sampled sources instead of every node (Eppstein-Wang /
    /// Brandes-Pich sampling).
    pub sampling_size: Option<i64>,
    /// Source sampler seed (`samplingSeed`); `0` = wall-clock seed.
    pub sampling_seed: u64,
}

impl GraphAlgorithm for HarmonicCloseness {
    type Config = HarmonicConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "harmonicCloseness",
            display_name: "Harmonic Closeness",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Sum of reciprocal shortest-path distances. Well-defined on disconnected \
                 graphs (unreachable nodes contribute 0). Parallel BFS per source.",
            params: vec![
                ConfigParam {
                    key: "normalize",
                    ty: ConfigParamType::Bool,
                    default: CypherValue::Bool(true),
                    description: "Divide by (n - 1) so scores land in [0, 1].",
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
    ) -> Result<HarmonicConfig, AlgorithmError> {
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
        Ok(HarmonicConfig {
            normalize: opt_bool(params, "normalize", true).map_err(AlgorithmError::from)?,
            sampling_size,
            sampling_seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &HarmonicConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }
        cancel.check()?;
        let denom = if cfg.normalize {
            (n_active.saturating_sub(1)) as f64
        } else {
            1.0
        };

        // sources: every active node, or a sample (Eppstein-Wang 2001 / Brandes-Pich 2007); when
        // sampling, only sampled nodes get scores, the rest are NaN; xorshift64* reservoir
        // sampling, so the same seed gives the same answer
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

        // per-thread scratch (visited epoch buffer + frontiers) reused across a worker's sources:
        // no per-source O(cap) allocation/reset
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
                        for v in visited.iter_mut() {
                            *v = 0;
                        }
                        *epoch = 1;
                    }
                    let sum_recip = bfs_sum_reciprocals_epoch(
                        graph, source, cap, visited, current, next, *epoch,
                    );
                    let score = if denom > 0.0 { sum_recip / denom } else { 0.0 };
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

/// BFS from `source`, returning sum 1/d(source, v) over reachable v. Epoch-counter variant for
/// scratch reuse (see closeness::bfs_sum_distances_epoch).
fn bfs_sum_reciprocals_epoch(
    graph: &dyn AlgorithmGraph,
    source: NodeId,
    cap: usize,
    visited: &mut [u32],
    current: &mut Vec<NodeId>,
    next: &mut Vec<NodeId>,
    epoch: u32,
) -> f64 {
    current.clear();
    next.clear();
    let src_idx = source.0 as usize;
    if src_idx >= cap {
        return 0.0;
    }
    visited[src_idx] = epoch;
    current.push(source);

    let mut depth: u64 = 0;
    let mut sum_recip: f64 = 0.0;

    while !current.is_empty() {
        depth += 1;
        let recip = 1.0 / depth as f64;
        for &u in current.iter() {
            if let Some(slice) = graph.outgoing_adj_slice(u) {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi < cap && visited[vi] != epoch {
                        visited[vi] = epoch;
                        sum_recip += recip;
                        next.push(NodeId(vi as u64));
                    }
                }
            } else {
                graph.for_each_out(u, &mut |v, _eid, _w| {
                    let vi = v.0 as usize;
                    if vi < cap && visited[vi] != epoch {
                        visited[vi] = epoch;
                        sum_recip += recip;
                        next.push(v);
                    }
                });
            }
        }
        current.clear();
        std::mem::swap(current, next);
    }
    sum_recip
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(HarmonicCloseness);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn harmonic_handles_disconnected_components() {
        // component 1: a->b; component 2: c->d->e
        let mut g = Graph::new();
        let n: Vec<_> = (0..5)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        g.create_edge(n[0], n[1], "T", Default::default()).unwrap();
        g.create_edge(n[2], n[3], "T", Default::default()).unwrap();
        g.create_edge(n[3], n[4], "T", Default::default()).unwrap();

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = HarmonicCloseness
            .run(
                &proj,
                &HarmonicConfig {
                    normalize: false,
                    sampling_size: None,
                    sampling_seed: 1,
                },
                &CancellationToken::none(),
            )
            .unwrap();

        // a reaches only b: H(a) = 1/1 = 1.0
        assert!((r.values[n[0].0 as usize] - 1.0).abs() < 1e-9);
        // c reaches d, e: H(c) = 1/1 + 1/2 = 1.5
        assert!((r.values[n[2].0 as usize] - 1.5).abs() < 1e-9);
    }
}
