//! HITS, Hyperlink-Induced Topic Search (Kleinberg, 1999).
//!
//! # Algorithm
//!
//! Two coupled scores per node:
//!
//! ```text
//!   auth(v) = sum_{u -> v} hub(u)        ; pointed AT by good hubs
//!   hub(v)  = sum_{v -> u} auth(u)       ; pointing AT good authorities
//! ```
//!
//! Both vectors are L2-normalized each iteration, converging to the principal
//! eigenvectors of `A^T A` (authority) and `A A^T` (hub).
//!
//! # Output
//!
//! `NodeMulti` with `auth` and `hub`: "good target" vs "good source" roles in
//! directed graphs.
//!
//! # Notes
//!
//! * One loop, two pull-style sweeps; same parallel pattern as PageRank /
//!   Eigenvector, disjoint writes, no atomics.
//! * Stops when `||auth_new - auth||_1 + ||hub_new - hub||_1 < tolerance`.

use std::collections::BTreeMap;

use rayon::prelude::*;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeMultiResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

const DEFAULT_MAX_ITERATIONS: i64 = 50;
const DEFAULT_TOLERANCE: f64 = 1e-7;

/// HITS hub and authority scores (Kleinberg) (`traverse.hits.*`). Stateless.
pub struct Hits;

/// Parsed `hits` config.
pub struct HitsConfig {
    /// Max iterations (`maxIterations`, default 50).
    pub max_iterations: i64,
    /// Stop once the combined L1 change of hub and authority falls below this (`tolerance`, default
    /// 1e-7).
    pub tolerance: f64,
}

impl GraphAlgorithm for Hits {
    type Config = HitsConfig;
    type Output = NodeMultiResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "hits",
            display_name: "HITS",
            category: Category::Centrality,
            output_kind: OutputKind::NodeMulti,
            stability: Stability::Beta,
            description: "Kleinberg's hub + authority scores. Hubs point at many authorities; \
                 authorities are pointed at by many hubs.",
            params: vec![
                ConfigParam {
                    key: "maxIterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_MAX_ITERATIONS),
                    description: "Cap on iteration steps.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "tolerance",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_TOLERANCE),
                    description: "Combined L1-delta threshold for both vectors.",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: &["nodeId", "auth", "hub"],
            stats_columns: OutputKind::NodeMulti.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeMulti.default_write_columns().unwrap(),
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<HitsConfig, AlgorithmError> {
        Ok(HitsConfig {
            max_iterations: opt_integer(params, "maxIterations", DEFAULT_MAX_ITERATIONS)
                .map_err(AlgorithmError::from)?,
            tolerance: opt_float(params, "tolerance", DEFAULT_TOLERANCE)
                .map_err(AlgorithmError::from)?,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &HitsConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeMultiResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeMultiResult::new(
                vec!["auth", "hub"],
                vec![vec![f64::NAN, f64::NAN]; cap],
            ));
        }

        let seed = 1.0 / (n_active as f64).sqrt();
        let mut auth: Vec<f64> = vec![0.0; cap];
        let mut hub: Vec<f64> = vec![0.0; cap];
        for &n in &nodes {
            auth[n.0 as usize] = seed;
            hub[n.0 as usize] = seed;
        }
        let mut auth_new: Vec<f64> = vec![0.0; cap];
        let mut hub_new: Vec<f64> = vec![0.0; cap];

        // active mask, built once, for direct par_chunks_mut writes
        let mut active_mask: Vec<bool> = vec![false; cap];
        for &n in &nodes {
            active_mask[n.0 as usize] = true;
        }

        for _ in 0..cfg.max_iterations {
            cancel.check()?;

            // auth_new[v] = sum_{u->v} hub[u]
            let hub_ref: &[f64] = &hub;
            let active_mask_ref: &[bool] = &active_mask;
            auth_new
                .par_chunks_mut(1)
                .enumerate()
                .for_each(|(idx, slot)| {
                    if !active_mask_ref[idx] {
                        slot[0] = 0.0;
                        return;
                    }
                    let v = NodeId(idx as u64);
                    let mut a = 0.0;
                    if let Some(slice) = graph.incoming_adj_slice(v) {
                        for ep in slice {
                            a += hub_ref[ep.neighbor().0 as usize];
                        }
                    } else {
                        graph.for_each_in(v, &mut |u, _eid, _w| {
                            a += hub_ref[u.0 as usize];
                        });
                    }
                    slot[0] = a;
                });

            // hub_new[v] = sum_{v->u} auth_new[u]
            let auth_new_ref: &[f64] = &auth_new;
            hub_new
                .par_chunks_mut(1)
                .enumerate()
                .for_each(|(idx, slot)| {
                    if !active_mask_ref[idx] {
                        slot[0] = 0.0;
                        return;
                    }
                    let v = NodeId(idx as u64);
                    let mut h = 0.0;
                    if let Some(slice) = graph.outgoing_adj_slice(v) {
                        for ep in slice {
                            h += auth_new_ref[ep.neighbor().0 as usize];
                        }
                    } else {
                        graph.for_each_out(v, &mut |u, _eid, _w| {
                            h += auth_new_ref[u.0 as usize];
                        });
                    }
                    slot[0] = h;
                });

            // L2-normalize each vector separately
            let auth_norm_sq: f64 = nodes
                .par_iter()
                .map(|&n| {
                    let v = auth_new[n.0 as usize];
                    v * v
                })
                .sum();
            let hub_norm_sq: f64 = nodes
                .par_iter()
                .map(|&n| {
                    let v = hub_new[n.0 as usize];
                    v * v
                })
                .sum();
            let auth_inv = if auth_norm_sq > f64::EPSILON {
                1.0 / auth_norm_sq.sqrt()
            } else {
                0.0
            };
            let hub_inv = if hub_norm_sq > f64::EPSILON {
                1.0 / hub_norm_sq.sqrt()
            } else {
                0.0
            };
            for &n in &nodes {
                let idx = n.0 as usize;
                auth_new[idx] *= auth_inv;
                hub_new[idx] *= hub_inv;
            }

            let delta: f64 = nodes
                .par_iter()
                .map(|&n| {
                    let i = n.0 as usize;
                    (auth_new[i] - auth[i]).abs() + (hub_new[i] - hub[i]).abs()
                })
                .sum();
            std::mem::swap(&mut auth, &mut auth_new);
            std::mem::swap(&mut hub, &mut hub_new);
            if delta < cfg.tolerance {
                break;
            }
        }

        let mut values: Vec<Vec<f64>> = vec![vec![f64::NAN, f64::NAN]; cap];
        for &n in &nodes {
            let i = n.0 as usize;
            values[i] = vec![auth[i], hub[i]];
        }
        Ok(NodeMultiResult::new(vec!["auth", "hub"], values))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Hits);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn bipartite_hubs_and_authorities_separate() {
        // 3 hubs all point at 2 authorities
        let mut g = Graph::new();
        let hubs: Vec<_> = (0..3)
            .map(|_| g.create_node(&["H"], Default::default()))
            .collect();
        let auths: Vec<_> = (0..2)
            .map(|_| g.create_node(&["A"], Default::default()))
            .collect();
        for &h in &hubs {
            for &a in &auths {
                g.create_edge(h, a, "T", Default::default()).unwrap();
            }
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Hits
            .run(
                &proj,
                &HitsConfig {
                    max_iterations: 50,
                    tolerance: 1e-9,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // hubs: hub > auth; authorities the opposite
        for &h in &hubs {
            let i = h.0 as usize;
            assert!(
                r.values[i][1] > r.values[i][0],
                "hub idx {i} should have hub > auth"
            );
        }
        for &a in &auths {
            let i = a.0 as usize;
            assert!(
                r.values[i][0] > r.values[i][1],
                "auth idx {i} should have auth > hub"
            );
        }
    }
}
