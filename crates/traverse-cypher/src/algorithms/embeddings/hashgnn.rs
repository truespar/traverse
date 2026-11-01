//! HashGNN: hash-based message-passing embeddings (Williamson et al., 2022).
//!
//! # Algorithm
//!
//! Each node starts with a sparse binary feature vector; without features it's
//! seeded from `embeddingDensity` random hash buckets derived from the NodeId.
//! Each iteration:
//!
//! 1. sample `embeddingDensity` independent hash functions `h_1, ..., h_k`;
//! 2. for each node `v`, gather the bits set in its neighbors' (and its own)
//!    vectors; the new vector has `1` at `b` iff some hash maps a gathered bit
//!    to `b`.
//!
//! After `iterations` rounds that vector is the embedding: a dense 0/1 vector
//! per node, comparable by Hamming distance / Jaccard.
//!
//! # Notes
//!
//! * **MinHash-like**, with `embeddingDensity` parallel hashes per round, so the
//!   result is a binary embedding rather than one bucket; the HashGNN /
//!   MinHash-GNN form that scales to billions of edges (Williamson et al. 2022;
//!   Nguyen et al. 2024).
//! * **Parallel per-node updates** per round, each rayon task writing its own row.
//! * **xorshift64\*** hash family seeded from config: reproducible embeddings.

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
const DEFAULT_DENSITY: i64 = 2;
const DEFAULT_ITERATIONS: i64 = 3;
const DEFAULT_NEIGHBOR_INFLUENCE: f64 = 1.0;
const DEFAULT_SEED: i64 = 0;

/// HashGNN binary embeddings by hash-based message passing (`traverse.hashGnn.*`). Stateless.
pub struct HashGnn;

/// Parsed `hashGnn` configuration.
pub struct HashGnnConfig {
    /// Binary embedding length (`embeddingDimension`, default 128, > 0).
    pub dimension: usize,
    /// Hash functions per round, roughly set bits per vector (`embeddingDensity`, default 2, > 0).
    pub embedding_density: usize,
    /// Message-passing rounds (`iterations`, default 3, > 0).
    pub iterations: usize,
    /// Probability (clamped to `[0, 1]`) of taking a neighbour's bit instead of the node's own each
    /// round (`neighborInfluence`, default 1.0).
    pub neighbor_influence: f64,
    /// RNG seed (`randomSeed`); `0` uses the wall clock.
    pub seed: u64,
}

impl GraphAlgorithm for HashGnn {
    type Config = HashGnnConfig;
    type Output = NodeVectorResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "hashGnn",
            display_name: "HashGNN",
            category: Category::Embeddings,
            output_kind: OutputKind::NodeVector,
            stability: Stability::Beta,
            description: "Hash-based message-passing embeddings. Produces sparse binary vectors \
                 via repeated MinHash neighbor aggregation.",
            params: vec![
                ConfigParam {
                    key: "embeddingDimension",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_DIM),
                    description: "Length of the binary embedding vector.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "embeddingDensity",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_DENSITY),
                    description: "Number of hash functions per round (~ non-zeros per row).",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "iterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_ITERATIONS),
                    description: "Number of message-passing rounds.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "neighborInfluence",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_NEIGHBOR_INFLUENCE),
                    description: "Per-round probability of including a neighbor's bit (vs self).",
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
    ) -> Result<HashGnnConfig, AlgorithmError> {
        let dimension = opt_integer(params, "embeddingDimension", DEFAULT_DIM)
            .map_err(AlgorithmError::from)? as usize;
        let embedding_density = opt_integer(params, "embeddingDensity", DEFAULT_DENSITY)
            .map_err(AlgorithmError::from)? as usize;
        let iterations = opt_integer(params, "iterations", DEFAULT_ITERATIONS)
            .map_err(AlgorithmError::from)? as usize;
        if dimension == 0 || embedding_density == 0 || iterations == 0 {
            return Err(AlgorithmError::Config(
                "embeddingDimension, embeddingDensity, and iterations must be positive".into(),
            ));
        }
        let neighbor_influence = opt_float(params, "neighborInfluence", DEFAULT_NEIGHBOR_INFLUENCE)
            .map_err(AlgorithmError::from)?;
        let seed_raw =
            opt_integer(params, "randomSeed", DEFAULT_SEED).map_err(AlgorithmError::from)?;
        let seed = if seed_raw == 0 {
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xa5a5_5a5a_a5a5_5a5a)
        } else {
            seed_raw as u64
        };
        Ok(HashGnnConfig {
            dimension,
            embedding_density,
            iterations,
            neighbor_influence,
            seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &HashGnnConfig,
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

        // initial features: `embeddingDensity` pseudo-random bits from each node's NodeId,
        // deterministic for the seed
        let mut features: Vec<bool> = vec![false; cap * dim];
        for &v in &active {
            let row = v.0 as usize * dim;
            let mut h = cfg.seed.wrapping_add(v.0).wrapping_mul(0x9E3779B97F4A7C15);
            for _ in 0..cfg.embedding_density {
                h ^= h >> 12;
                h ^= h << 25;
                h ^= h >> 27;
                h = h.wrapping_mul(0x2545F4914F6CDD1D);
                let bit = (h % dim as u64) as usize;
                features[row + bit] = true;
            }
        }
        cancel.check()?;

        // active-row mask: par_chunks_mut visits EVERY row and zero-fills inactive ones without
        // scanning `active`
        let mut active_mask = vec![false; cap];
        for &v in &active {
            active_mask[v.0 as usize] = true;
        }

        // per-round hash seeds, independent across rounds and the `embedding_density` hashes;
        // writes go straight into v's `new_features` row via par_chunks_mut (no per-row Vec +
        // serial copy-back), as fastRP / graphSage
        let mut new_features: Vec<bool> = vec![false; cap * dim];
        for round in 0..cfg.iterations {
            cancel.check()?;
            let round_seed = cfg
                .seed
                .wrapping_add(round as u64)
                .wrapping_mul(0xC2B2AE3D27D4EB4F);
            let prev: &[bool] = &features;
            let active_mask_ref: &[bool] = &active_mask;
            let neighbor_influence = cfg.neighbor_influence;
            let embedding_density = cfg.embedding_density;

            new_features
                .par_chunks_mut(dim)
                .enumerate()
                .for_each(|(v_idx, row_out)| {
                    for x in row_out.iter_mut() {
                        *x = false;
                    }
                    if !active_mask_ref[v_idx] {
                        return;
                    }
                    let v = NodeId(v_idx as u64);
                    let mut neighbor_bits: Vec<usize> = Vec::with_capacity(dim);
                    let self_row = v_idx * dim;
                    for j in 0..dim {
                        if prev[self_row + j] {
                            neighbor_bits.push(j);
                        }
                    }
                    let mut local_rng = XorShift64Star::new(round_seed.wrapping_add(v.0).max(1));
                    let threshold = (u64::MAX as f64 * neighbor_influence.clamp(0.0, 1.0))
                        .min(u64::MAX as f64) as u64;
                    let mut visit = |u: NodeId, _w: f64| {
                        if u == v {
                            return;
                        }
                        let ui = u.0 as usize;
                        let u_row = ui * dim;
                        for j in 0..dim {
                            if prev[u_row + j] && local_rng.next_u64() < threshold {
                                neighbor_bits.push(j);
                            }
                        }
                    };
                    match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                        (Some(out), Some(inc)) => {
                            for ep in out {
                                visit(NodeId(ep.neighbor().0), 1.0);
                            }
                            for ep in inc {
                                visit(NodeId(ep.neighbor().0), 1.0);
                            }
                        }
                        _ => {
                            graph.for_each_out(v, &mut |u, _eid, w| visit(u, w));
                            graph.for_each_in(v, &mut |u, _eid, w| visit(u, w));
                        }
                    }
                    if neighbor_bits.is_empty() {
                        return;
                    }
                    // `embedding_density` MinHash-style picks set bits directly in our row
                    for k in 0..embedding_density {
                        let hash_seed = round_seed.wrapping_add(k as u64 * 0x9E3779B97F4A7C15);
                        let mut min_h = u64::MAX;
                        let mut min_bit = 0;
                        for &b in &neighbor_bits {
                            let h = mix(hash_seed.wrapping_add(b as u64));
                            if h < min_h {
                                min_h = h;
                                min_bit = b;
                            }
                        }
                        row_out[min_bit] = true;
                    }
                });
            std::mem::swap(&mut features, &mut new_features);
        }

        // to f32 (0.0 / 1.0) for the NodeVectorResult layout; consumers can binarize back or treat
        // it as sparse 0/1
        let mut values: Vec<f32> = vec![0.0; cap * dim];
        for i in 0..cap * dim {
            if features[i] {
                values[i] = 1.0;
            }
        }
        let mut valid = vec![false; cap];
        for &v in &active {
            valid[v.0 as usize] = true;
        }
        Ok(NodeVectorResult {
            field_name: "embedding",
            dim,
            values,
            valid,
        })
    }
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545F4914F6CDD1D)
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
    super::super::procedure::register(HashGnn);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn embeddings_are_dim_sized_and_binary() {
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
        let cfg = HashGnnConfig {
            dimension: 32,
            embedding_density: 2,
            iterations: 2,
            neighbor_influence: 1.0,
            seed: 7,
        };
        let r = HashGnn
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        assert_eq!(r.dim, 32);
        for &v in &nodes {
            let row = &r.values[v.0 as usize * 32..(v.0 as usize + 1) * 32];
            // every entry is 0.0 or 1.0
            for &x in row {
                assert!(x == 0.0 || x == 1.0, "non-binary value {x}");
            }
        }
    }
}
