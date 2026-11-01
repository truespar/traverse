//! GraphSAGE: inductive node embeddings by neighbor aggregation (Hamilton, Ying
//! & Leskovec 2017).
//!
//! # Algorithm
//!
//! For each layer `k = 1..numLayers`:
//!
//! ```text
//!   h_N(v)^k = MEAN({ h_u^{k-1} : u in N(v) })
//!   h_v^k    = sigma( W_k * CONCAT(h_v^{k-1}, h_N(v)^k) )
//! ```
//!
//! The paper trains `W_k` on a self-supervised neighborhood objective. This is
//! the **inference-only** variant: MEAN aggregator, identity projection
//! (`W_k = I`), keeping the structural features GraphSAGE generalizes.
//! `concatenateLayers = true` concatenates per-layer embeddings; `false`
//! (default) emits only the last layer.
//!
//! Unlike FastRP, callers can supply **their own initial features**
//! (`featureProperties`), the inductive reading. Without them, h_v^0 is a sparse
//! random projection (FastRP's Achlioptas matrix).
//!
//! # Notes
//!
//! - **Per-row rayon parallelism**: each layer writes disjoint rows of
//!   `next_emb`, no atomics.
//! - **f32 storage** keeps memory bounded (as FastRP).
//! - **L2 normalization between layers** stops magnitudes exploding on
//!   heavy-tailed degrees.
//! - **No learned weights**: training would replace the identity projection
//!   with SGD-trained matrices from a model registry.

use std::collections::BTreeMap;

use rayon::prelude::*;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_bool, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeVectorResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// GraphSAGE embeddings by mean aggregation over neighbour layers (Hamilton et al.)
/// (`traverse.graphSage.*`). Stateless.
pub struct GraphSage;

/// Parsed `graphSage` configuration.
pub struct GraphSageConfig {
    /// Embedding length per layer (`embeddingDimension`, default 64, > 0).
    pub embedding_dimension: usize,
    /// Aggregation layers, i.e. neighbourhood depth (`numLayers`, default 2, >= 1).
    pub num_layers: usize,
    /// Emit all layers concatenated instead of only the last (`concatenateLayers`, default false).
    pub concatenate_layers: bool,
    /// Seed for the random projection used without feature properties (`randomSeed`, default 42).
    pub random_seed: u64,
    /// Node properties as initial features (`featureProperties`); empty = sparse random projection.
    pub feature_properties: Vec<String>,
}

impl GraphAlgorithm for GraphSage {
    type Config = GraphSageConfig;
    type Output = NodeVectorResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "graphSage",
            display_name: "GraphSAGE (mean aggregator)",
            category: Category::Embeddings,
            output_kind: OutputKind::NodeVector,
            stability: Stability::Beta,
            description: "Inductive node embedding via iterative mean-aggregation over neighborhoods \
                 (Hamilton-Ying-Leskovec 2017). Inference-only: identity projection (no learned \
                 weights). Initial features come from `featureProperties`; falls back to a sparse \
                 random projection if unset. Output dimension is `embeddingDimension` (or that \
                 x numLayers when `concatenateLayers` = true).",
            params: vec![
                ConfigParam {
                    key: "embeddingDimension",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(64),
                    description: "Per-layer embedding dimension.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "numLayers",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(2),
                    description: "Number of mean-aggregation layers (effective neighborhood depth).",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "concatenateLayers",
                    ty: ConfigParamType::Bool,
                    default: CypherValue::Bool(false),
                    description: "If true, output is the concatenation of all layer embeddings.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "featureProperties",
                    ty: ConfigParamType::StringList,
                    default: CypherValue::Null,
                    description: "Node property names to use as initial features. If empty/missing, h_v^0 \
                         is a sparse random projection of `embeddingDimension` dims.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "randomSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(42),
                    description: "Seed for the fallback random projection.",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: OutputKind::NodeVector.default_columns().unwrap(),
            stats_columns: OutputKind::NodeVector.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<GraphSageConfig, AlgorithmError> {
        let embedding_dimension =
            opt_integer(params, "embeddingDimension", 64).map_err(AlgorithmError::from)? as usize;
        let num_layers =
            opt_integer(params, "numLayers", 2).map_err(AlgorithmError::from)? as usize;
        let concatenate_layers =
            opt_bool(params, "concatenateLayers", false).map_err(AlgorithmError::from)?;
        let random_seed =
            opt_integer(params, "randomSeed", 42).map_err(AlgorithmError::from)? as u64;
        let feature_properties = match params.get("featureProperties") {
            Some(CypherValue::List(l)) => l
                .iter()
                .filter_map(|v| match v {
                    CypherValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if embedding_dimension == 0 {
            return Err(AlgorithmError::Config(
                "embeddingDimension must be >= 1".into(),
            ));
        }
        if num_layers == 0 {
            return Err(AlgorithmError::Config("numLayers must be >= 1".into()));
        }
        Ok(GraphSageConfig {
            embedding_dimension,
            num_layers,
            concatenate_layers,
            random_seed,
            feature_properties,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &GraphSageConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeVectorResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let active = graph.active_nodes();
        let n = active.len();
        let d = cfg.embedding_dimension;
        if n == 0 {
            return Ok(NodeVectorResult {
                field_name: "embedding",
                dim: d,
                values: Vec::new(),
                valid: Vec::new(),
            });
        }
        cancel.check()?;

        // step 1: initial embeddings h_v^0
        let mut current: Vec<f32> = if cfg.feature_properties.is_empty() {
            // sparse random projection: +/-1 at ~1/3 density, zeros elsewhere (as FastRP)
            let mut emb = vec![0.0f32; cap * d];
            let scale = (3.0_f64).sqrt() as f32; // Achlioptas constant
            // each active node writes only its own row
            let rows: Vec<(u64, Vec<f32>)> = active
                .par_iter()
                .map(|&v| {
                    let mut row = vec![0.0f32; d];
                    let mut rng = XorShift64Star::new(cfg.random_seed.wrapping_add(v.0).max(1));
                    for j in 0..d {
                        let r = rng.next_u64();
                        let bucket = r % 6;
                        if bucket == 0 {
                            row[j] = scale;
                        } else if bucket == 1 {
                            row[j] = -scale;
                        }
                    }
                    (v.0, row)
                })
                .collect();
            for (vid, row) in rows {
                let off = vid as usize * d;
                emb[off..off + d].copy_from_slice(&row);
            }
            emb
        } else {
            // user properties as the initial vector, one entry per property, zero-padded up to the
            // dimension
            let view = graph.graph();
            let mut emb = vec![0.0f32; cap * d];
            for &v in &active {
                let row_offset = v.0 as usize * d;
                for (i, key) in cfg.feature_properties.iter().enumerate() {
                    if i >= d {
                        break;
                    }
                    let value = match view.get_node_property(v, key) {
                        Ok(Some(PropertyValue::Float(f))) => f as f32,
                        Ok(Some(PropertyValue::Int(i))) => i as f32,
                        Ok(Some(PropertyValue::Bool(b))) if b => 1.0,
                        _ => 0.0,
                    };
                    emb[row_offset + i] = value;
                }
            }
            l2_normalize_rows(&mut emb, d, &active);
            emb
        };

        // per-layer snapshots when concatenating: outputs h_v^1 .. h_v^numLayers-1; the last layer
        // ends in `current`
        let mut layer_snapshots: Vec<Vec<f32>> = Vec::new();

        // step 2: mean-aggregation layers; an active-row mask lets the parallel row loop skip
        // inactive rows in O(1)
        let mut active_mask = vec![false; cap];
        for &v in &active {
            active_mask[v.0 as usize] = true;
        }
        let mut next = vec![0.0f32; cap * d];
        for layer in 0..cfg.num_layers {
            cancel.check()?;
            // for each v, sum h_u over u in N(v) union {v} and divide by (deg + 1), writing straight
            // into v's `next` row via par_chunks_mut (no per-row Vec + copy-back), as fastRP
            let current_ref: &[f32] = &current;
            let active_mask_ref: &[bool] = &active_mask;
            next.par_chunks_mut(d).enumerate().for_each(|(vi, row)| {
                if !active_mask_ref[vi] {
                    for x in row.iter_mut() {
                        *x = 0.0;
                    }
                    return;
                }
                let v = NodeId(vi as u64);
                let self_off = vi * d;
                row[..d].copy_from_slice(&current_ref[self_off..self_off + d]);
                let mut count = 1u32;
                match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                    (Some(out), Some(inc)) => {
                        for ep in out {
                            count += 1;
                            let uo = ep.neighbor().0 as usize * d;
                            for j in 0..d {
                                row[j] += current_ref[uo + j];
                            }
                        }
                        for ep in inc {
                            count += 1;
                            let uo = ep.neighbor().0 as usize * d;
                            for j in 0..d {
                                row[j] += current_ref[uo + j];
                            }
                        }
                    }
                    _ => {
                        graph.for_each_out(v, &mut |u, _eid, _w| {
                            count += 1;
                            let uo = u.0 as usize * d;
                            for j in 0..d {
                                row[j] += current_ref[uo + j];
                            }
                        });
                        graph.for_each_in(v, &mut |u, _eid, _w| {
                            count += 1;
                            let uo = u.0 as usize * d;
                            for j in 0..d {
                                row[j] += current_ref[uo + j];
                            }
                        });
                    }
                }
                let inv = 1.0 / count as f32;
                for j in 0..d {
                    row[j] *= inv;
                }
            });
            std::mem::swap(&mut current, &mut next);
            l2_normalize_rows(&mut current, d, &active);

            if cfg.concatenate_layers && layer + 1 < cfg.num_layers {
                layer_snapshots.push(current.clone());
            }
        }

        // step 3: output in NodeVectorResult's flat layout (`values[i*dim..(i+1)*dim]`)
        let final_dim = if cfg.concatenate_layers {
            d * cfg.num_layers
        } else {
            d
        };
        let mut values: Vec<f32> = vec![0.0; cap * final_dim];
        let mut valid: Vec<bool> = vec![false; cap];
        let active_set: rustc_hash::FxHashSet<u64> = active.iter().map(|n| n.0).collect();
        for i in 0..cap {
            if !active_set.contains(&(i as u64)) {
                continue;
            }
            valid[i] = true;
            let dest = i * final_dim;
            if cfg.concatenate_layers {
                let mut offset = 0usize;
                for snap in &layer_snapshots {
                    values[dest + offset..dest + offset + d]
                        .copy_from_slice(&snap[i * d..(i + 1) * d]);
                    offset += d;
                }
                values[dest + offset..dest + offset + d]
                    .copy_from_slice(&current[i * d..(i + 1) * d]);
            } else {
                values[dest..dest + d].copy_from_slice(&current[i * d..(i + 1) * d]);
            }
        }

        Ok(NodeVectorResult {
            field_name: "embedding",
            dim: final_dim,
            values,
            valid,
        })
    }
}

fn l2_normalize_rows(emb: &mut [f32], d: usize, active: &[NodeId]) {
    // serial rows: the inner loop is dim-bounded, parallelism would gain little
    for v in active {
        let off = v.0 as usize * d;
        let mut sumsq = 0.0f32;
        for j in 0..d {
            sumsq += emb[off + j] * emb[off + j];
        }
        if sumsq > 0.0 {
            let inv = 1.0 / sumsq.sqrt();
            for j in 0..d {
                emb[off + j] *= inv;
            }
        }
    }
}

struct XorShift64Star {
    state: u64,
}

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
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
    super::super::procedure::register(GraphSage);
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
        let n: Vec<_> = (0..5)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = GraphSage
            .run(
                &proj,
                &GraphSageConfig {
                    embedding_dimension: 16,
                    num_layers: 2,
                    concatenate_layers: false,
                    random_seed: 42,
                    feature_properties: Vec::new(),
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.dim, 16);
        // every active node has a length-16 row
        for &v in &n {
            assert!(r.valid[v.0 as usize]);
            let start = v.0 as usize * r.dim;
            let row = &r.values[start..start + r.dim];
            assert_eq!(row.len(), 16);
        }
    }

    #[test]
    fn concatenation_doubles_dimension_for_two_layers() {
        let mut g = Graph::new();
        let n: Vec<_> = (0..3)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        g.create_edge(n[0], n[1], "T", Default::default()).unwrap();
        g.create_edge(n[1], n[2], "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = GraphSage
            .run(
                &proj,
                &GraphSageConfig {
                    embedding_dimension: 8,
                    num_layers: 2,
                    concatenate_layers: true,
                    random_seed: 1,
                    feature_properties: Vec::new(),
                },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.dim, 16);
        let start = n[0].0 as usize * 16;
        let row = &r.values[start..start + 16];
        assert_eq!(row.len(), 16);
    }
}
