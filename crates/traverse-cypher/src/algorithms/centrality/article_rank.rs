//! ArticleRank, a PageRank variant ("ArticleRank: A PageRank-based alternative
//! to numbers of citations", Li & Willett, 2009).
//!
//! # Formula
//!
//! ```text
//!   ar(v) = (1 - d) + d * sum_{u -> v} ar(u) / (out_deg(u) + avg_deg)
//! ```
//!
//! `avg_deg` is the average out-degree of active nodes. The `+ avg_deg` in the denominator
//! damps the boost low-out-degree nodes give their neighbors, countering
//! PageRank's bias toward sinks of citation chains.
//!
//! # Implementation
//!
//! Same pull-style Jacobi loop as PageRank (see `pagerank.rs`: no atomic f64,
//! disjoint writes per task, shares computed once per step). Only the share's
//! denominator differs, and there's no dangling redistribution (`+ avg_deg`
//! already handles low-degree nodes).

use std::collections::BTreeMap;

use rayon::prelude::*;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeScalarResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

const DEFAULT_DAMPING: f64 = 0.85;
const DEFAULT_MAX_ITERATIONS: i64 = 20;
const DEFAULT_TOLERANCE: f64 = 1e-7;

/// ArticleRank, a PageRank variant that discounts nodes with many out-links
/// (`traverse.articleRank.*`). Stateless.
pub struct ArticleRank;

/// Parsed `articleRank` config.
pub struct ArticleRankConfig {
    /// Probability of following an out-edge instead of restarting (`dampingFactor`, default 0.85).
    pub damping: f64,
    /// Max power-iteration steps (`maxIterations`, default 20).
    pub max_iterations: i64,
    /// Stop once the L1 change between steps falls below this (`tolerance`, default 1e-7).
    pub tolerance: f64,
}

impl GraphAlgorithm for ArticleRank {
    type Config = ArticleRankConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "articleRank",
            display_name: "Article Rank",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "PageRank variant biased away from low-out-degree sinks. Each \
                 contribution is divided by (out_degree(u) + avg_out_degree) \
                 instead of out_degree(u).",
            params: vec![
                ConfigParam {
                    key: "dampingFactor",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_DAMPING),
                    description: "Probability of following a random out-edge.",
                    advanced: false,
                    required: false,
                },
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
    ) -> Result<ArticleRankConfig, AlgorithmError> {
        Ok(ArticleRankConfig {
            damping: opt_float(params, "dampingFactor", DEFAULT_DAMPING)
                .map_err(AlgorithmError::from)?,
            max_iterations: opt_integer(params, "maxIterations", DEFAULT_MAX_ITERATIONS)
                .map_err(AlgorithmError::from)?,
            tolerance: opt_float(params, "tolerance", DEFAULT_TOLERANCE)
                .map_err(AlgorithmError::from)?,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &ArticleRankConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }

        // out-degrees and their average up front; the +avg term is fixed across iterations
        let mut out_deg: Vec<f64> = vec![0.0; cap];
        let mut sum_deg = 0.0;
        for &n in &nodes {
            let d = graph.out_degree(n) as f64;
            out_deg[n.0 as usize] = d;
            sum_deg += d;
        }
        let avg_deg = sum_deg / n_active as f64;

        let mut scores: Vec<f64> = vec![0.0; cap];
        let mut new_scores: Vec<f64> = vec![0.0; cap];
        // what each node passes along each out-relationship, score / (out-degree + average)
        let mut share: Vec<f64> = vec![0.0; cap];
        let mut active: Vec<bool> = vec![false; cap];
        for &n in &nodes {
            scores[n.0 as usize] = 1.0 - cfg.damping;
            active[n.0 as usize] = true;
        }
        let denom: Vec<f64> = out_deg.iter().map(|&od| od + avg_deg).collect();

        let d = cfg.damping;
        let one_minus_d = 1.0 - d;
        let chunk = super::node_chunk(cap);
        let in_neighbors = super::InNeighbors::new(graph, cap);

        // on a pool worker, as PageRank's steps
        rayon::scope(|_| -> Result<(), AlgorithmError> {
            for _ in 0..cfg.max_iterations {
                cancel.check()?;

                share
                    .par_chunks_mut(chunk)
                    .zip(scores.par_chunks(chunk))
                    .zip(denom.par_chunks(chunk))
                    .for_each(|((share, scores), denom)| {
                        for ((s, &x), &dn) in share.iter_mut().zip(scores).zip(denom) {
                            *s = if dn > 0.0 { x / dn } else { 0.0 };
                        }
                    });

                let (scores_ref, share_ref, active_ref, in_neighbors) =
                    (&scores, &share, &active, &in_neighbors);
                let delta: f64 = new_scores
                    .par_chunks_mut(chunk)
                    .enumerate()
                    .map(|(c, slots)| {
                        let first = c * chunk;
                        let mut change = 0.0;
                        let range = first..first + slots.len();
                        in_neighbors.sums(share_ref, active_ref, range, |i, acc| {
                            let s = one_minus_d + d * acc;
                            change += (s - scores_ref[i]).abs();
                            slots[i - first] = s;
                        });
                        change
                    })
                    .sum();
                std::mem::swap(&mut scores, &mut new_scores);
                if delta < cfg.tolerance {
                    break;
                }
            }
            Ok(())
        })?;

        let mut out = vec![f64::NAN; cap];
        for &n in &nodes {
            out[n.0 as usize] = scores[n.0 as usize];
        }
        Ok(NodeScalarResult::new("score", out))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(ArticleRank);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn hub_outranks_leaves_in_star() {
        let mut g = Graph::new();
        let hub = g.create_node(&["N"], Default::default());
        let leaves: Vec<_> = (0..4)
            .map(|_| {
                let l = g.create_node(&["N"], Default::default());
                g.create_edge(l, hub, "T", Default::default()).unwrap();
                l
            })
            .collect();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = ArticleRankConfig {
            damping: 0.85,
            max_iterations: 30,
            tolerance: 1e-9,
        };
        let r = ArticleRank
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        let hub_score = r.values[hub.0 as usize];
        for &l in &leaves {
            assert!(
                hub_score > r.values[l.0 as usize],
                "hub {hub_score} should outrank leaf {}",
                r.values[l.0 as usize]
            );
        }
    }
}
