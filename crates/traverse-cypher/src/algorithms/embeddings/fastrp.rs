//! FastRP: Fast Random Projection node embeddings (Chen et al., 2019).
//!
//! # Algorithm
//!
//! 1. **Random projection**: a sparse +/-1 matrix `R in R^{n x d}` at ~`1/3`
//!    density (Achlioptas); `E_0 = R`.
//! 2. **Propagation**, for `i = 1..k`:
//!
//!    ```text
//!      E_i[v] = sum_{u -> v} (deg(v))^alpha * w(u,v) * (deg(u))^{alpha-1} * E_{i-1}[u]
//!    ```
//!
//!    rows L2-normalized each iteration; `alpha` in [-1, 1] is
//!    `normalizationStrength` (default 0).
//! 3. **Combine**: `E = sum_i w_i * E_i` with `w_i = iterationWeights[i]`.
//!
//! Default weights `[0, 1, 1]` skip `E_0` and weigh the two propagated levels
//! equally, GDS's defaults.
//!
//! # Notes
//!
//! * **Parallel rows**: each iteration writes disjoint rows of `new_emb`, so
//!   rayon needs no atomics; `O(m * d)` per iteration (the GDS bound).
//! * **xorshift64\*** PRNG seeded from config: reproducible embeddings, 8 bytes
//!   of state.
//! * **f32 storage**: consumers (kNN, classification) don't need more
//!   precision, and it halves the working set.

use std::collections::BTreeMap;

use rayon::prelude::*;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeVectorResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

const DEFAULT_DIM: i64 = 128;
const DEFAULT_NORM: f64 = 0.0;
const DEFAULT_SEED: i64 = 0;

/// FastRP embeddings: sparse random projections propagated over the graph and summed per level
/// (Chen et al.) (`traverse.fastRP.*`). Stateless.
pub struct FastRP;

/// Parsed `fastRP` configuration.
pub struct FastRPConfig {
    /// Embedding length (`embeddingDimension`, default 128, > 0).
    pub dimension: usize,
    /// Weight per propagation level in the final sum, from level 0 (`iterationWeights`,
    /// comma-separated, default `0,1,1`).
    pub iteration_weights: Vec<f64>,
    /// Degree-normalisation exponent alpha in `[-1, 1]` (`normalizationStrength`, default 0).
    pub normalization_strength: f64,
    /// RNG seed (`randomSeed`); `0` uses the wall clock.
    pub seed: u64,
}

impl GraphAlgorithm for FastRP {
    type Config = FastRPConfig;
    type Output = NodeVectorResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "fastRP",
            display_name: "FastRP",
            category: Category::Embeddings,
            output_kind: OutputKind::NodeVector,
            stability: Stability::Beta,
            description: "Fast Random Projection node embeddings via sparse +/-1 initialization \
                 and iterative graph propagation (Chen et al., 2019).",
            params: vec![
                ConfigParam {
                    key: "embeddingDimension",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_DIM),
                    description: "Dimensionality of the emitted embedding vectors.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "iterationWeights",
                    ty: ConfigParamType::String,
                    default: CypherValue::String("0,1,1".into()),
                    description: "Comma-separated weights for each propagation level (E_0, E_1, ...). \
                         The final embedding is sum_i w_i * E_i.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "normalizationStrength",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_NORM),
                    description: "alpha in [-1, 1]; 0 = pure column-normalized, 0.5 = symmetric.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "randomSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_SEED),
                    description: "Seed for the xorshift PRNG (0 = wall-clock seed).",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: OutputKind::NodeVector.default_columns().unwrap(),
            stats_columns: OutputKind::NodeVector.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeVector.default_write_columns().unwrap(),
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<FastRPConfig, AlgorithmError> {
        let dimension = opt_integer(params, "embeddingDimension", DEFAULT_DIM)
            .map_err(AlgorithmError::from)? as usize;
        if dimension == 0 {
            return Err(AlgorithmError::Config(
                "embeddingDimension must be positive".into(),
            ));
        }
        let raw_weights = match params.get("iterationWeights") {
            Some(CypherValue::String(s)) => s.clone(),
            Some(CypherValue::List(l)) => l
                .iter()
                .filter_map(|cv| match cv {
                    CypherValue::Float(f) => Some(f.to_string()),
                    CypherValue::Integer(i) => Some((*i as f64).to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(","),
            None => "0,1,1".to_string(),
            other => {
                return Err(AlgorithmError::Config(format!(
                    "iterationWeights must be a list or string, got {other:?}"
                )));
            }
        };
        let iteration_weights: Vec<f64> = raw_weights
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|e| AlgorithmError::Config(format!("iterationWeights parse: {e}")))?;
        if iteration_weights.is_empty() {
            return Err(AlgorithmError::Config(
                "iterationWeights must contain at least one value".into(),
            ));
        }
        let normalization_strength = opt_float(params, "normalizationStrength", DEFAULT_NORM)
            .map_err(AlgorithmError::from)?;
        let seed_raw =
            opt_integer(params, "randomSeed", DEFAULT_SEED).map_err(AlgorithmError::from)?;
        let seed = if seed_raw == 0 {
            // `web_time` is `std::time` natively and a performance-clock shim on wasm32, where
            // `SystemTime::now()` panics
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xdeadbeef_cafebabe)
        } else {
            seed_raw as u64
        };
        Ok(FastRPConfig {
            dimension,
            iteration_weights,
            normalization_strength,
            seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &FastRPConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeVectorResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let active = graph.active_nodes();
        let n_active = active.len();
        if n_active == 0 {
            return Ok(NodeVectorResult {
                field_name: "embedding",
                dim: cfg.dimension,
                values: Vec::new(),
                valid: vec![false; cap],
            });
        }
        cancel.check()?;

        // step 1: Achlioptas random projection, ~1/3 density, entries +/-sqrt(3) so variance ~ 1
        let dim = cfg.dimension;
        let mut rng = XorShift64Star::new(cfg.seed.max(1));
        let sparsity = 3.0_f64;
        let scale = sparsity.sqrt() as f32;
        let prob_nonzero_threshold = (u64::MAX as f64 / sparsity) as u64;
        let prob_negative_threshold = u64::MAX / 2;

        let mut e_prev: Vec<f32> = vec![0.0; cap * dim];
        for &v in &active {
            let row = v.0 as usize * dim;
            for j in 0..dim {
                let r1 = rng.next_u64();
                if r1 < prob_nonzero_threshold {
                    let r2 = rng.next_u64();
                    e_prev[row + j] = if r2 < prob_negative_threshold {
                        -scale
                    } else {
                        scale
                    };
                }
            }
        }
        cancel.check()?;

        // degrees (sum of incident weights); unit-weighted graphs just use slice lengths, no
        // closure calls
        let mut weighted_deg: Vec<f64> = vec![0.0; cap];
        let unit_weight = graph.is_unit_weighted();
        for &v in &active {
            let vi = v.0 as usize;
            if unit_weight
                && let (Some(out), Some(inc)) =
                    (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v))
            {
                weighted_deg[vi] = (out.len() + inc.len()) as f64;
                continue;
            }
            let mut sum = 0.0;
            graph.for_each_out(v, &mut |_u, _eid, w| sum += w);
            // an undirected projection lists both directions as out already
            if !graph.is_undirected() {
                graph.for_each_in(v, &mut |_u, _eid, w| sum += w);
            }
            weighted_deg[vi] = sum;
        }

        // step 2: the sum starts with w_0 * E_0
        let mut final_emb: Vec<f32> = vec![0.0; cap * dim];
        let alpha = cfg.normalization_strength;
        if let Some(&w0) = cfg.iteration_weights.first()
            && w0 != 0.0
        {
            let wf = w0 as f32;
            for i in 0..cap * dim {
                final_emb[i] = wf * e_prev[i];
            }
        }

        // active-row mask for O(1) checks in the parallel write loop
        let mut active_mask = vec![false; cap];
        for &v in &active {
            active_mask[v.0 as usize] = true;
        }

        // deg^alpha and deg^(alpha-1) computed once, not powf per edge; f32 like the inner-loop
        // coefficient, halving the arrays' cache footprint in this bandwidth-bound kernel
        let mut deg_pow_alpha = vec![0.0_f32; cap];
        let mut deg_pow_alpha_m1 = vec![0.0_f32; cap];
        for &v in &active {
            let vi = v.0 as usize;
            let dv = weighted_deg[vi];
            if dv > 0.0 {
                deg_pow_alpha[vi] = dv.powf(alpha) as f32;
                deg_pow_alpha_m1[vi] = dv.powf(alpha - 1.0) as f32;
            }
        }

        // propagate straight into e_new row slices in parallel (no per-row Vec + serial copy-back)
        let mut e_new: Vec<f32> = vec![0.0; cap * dim];
        for level in 1..cfg.iteration_weights.len() {
            cancel.check()?;

            let e_prev_ref: &[f32] = &e_prev;
            let active_mask_ref: &[bool] = &active_mask;
            let deg_pow_alpha_ref: &[f32] = &deg_pow_alpha;
            let deg_pow_alpha_m1_ref: &[f32] = &deg_pow_alpha_m1;
            e_new
                .par_chunks_mut(dim)
                .enumerate()
                .for_each(|(v_idx, dst)| {
                    for x in dst.iter_mut() {
                        *x = 0.0;
                    }
                    if !active_mask_ref[v_idx] {
                        return;
                    }
                    let dv_pow_alpha = deg_pow_alpha_ref[v_idx];
                    if dv_pow_alpha == 0.0 {
                        return;
                    }
                    let v = NodeId(v_idx as u64);

                    // coef in f32 like the deg powers: no f64->f32 cast per edge, half the L2
                    // footprint
                    let mut sweep = |u: NodeId, w: f32| {
                        let u_idx = u.0 as usize;
                        let du_pa_m1 = deg_pow_alpha_m1_ref[u_idx];
                        if du_pa_m1 == 0.0 {
                            return;
                        }
                        let coef = dv_pow_alpha * w * du_pa_m1;
                        let row = u_idx * dim;
                        let src = &e_prev_ref[row..row + dim];
                        for j in 0..dim {
                            dst[j] += coef * src[j];
                        }
                    };
                    match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                        (Some(out), Some(inc)) if unit_weight => {
                            for ep in out {
                                sweep(NodeId(ep.neighbor().0), 1.0);
                            }
                            for ep in inc {
                                sweep(NodeId(ep.neighbor().0), 1.0);
                            }
                        }
                        _ => {
                            graph.for_each_out(v, &mut |u, _eid, w| sweep(u, w as f32));
                            // an undirected projection lists both directions as out already
                            if !graph.is_undirected() {
                                graph.for_each_in(v, &mut |u, _eid, w| sweep(u, w as f32));
                            }
                        }
                    }
                    // L2 normalize
                    let norm_sq: f32 = dst.iter().map(|x| x * x).sum();
                    if norm_sq > 0.0 {
                        let inv = 1.0 / norm_sq.sqrt();
                        for x in dst.iter_mut() {
                            *x *= inv;
                        }
                    }
                });

            // accumulate into final_emb, active rows only
            let wi = cfg.iteration_weights[level] as f32;
            if wi != 0.0 {
                let e_new_ref: &[f32] = &e_new;
                final_emb
                    .par_chunks_mut(dim)
                    .enumerate()
                    .for_each(|(v_idx, dst)| {
                        if !active_mask_ref[v_idx] {
                            return;
                        }
                        let src = &e_new_ref[v_idx * dim..(v_idx + 1) * dim];
                        for j in 0..dim {
                            dst[j] += wi * src[j];
                        }
                    });
            }
            std::mem::swap(&mut e_prev, &mut e_new);
        }

        let mut valid = vec![false; cap];
        for &v in &active {
            valid[v.0 as usize] = true;
        }
        Ok(NodeVectorResult {
            field_name: "embedding",
            dim,
            values: final_emb,
            valid,
        })
    }
}

/// xorshift64*, as elsewhere in the algorithms.
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
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(FastRP);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn similar_neighbors_get_similar_embeddings() {
        // a and b share all neighbors -> similar embeddings; c is isolated -> different
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let w = g.create_node(&["N"], Default::default());
        let x = g.create_node(&["N"], Default::default());
        let y = g.create_node(&["N"], Default::default());
        let edge = |g: &mut Graph, u, v| {
            g.create_edge(u, v, "T", Default::default()).unwrap();
            g.create_edge(v, u, "T", Default::default()).unwrap();
        };
        edge(&mut g, a, w);
        edge(&mut g, a, x);
        edge(&mut g, b, w);
        edge(&mut g, b, x);
        edge(&mut g, c, y);

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = FastRPConfig {
            dimension: 64,
            iteration_weights: vec![0.0, 1.0, 1.0],
            normalization_strength: 0.0,
            seed: 42,
        };
        let r = FastRP.run(&proj, &cfg, &CancellationToken::none()).unwrap();
        assert_eq!(r.dim, 64);
        // cosine similarity
        let cos = |idx_a: usize, idx_b: usize| {
            let sa = &r.values[idx_a * r.dim..(idx_a + 1) * r.dim];
            let sb = &r.values[idx_b * r.dim..(idx_b + 1) * r.dim];
            let dot: f32 = sa.iter().zip(sb).map(|(x, y)| x * y).sum();
            let na: f32 = sa.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = sb.iter().map(|x| x * x).sum::<f32>().sqrt();
            dot / (na * nb + 1e-12)
        };
        let cos_ab = cos(a.0 as usize, b.0 as usize);
        let cos_ac = cos(a.0 as usize, c.0 as usize);
        assert!(
            cos_ab > cos_ac,
            "a and b (same neighbors) should be more similar than a and c (disconnected); got {cos_ab} vs {cos_ac}"
        );
    }
}
