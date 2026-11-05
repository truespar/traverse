//! Node2Vec: biased random walks + skip-gram (Grover & Leskovec, 2016).
//!
//! # Algorithm
//!
//! 1. **Biased random walks.** `walksPerNode` second-order walks of `walkLength`
//!    steps from each active node. At current node `v` with previous node
//!    `t_prev`, neighbor `x` of `v` gets weight:
//!
//!    ```text
//!      pi(v, x) =  1/p   if x == t_prev      (return)
//!                 1     if x in N(t_prev)    (stay close)
//!                 1/q   else                 (depart)
//!    ```
//!
//!    `p` is `returnFactor`, `q` is `inOutFactor`.
//! 2. **Skip-gram with negative sampling (SGNS).** Each walk is a "sentence":
//!    for each center `c` and context `w` within `windowSize`, push up
//!    `sigma(v_c * v_w)` and push down `sigma(v_c * v_{w^-})` for `negativeSamplingRate`
//!    random negatives `w^-`.
//!
//! # Notes
//!
//! * Walks are generated in parallel (rayon); sources are independent.
//! * Hogwild! parallel SGD over the walks: racing updates on sparse skip-gram
//!   are rare, bounded noise (Niu et al. 2011), so results vary slightly
//!   between runs.
//! * Sigmoid via the standard exp-table approximation (1000 entries over
//!   [-6, 6]); skip-gram is bound by sigmoid evaluations.
//! * **xorshift64\*** PRNG seeded from config, so walks are reproducible.

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
const DEFAULT_WALKS_PER_NODE: i64 = 10;
const DEFAULT_WALK_LENGTH: i64 = 80;
const DEFAULT_WINDOW_SIZE: i64 = 10;
const DEFAULT_NEGATIVE: i64 = 5;
const DEFAULT_ITERATIONS: i64 = 1;
const DEFAULT_RETURN_FACTOR: f64 = 1.0;
const DEFAULT_IN_OUT_FACTOR: f64 = 1.0;
const DEFAULT_LR: f64 = 0.025;
const DEFAULT_MIN_LR: f64 = 1e-4;
const DEFAULT_SEED: i64 = 0;

/// node2vec embeddings: biased random walks trained with skip-gram + negative sampling (Grover &
/// Leskovec) (`traverse.node2vec.*`). Stateless.
pub struct Node2Vec;

/// Parsed `node2vec` config; every count must be positive.
pub struct Node2VecConfig {
    /// Embedding length (`embeddingDimension`, default 128).
    pub dimension: usize,
    /// Walks generated from each node (`walksPerNode`, default 10).
    pub walks_per_node: usize,
    /// Steps per walk (`walkLength`, default 80).
    pub walk_length: usize,
    /// Skip-gram context radius per step (`windowSize`, default 10).
    pub window_size: usize,
    /// Negative samples per positive pair (`negativeSamplingRate`, default 5).
    pub negative_samples: usize,
    /// Training epochs over the walks (`iterations`, default 1).
    pub iterations: usize,
    /// `p` bias: higher means a walk steps straight back less often (`returnFactor`, default 1.0, >
    /// 0).
    pub return_factor: f64,
    /// `q` bias: below 1 favours moving outward, above 1 staying near the previous node
    /// (`inOutFactor`, default 1.0, > 0).
    pub in_out_factor: f64,
    /// Starting learning rate (`initialLearningRate`, default 0.025).
    pub initial_lr: f64,
    /// Floor the learning rate decays to linearly (`minLearningRate`, default 1e-4).
    pub min_lr: f64,
    /// RNG seed (`randomSeed`); `0` uses the wall clock.
    pub seed: u64,
}

impl GraphAlgorithm for Node2Vec {
    type Config = Node2VecConfig;
    type Output = NodeVectorResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "node2vec",
            display_name: "Node2Vec",
            category: Category::Embeddings,
            output_kind: OutputKind::NodeVector,
            stability: Stability::Beta,
            description: "Biased random walks (Grover-Leskovec) + skip-gram with negative \
                 sampling. Learns dense embeddings that capture neighborhood structure.",
            params: vec![
                ConfigParam {
                    key: "embeddingDimension",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_DIM),
                    description: "Dimensionality of the embedding.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "walksPerNode",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_WALKS_PER_NODE),
                    description: "Walks generated per source node.",
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
                    key: "windowSize",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_WINDOW_SIZE),
                    description: "Skip-gram context window radius.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "negativeSamplingRate",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_NEGATIVE),
                    description: "Negative samples per positive (center, context) pair.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "iterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_ITERATIONS),
                    description: "SGD epochs over the walk corpus.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "returnFactor",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_RETURN_FACTOR),
                    description: "p - bias toward returning to the previous step.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "inOutFactor",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_IN_OUT_FACTOR),
                    description: "q - bias toward departing from the previous neighborhood.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "initialLearningRate",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_LR),
                    description: "Starting SGD learning rate.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "minLearningRate",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_MIN_LR),
                    description: "Floor learning rate after linear decay.",
                    advanced: true,
                    required: false,
                },
                ConfigParam {
                    key: "randomSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_SEED),
                    description: "Seed (0 = wall-clock).",
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
    ) -> Result<Node2VecConfig, AlgorithmError> {
        let dim = opt_integer(params, "embeddingDimension", DEFAULT_DIM)
            .map_err(AlgorithmError::from)? as usize;
        let wpn = opt_integer(params, "walksPerNode", DEFAULT_WALKS_PER_NODE)
            .map_err(AlgorithmError::from)? as usize;
        let wl = opt_integer(params, "walkLength", DEFAULT_WALK_LENGTH)
            .map_err(AlgorithmError::from)? as usize;
        let ws = opt_integer(params, "windowSize", DEFAULT_WINDOW_SIZE)
            .map_err(AlgorithmError::from)? as usize;
        let neg = opt_integer(params, "negativeSamplingRate", DEFAULT_NEGATIVE)
            .map_err(AlgorithmError::from)? as usize;
        let it = opt_integer(params, "iterations", DEFAULT_ITERATIONS)
            .map_err(AlgorithmError::from)? as usize;
        let p = opt_float(params, "returnFactor", DEFAULT_RETURN_FACTOR)
            .map_err(AlgorithmError::from)?;
        let q = opt_float(params, "inOutFactor", DEFAULT_IN_OUT_FACTOR)
            .map_err(AlgorithmError::from)?;
        let lr =
            opt_float(params, "initialLearningRate", DEFAULT_LR).map_err(AlgorithmError::from)?;
        let min_lr =
            opt_float(params, "minLearningRate", DEFAULT_MIN_LR).map_err(AlgorithmError::from)?;
        if dim == 0 || wpn == 0 || wl == 0 || ws == 0 || it == 0 {
            return Err(AlgorithmError::Config(
                "embeddingDimension, walksPerNode, walkLength, windowSize, iterations must be positive".into(),
            ));
        }
        if p <= 0.0 || q <= 0.0 {
            return Err(AlgorithmError::Config(
                "returnFactor and inOutFactor must be positive".into(),
            ));
        }
        let seed_raw =
            opt_integer(params, "randomSeed", DEFAULT_SEED).map_err(AlgorithmError::from)?;
        let seed = if seed_raw == 0 {
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x5a5a_a5a5_5a5a_a5a5)
        } else {
            seed_raw as u64
        };
        Ok(Node2VecConfig {
            dimension: dim,
            walks_per_node: wpn,
            walk_length: wl,
            window_size: ws,
            negative_samples: neg,
            iterations: it,
            return_factor: p,
            in_out_factor: q,
            initial_lr: lr,
            min_lr,
            seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &Node2VecConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeVectorResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let active = graph.active_nodes();
        let dim = cfg.dimension;
        if active.is_empty() {
            return Ok(NodeVectorResult {
                field_name: "embedding",
                dim,
                values: Vec::new(),
                valid: vec![false; cap],
            });
        }
        cancel.check()?;

        // step 1: neighbor sets (sorted, deduped), in parallel over active nodes; push raw ids then
        // sort+dedup (no hashing), like triangleCount
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); cap];
        let rows: Vec<(usize, Vec<u32>)> = active
            .par_iter()
            .map(|&v| {
                let mut list: Vec<u32> = Vec::with_capacity(32);
                match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                    (Some(out), Some(inc)) => {
                        for ep in out {
                            let u = ep.neighbor().0;
                            if u != v.0 {
                                list.push(u as u32);
                            }
                        }
                        for ep in inc {
                            let u = ep.neighbor().0;
                            if u != v.0 {
                                list.push(u as u32);
                            }
                        }
                    }
                    _ => {
                        graph.for_each_out(v, &mut |u, _eid, _w| {
                            if u != v {
                                list.push(u.0 as u32);
                            }
                        });
                        // an undirected projection lists both directions as out already
                        if !graph.is_undirected() {
                            graph.for_each_in(v, &mut |u, _eid, _w| {
                                if u != v {
                                    list.push(u.0 as u32);
                                }
                            });
                        }
                    }
                }
                list.sort_unstable();
                list.dedup();
                (v.0 as usize, list)
            })
            .collect();
        for (idx, list) in rows {
            adj[idx] = list;
        }
        cancel.check()?;

        // step 2: walks in parallel, one per (source, k)
        let p_inv = 1.0 / cfg.return_factor;
        let q_inv = 1.0 / cfg.in_out_factor;
        let walks: Vec<Vec<u32>> = active
            .par_iter()
            .flat_map_iter(|&source| {
                let mut rng = XorShift64Star::new(
                    cfg.seed
                        .wrapping_add(source.0)
                        .wrapping_mul(0x9E3779B97F4A7C15)
                        .max(1),
                );
                (0..cfg.walks_per_node)
                    .map(|_| sample_walk(source, cfg.walk_length, &adj, p_inv, q_inv, &mut rng))
                    .collect::<Vec<_>>()
            })
            .collect();
        cancel.check()?;

        // step 3: SGD over the walks with two matrices: input (the result) and output (context side
        // of negative sampling, discarded after)
        let mut in_emb: Vec<f32> = vec![0.0; cap * dim];
        let mut out_emb: Vec<f32> = vec![0.0; cap * dim];
        // input embeddings in [-0.5/d, 0.5/d] for stable SGD
        let mut init_rng = XorShift64Star::new(cfg.seed.max(1));
        let init_scale = 1.0 / dim as f32;
        for &v in &active {
            let row = v.0 as usize * dim;
            for j in 0..dim {
                let u = init_rng.next_u64();
                let r = ((u >> 11) as f32 / (1u64 << 53) as f32) - 0.5;
                in_emb[row + j] = r * init_scale;
            }
        }

        let exp_table = build_exp_table();
        let active_count = active.len();
        let total_pairs = estimate_pair_count(&walks, cfg.window_size) * cfg.iterations as u64;

        // Hogwild! parallel SGD: each rayon task trains its slice of walks against the shared
        // in_emb / out_emb. Races are rare on sparse skip-gram and the algorithm absorbs them (Niu
        // et al. 2011); SGD dominates the cost, so this is the parallel win
        let in_emb_ptr = EmbPtr(in_emb.as_mut_ptr() as usize);
        let out_emb_ptr = EmbPtr(out_emb.as_mut_ptr() as usize);
        let active_slice: &[NodeId] = &active;
        let exp_table_ref: &[f32] = &exp_table;
        let walks_slice: &[Vec<u32>] = &walks;
        let iterations = cfg.iterations;
        let window_size = cfg.window_size;
        let negative_samples = cfg.negative_samples;
        let initial_lr = cfg.initial_lr;
        let min_lr = cfg.min_lr;
        let seed_root = cfg.seed.wrapping_add(0x1234_5678).max(1);
        let pairs_per_walk_avg = if walks_slice.is_empty() {
            0
        } else {
            (total_pairs / iterations as u64 / walks_slice.len() as u64).max(1)
        };

        for iter_i in 0..iterations {
            cancel.check()?;
            walks_slice.par_iter().enumerate().for_each(|(wi, walk)| {
                // stopped: skip the remaining walks; the check after the epoch reports it
                if cancel.is_stopped() {
                    return;
                }
                let mut rng = XorShift64Star::new(
                    seed_root
                        .wrapping_add((iter_i as u64) * 0x9E37 + wi as u64)
                        .max(1),
                );
                // approximate global pair index for lr decay; exact order doesn't matter for
                // convergence
                let base_pair = (iter_i as u64) * (walks_slice.len() as u64) * pairs_per_walk_avg
                    + (wi as u64) * pairs_per_walk_avg;
                let mut pair_off: u64 = 0;
                for center_pos in 0..walk.len() {
                    let center = walk[center_pos] as usize;
                    let window = rng.next_u64() as usize % window_size + 1;
                    let lo = center_pos.saturating_sub(window);
                    let hi = (center_pos + window + 1).min(walk.len());
                    for ctx_pos in lo..hi {
                        if ctx_pos == center_pos {
                            continue;
                        }
                        let context = walk[ctx_pos] as usize;
                        let lr = decay_lr(initial_lr, min_lr, base_pair + pair_off, total_pairs);
                        pair_off += 1;
                        // SAFETY: Hogwild, concurrent writes accepted as bounded noise.
                        unsafe {
                            sgd_step_raw(
                                in_emb_ptr.0 as *mut f32,
                                out_emb_ptr.0 as *mut f32,
                                center,
                                context,
                                true,
                                lr as f32,
                                dim,
                                exp_table_ref,
                            );
                            for _ in 0..negative_samples {
                                let neg = active_slice[(rng.next_u64() as usize) % active_count].0
                                    as usize;
                                if neg == context {
                                    continue;
                                }
                                sgd_step_raw(
                                    in_emb_ptr.0 as *mut f32,
                                    out_emb_ptr.0 as *mut f32,
                                    center,
                                    neg,
                                    false,
                                    lr as f32,
                                    dim,
                                    exp_table_ref,
                                );
                            }
                        }
                    }
                }
            });
        }
        cancel.check()?;

        let mut valid = vec![false; cap];
        for &v in &active {
            valid[v.0 as usize] = true;
        }
        Ok(NodeVectorResult {
            field_name: "embedding",
            dim,
            values: in_emb,
            valid,
        })
    }
}

/// One p/q-biased walk from `source`.
fn sample_walk(
    source: NodeId,
    walk_length: usize,
    adj: &[Vec<u32>],
    p_inv: f64,
    q_inv: f64,
    rng: &mut XorShift64Star,
) -> Vec<u32> {
    let mut walk: Vec<u32> = Vec::with_capacity(walk_length + 1);
    walk.push(source.0 as u32);
    let first_neighbors = &adj[source.0 as usize];
    if first_neighbors.is_empty() {
        return walk;
    }
    // first step: uniform over neighbors (no previous node yet)
    let next = first_neighbors[(rng.next_u64() as usize) % first_neighbors.len()];
    walk.push(next);
    for _ in 1..walk_length {
        let prev = walk[walk.len() - 2];
        let cur = *walk.last().unwrap();
        let neighbors = &adj[cur as usize];
        if neighbors.is_empty() {
            break;
        }
        let prev_neighbors = &adj[prev as usize];
        // small weighted distribution, O(d_v) per step over the neighbor set; on par with alias
        // tables for typical graphs
        let mut total = 0.0;
        let mut weights: Vec<(u32, f64)> = Vec::with_capacity(neighbors.len());
        for &x in neighbors {
            let w = if x == prev {
                p_inv
            } else if prev_neighbors.binary_search(&x).is_ok() {
                1.0
            } else {
                q_inv
            };
            total += w;
            weights.push((x, w));
        }
        if total == 0.0 {
            break;
        }
        let r = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * total;
        let mut acc = 0.0;
        let mut chosen = weights.last().unwrap().0;
        for &(x, w) in &weights {
            acc += w;
            if r <= acc {
                chosen = x;
                break;
            }
        }
        walk.push(chosen);
    }
    walk
}

/// Hogwild address wrapper for in_emb / out_emb: a `usize` is Send+Sync, re-cast to `*mut f32` in
/// the unsafe SGD step. Workers write concurrently, accepting Hogwild's bounded races (Niu, Recht,
/// Ré, Wright 2011).
#[derive(Copy, Clone)]
struct EmbPtr(usize);

/// One skip-gram SGD step on raw pointers, callable concurrently (Hogwild). The caller keeps the
/// pointers valid for the whole SGD phase.
#[inline]
unsafe fn sgd_step_raw(
    in_emb: *mut f32,
    out_emb: *mut f32,
    center: usize,
    context: usize,
    positive: bool,
    lr: f32,
    dim: usize,
    exp_table: &[f32],
) {
    // SAFETY: the caller keeps both tables alive and sized `rows * dim` through the SGD phase,
    // `center` / `context` are in bounds, and concurrent writes are the bounded Hogwild races
    // above.
    unsafe {
        let cs = center * dim;
        let ws = context * dim;
        let mut dot = 0.0_f32;
        for j in 0..dim {
            dot += *in_emb.add(cs + j) * *out_emb.add(ws + j);
        }
        let sigmoid = sigmoid_approx(dot, exp_table);
        let label = if positive { 1.0 } else { 0.0 };
        let g = lr * (label - sigmoid);
        let mut center_old: [f32; 1024] = [0.0; 1024];
        let cap = dim.min(1024);
        for j in 0..cap {
            center_old[j] = *in_emb.add(cs + j);
        }
        for j in 0..dim {
            *in_emb.add(cs + j) += g * *out_emb.add(ws + j);
        }
        if dim <= 1024 {
            for j in 0..dim {
                *out_emb.add(ws + j) += g * center_old[j];
            }
        } else {
            for j in 0..dim {
                *out_emb.add(ws + j) += g * *in_emb.add(cs + j);
            }
        }
    }
}

const EXP_TABLE_SIZE: usize = 1000;
const MAX_EXP: f32 = 6.0;

fn build_exp_table() -> Vec<f32> {
    let mut t = vec![0.0; EXP_TABLE_SIZE];
    for i in 0..EXP_TABLE_SIZE {
        let x = (i as f32 / EXP_TABLE_SIZE as f32 * 2.0 - 1.0) * MAX_EXP;
        let e = x.exp();
        t[i] = e / (e + 1.0);
    }
    t
}

#[inline]
fn sigmoid_approx(x: f32, table: &[f32]) -> f32 {
    if x >= MAX_EXP {
        return 1.0;
    }
    if x <= -MAX_EXP {
        return 0.0;
    }
    let idx = ((x + MAX_EXP) * (EXP_TABLE_SIZE as f32 / (MAX_EXP * 2.0))) as usize;
    table[idx.min(EXP_TABLE_SIZE - 1)]
}

#[inline]
fn decay_lr(initial: f64, min_lr: f64, step: u64, total: u64) -> f64 {
    if total == 0 {
        return initial;
    }
    let progress = step as f64 / total as f64;
    let lr = initial * (1.0 - progress);
    lr.max(min_lr)
}

fn estimate_pair_count(walks: &[Vec<u32>], window: usize) -> u64 {
    walks
        .iter()
        .map(|w| (w.len() as u64) * 2 * window as u64)
        .sum()
}

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
    super::super::procedure::register(Node2Vec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn produces_embeddings_of_correct_dimension() {
        let mut g = Graph::new();
        let nodes: Vec<_> = (0..6)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..5 {
            g.create_edge(nodes[i], nodes[i + 1], "T", Default::default())
                .unwrap();
            g.create_edge(nodes[i + 1], nodes[i], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = Node2VecConfig {
            dimension: 16,
            walks_per_node: 3,
            walk_length: 10,
            window_size: 3,
            negative_samples: 3,
            iterations: 1,
            return_factor: 1.0,
            in_out_factor: 1.0,
            initial_lr: 0.025,
            min_lr: 1e-4,
            seed: 7,
        };
        let r = Node2Vec
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        assert_eq!(r.dim, 16);
        for &v in &nodes {
            assert!(r.valid[v.0 as usize]);
            let row = &r.values[v.0 as usize * 16..(v.0 as usize + 1) * 16];
            // after SGD the embeddings vary
            let max = row.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
            let min = row.iter().fold(f32::INFINITY, |a, &b| a.min(b));
            assert!(max > min, "embedding row collapsed to constant");
        }
    }
}
