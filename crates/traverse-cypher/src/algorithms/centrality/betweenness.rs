//! Betweenness centrality (Brandes 2001).
//!
//! # Definition
//!
//! ```text
//!   BC(v) = sum_{s != v != t} sigma_st(v) / sigma_st
//! ```
//!
//! `sigma_st` counts shortest s->t paths, `sigma_st(v)` those through `v`. Brandes:
//! the per-source dependency `delta_s(v) = sum_{w : v in pred(w)} (sigma_v/sigma_w) * (1 + delta_s(w))`
//! takes O(m) per source, O(n*m) total unweighted.
//!
//! # Implementation
//!
//! * **Source-parallel**: each source runs BFS + a backward dependency sweep on
//!   a thread-local accumulator; partial `Vec<f64>`s are summed at the end
//!   instead of atomic f64 adds (Madduri & Bader).
//! * **Per-source structures** (allocated once per worker):
//!   - `dist[v]`: hop distance from the source (`i32::MAX` if unreached)
//!   - `sigma[v]`: shortest paths from the source to `v`
//!   - `delta[v]`: dependency delta_s(v), accumulated in reverse BFS order
//!   - `stack`: visitation order, for back-propagation
//!   - `preds[v]`: predecessors on shortest paths
//! * **Unweighted**: honors the projection's orientation (Natural / Reverse /
//!   Undirected); weights are ignored.
//! * Normalization: undirected divides by `2/((n-1)(n-2))` (Newman), directed by
//!   `1/((n-1)(n-2))`.

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

/// Betweenness centrality by Brandes' algorithm, exact or sampled (`traverse.betweenness.*`).
/// Stateless.
pub struct Betweenness;

/// Parsed `betweenness` configuration.
pub struct BetweennessConfig {
    /// Normalize by 1/((n-1)(n-2)) (2/... when undirected).
    pub normalize: bool,
    /// If > 0, sample this many random sources (deterministic seed) instead of running from every
    /// node, like GDS `samplingSize`; scores are unbiased estimates (scaled by
    /// n_active/samplingSize).
    pub sampling_size: Option<i64>,
    /// Sampler seed; used only when `sampling_size > 0`.
    pub sampling_seed: u64,
}

impl GraphAlgorithm for Betweenness {
    type Config = BetweennessConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "betweenness",
            display_name: "Betweenness Centrality",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Fraction of shortest paths between all other pairs that pass through \
                 each node, via Brandes' source-parallel algorithm.",
            params: vec![
                ConfigParam {
                    key: "normalize",
                    ty: ConfigParamType::Bool,
                    default: CypherValue::Bool(false),
                    description: "Divide by (n-1)(n-2) for directed graphs (x2 for undirected).",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "samplingSize",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Null,
                    description: "Number of random source nodes to sample (Brandes-Pich). If \
                         unset or <= 0, runs full all-pairs Brandes.",
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
    ) -> Result<BetweennessConfig, AlgorithmError> {
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
        Ok(BetweennessConfig {
            normalize: opt_bool(params, "normalize", false).map_err(AlgorithmError::from)?,
            sampling_size,
            sampling_seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &BetweennessConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }
        cancel.check()?;

        // sources: all nodes, or `samplingSize` random ones picked deterministically
        // (Brandes-Pich 2007) with BC scaled by n_active / samplingSize for an unbiased estimate
        let (sources, sample_scale) = if let Some(k) = cfg.sampling_size {
            let k = (k as usize).min(n_active).max(1);
            let mut sampled: Vec<NodeId> = Vec::with_capacity(k);
            let mut rng_state = cfg.sampling_seed.max(1);
            // reservoir sampling, uniform without replacement
            for (i, &v) in nodes.iter().enumerate() {
                if i < k {
                    sampled.push(v);
                } else {
                    // xorshift64* for the index draw
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
            let scale = n_active as f64 / sampled.len() as f64;
            (sampled, scale)
        } else {
            (nodes.clone(), 1.0)
        };

        // per-source dependencies: each rayon task fills a thread-local Vec<f64>,
        // summed element-wise at the end (no per-update f64 atomics, which would need
        // CAS loops). `map_init` gives each worker long-lived scratch (sigma, dist,
        // preds, stack, current, next, delta) reused across its sources instead of
        // ~6 cap-sized Vecs per source
        let partials: Vec<f64> = sources
            .par_iter()
            .fold(
                || {
                    (
                        vec![0.0_f64; cap], // bc accumulator
                        BrandesScratch::new(cap),
                    )
                },
                |(mut acc, mut scratch), &s| {
                    // stopped: skip the remaining sources; the check below reports it
                    if !cancel.is_stopped() {
                        brandes_one_source_scratch(graph, s, cap, &mut acc, &mut scratch);
                    }
                    (acc, scratch)
                },
            )
            .map(|(acc, _)| acc)
            .reduce(
                || vec![0.0_f64; cap],
                |mut a, b| {
                    for i in 0..cap {
                        a[i] += b[i];
                    }
                    a
                },
            );

        cancel.check()?;

        // Normalization.
        let scale: f64 = if cfg.normalize {
            let n = n_active as f64;
            if n < 3.0 {
                1.0
            } else {
                let denom = (n - 1.0) * (n - 2.0);
                // the directed scale 1/denom: undirected, every pair is counted from both ends,
                // which is the undirected scale's factor 2 already
                1.0 / denom
            }
        } else if graph.is_undirected() {
            // every pair counted from both ends: once, as in GDS (and NetworkX)
            0.5
        } else {
            1.0
        };

        let mut out = vec![f64::NAN; cap];
        for &n in &nodes {
            let i = n.0 as usize;
            out[i] = partials[i] * scale * sample_scale;
        }
        Ok(NodeScalarResult::new("score", out))
    }
}

/// Scratch reused across a worker's Brandes calls. Resetting only the previous source's touched
/// entries avoids an O(cap) memset per source.
struct BrandesScratch {
    sigma: Vec<f64>,
    dist: Vec<i32>,
    preds: Vec<Vec<u32>>,
    delta: Vec<f64>,
    stack: Vec<NodeId>,
    current: Vec<NodeId>,
    next: Vec<NodeId>,
}

impl BrandesScratch {
    fn new(cap: usize) -> Self {
        Self {
            sigma: vec![0.0; cap],
            dist: vec![-1; cap],
            preds: vec![Vec::new(); cap],
            delta: vec![0.0; cap],
            stack: Vec::with_capacity(64),
            current: Vec::with_capacity(64),
            next: Vec::with_capacity(64),
        }
    }
}

/// Brandes from one source with caller-owned scratch: first resets the previous source's touched
/// indices (recorded in `stack`, drained into the reset pass).
fn brandes_one_source_scratch(
    graph: &dyn AlgorithmGraph,
    s: NodeId,
    cap: usize,
    bc: &mut [f64],
    sc: &mut BrandesScratch,
) {
    // reset only what the previous source touched: `stack` holds every visited node, so clearing
    // their sigma/dist/preds/delta restores the scratch
    for v in sc.stack.drain(..) {
        let vi = v.0 as usize;
        sc.sigma[vi] = 0.0;
        sc.dist[vi] = -1;
        sc.preds[vi].clear();
        sc.delta[vi] = 0.0;
    }
    sc.current.clear();
    sc.next.clear();

    let s_idx = s.0 as usize;
    if s_idx >= cap {
        return;
    }
    sc.sigma[s_idx] = 1.0;
    sc.dist[s_idx] = 0;
    sc.current.push(s);

    while !sc.current.is_empty() {
        for &v in sc.current.iter() {
            sc.stack.push(v);
            let v_idx = v.0 as usize;
            let v_dist = sc.dist[v_idx];
            let v_sigma = sc.sigma[v_idx];
            if let Some(slice) = graph.outgoing_adj_slice(v) {
                for ep in slice {
                    let wi = ep.neighbor().0 as usize;
                    if wi >= cap {
                        continue;
                    }
                    if sc.dist[wi] < 0 {
                        sc.dist[wi] = v_dist + 1;
                        sc.next.push(NodeId(wi as u64));
                    }
                    if sc.dist[wi] == v_dist + 1 {
                        sc.sigma[wi] += v_sigma;
                        sc.preds[wi].push(v.0 as u32);
                    }
                }
            } else {
                graph.for_each_out(v, &mut |w, _eid, _weight| {
                    let wi = w.0 as usize;
                    if wi >= cap {
                        return;
                    }
                    if sc.dist[wi] < 0 {
                        sc.dist[wi] = v_dist + 1;
                        sc.next.push(w);
                    }
                    if sc.dist[wi] == v_dist + 1 {
                        sc.sigma[wi] += v_sigma;
                        sc.preds[wi].push(v.0 as u32);
                    }
                });
            }
        }
        sc.current.clear();
        std::mem::swap(&mut sc.current, &mut sc.next);
    }

    // backward sweep over the stack in reverse, without popping (the next call's reset needs it)
    for i in (0..sc.stack.len()).rev() {
        let w = sc.stack[i];
        let wi = w.0 as usize;
        let coef = (1.0 + sc.delta[wi]) / sc.sigma[wi];
        for &v_raw in &sc.preds[wi] {
            let vi = v_raw as usize;
            sc.delta[vi] += sc.sigma[vi] * coef;
        }
        if w != s {
            bc[wi] += sc.delta[wi];
        }
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Betweenness);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn bridge_node_has_highest_betweenness() {
        // two triangles joined through bridge node x:
        //   a - b - c   x   d - e - f
        //   \_____/    /|\    \_____/
        //              | |
        //         (a, b, c -> x -> d, e, f via x)
        // x is on every cross-component shortest path
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let x = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        let e = g.create_node(&["N"], Default::default());
        let f = g.create_node(&["N"], Default::default());
        // left triangle (both directions)
        for &(u, v) in &[(a, b), (b, a), (b, c), (c, b), (a, c), (c, a)] {
            g.create_edge(u, v, "T", Default::default()).unwrap();
        }
        // bridge
        for &(u, v) in &[(c, x), (x, c), (x, d), (d, x)] {
            g.create_edge(u, v, "T", Default::default()).unwrap();
        }
        // right triangle
        for &(u, v) in &[(d, e), (e, d), (e, f), (f, e), (d, f), (f, d)] {
            g.create_edge(u, v, "T", Default::default()).unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Betweenness
            .run(
                &proj,
                &BetweennessConfig {
                    normalize: false,
                    sampling_size: None,
                    sampling_seed: 1,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        let bc_x = r.values[x.0 as usize];
        for &other in &[a, b, c, d, e, f] {
            assert!(
                bc_x > r.values[other.0 as usize],
                "bridge x={} should outrank {:?}={}",
                bc_x,
                other,
                r.values[other.0 as usize]
            );
        }
    }
}
