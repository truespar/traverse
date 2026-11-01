//! Eigenvector centrality.
//!
//! # Algorithm
//!
//! Power iteration for the dominant eigenvector of the adjacency matrix `A` (in
//! the projection's orientation):
//!
//! ```text
//!   x'[v] = sum_{u -> v} x[u] * w(u,v)
//!   x   = x' / ||x'||_2
//! ```
//!
//! L2 normalization keeps scores bounded. Stops when `||x' - x||_1 < tolerance` or
//! after `maxIterations`.
//!
//! # Notes
//!
//! * Pull-style (per-node accumulation via `for_each_in`), same parallel scheme
//!   as PageRank: disjoint writes, no atomics.
//! * Weighted edges via the projection's `WeightConfig` (the third arg of
//!   `for_each_in`'s closure).
//! * Starts from uniform `1/sqrt(n)`, avoiding the all-zeros fixed point.

use std::collections::BTreeMap;

use rayon::prelude::*;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeScalarResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

const DEFAULT_MAX_ITERATIONS: i64 = 100;
const DEFAULT_TOLERANCE: f64 = 1e-7;

/// Eigenvector centrality by power iteration (`traverse.eigenvector.*`). Stateless.
pub struct Eigenvector;

/// Parsed `eigenvector` config.
pub struct EigenvectorConfig {
    /// Max power-iteration steps (`maxIterations`, default 100).
    pub max_iterations: i64,
    /// Stop once the L1 change between steps falls below this (`tolerance`, default 1e-7).
    pub tolerance: f64,
}

impl GraphAlgorithm for Eigenvector {
    type Config = EigenvectorConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "eigenvector",
            display_name: "Eigenvector Centrality",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Dominant eigenvector of the projection's adjacency matrix, via \
                 L2-normalized power iteration. Nodes connected to other influential \
                 nodes score higher.",
            params: vec![
                ConfigParam {
                    key: "maxIterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_MAX_ITERATIONS),
                    description: "Cap on power-iteration steps.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "tolerance",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_TOLERANCE),
                    description: "L1 distance threshold for early termination.",
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
    ) -> Result<EigenvectorConfig, AlgorithmError> {
        Ok(EigenvectorConfig {
            max_iterations: opt_integer(params, "maxIterations", DEFAULT_MAX_ITERATIONS)
                .map_err(AlgorithmError::from)?,
            tolerance: opt_float(params, "tolerance", DEFAULT_TOLERANCE)
                .map_err(AlgorithmError::from)?,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &EigenvectorConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }

        // uniform seed: 1/sqrt(n) gives ||x||_2 = 1 from the start
        let seed = 1.0 / (n_active as f64).sqrt();
        let mut x: Vec<f64> = vec![0.0; cap];
        for &n in &nodes {
            x[n.0 as usize] = seed;
        }
        let mut x_new: Vec<f64> = vec![0.0; cap];

        // active mask, built once, for direct par_chunks_mut writes
        let mut active_mask: Vec<bool> = vec![false; cap];
        for &n in &nodes {
            active_mask[n.0 as usize] = true;
        }

        for _ in 0..cfg.max_iterations {
            cancel.check()?;

            // x_new[v] = sum_{u->v} w(u,v) * x[u]
            let is_unit = graph.is_unit_weighted();
            let x_ref: &[f64] = &x;
            let active_mask_ref: &[bool] = &active_mask;
            x_new.par_chunks_mut(1).enumerate().for_each(|(idx, slot)| {
                if !active_mask_ref[idx] {
                    slot[0] = 0.0;
                    return;
                }
                let v = NodeId(idx as u64);
                let mut acc = 0.0;
                match graph.incoming_adj_slice(v) {
                    Some(slice) if is_unit => {
                        for ep in slice {
                            acc += x_ref[ep.neighbor().0 as usize];
                        }
                    }
                    Some(_slice) => {
                        graph.for_each_in(v, &mut |u, _eid, w| {
                            acc += x_ref[u.0 as usize] * w;
                        });
                    }
                    None => {
                        graph.for_each_in(v, &mut |u, _eid, w| {
                            acc += x_ref[u.0 as usize] * w;
                        });
                    }
                }
                slot[0] = acc;
            });

            // L2-normalize; a zero vector (no edges into active nodes) means no meaningful
            // centrality, stop early
            let norm_sq: f64 = nodes
                .par_iter()
                .map(|&n| {
                    let v = x_new[n.0 as usize];
                    v * v
                })
                .sum();
            if norm_sq < f64::EPSILON {
                break;
            }
            let inv_norm = 1.0 / norm_sq.sqrt();
            for &n in &nodes {
                x_new[n.0 as usize] *= inv_norm;
            }

            let delta: f64 = nodes
                .par_iter()
                .map(|&n| (x_new[n.0 as usize] - x[n.0 as usize]).abs())
                .sum();
            std::mem::swap(&mut x, &mut x_new);
            if delta < cfg.tolerance {
                break;
            }
        }

        let mut out = vec![f64::NAN; cap];
        for &n in &nodes {
            out[n.0 as usize] = x[n.0 as usize];
        }
        Ok(NodeScalarResult::new("score", out))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Eigenvector);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn star_center_dominates() {
        let mut g = Graph::new();
        let center = g.create_node(&["N"], Default::default());
        let leaves: Vec<_> = (0..5)
            .map(|_| {
                let l = g.create_node(&["N"], Default::default());
                g.create_edge(l, center, "T", Default::default()).unwrap();
                l
            })
            .collect();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = EigenvectorConfig {
            max_iterations: 100,
            tolerance: 1e-9,
        };
        let r = Eigenvector
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        let c = r.values[center.0 as usize];
        for &l in &leaves {
            assert!(
                c > r.values[l.0 as usize],
                "center {c} should outrank leaf {}",
                r.values[l.0 as usize]
            );
        }
    }
}
