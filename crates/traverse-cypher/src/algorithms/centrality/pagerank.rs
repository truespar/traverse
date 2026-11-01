//! PageRank centrality.
//!
//! # Algorithm
//!
//! Brin & Page's PageRank:
//!
//! ```text
//!   pr(v) = (1 - d)/N + d * ( sum_{u -> v} pr(u) / out_degree(u)
//!                              + dangling_mass / N )
//! ```
//!
//! `dangling_mass = sum_{u : out_deg(u)=0} pr(u)` is spread uniformly each
//! iteration so the scores stay a probability distribution.
//!
//! # Implementation
//!
//! **Pull-style Jacobi iteration**: each node *reads* its in-neighbors'
//! contributions into its own `new_score[v]`. Writes are exclusive per node, so
//! it parallelises with rayon without atomics. Push-style (`u` writing into
//! `new_score[v]` per out-edge) needs a single thread or atomic float adds;
//! pull is the usual parallel form (GAP benchmark, Ligra, Gunrock).
//!
//! Stops when the L1 distance between old and new scores drops below
//! `tolerance`, or after `maxIterations`.
//!
//! # Complexity
//!
//! O(maxIterations * m + N), m = projected edges. Memory: two `f64` per node.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

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

const DEFAULT_DAMPING: f64 = 0.85;
const DEFAULT_MAX_ITERATIONS: i64 = 20;
const DEFAULT_TOLERANCE: f64 = 1e-7;
/// Cancellation poll interval (nodes) within one iteration's parallel sweep.
const CANCEL_CHECK_PERIOD: usize = 65_536;

/// PageRank by power iteration (`traverse.pageRank.*`). Stateless.
pub struct PageRank;

/// Parsed `pageRank` configuration.
pub struct PageRankConfig {
    /// Probability of following an out-edge instead of teleporting (`dampingFactor`, default 0.85).
    pub damping: f64,
    /// Max power-iteration steps (`maxIterations`, default 20).
    pub max_iterations: i64,
    /// Stops once the L1 change per step is below this (`tolerance`, default 1e-7).
    pub tolerance: f64,
    /// GDS-compatible iteration: per-node teleport `(1-d)` (not `(1-d)/N`) and NO dangling-mass
    /// redistribution (sink mass evaporates), as in Neo4j GDS. Scores then match GDS exactly,
    /// dangling nodes included, which rescaling the normalized variant can't.
    pub gds_mode: bool,
}

impl GraphAlgorithm for PageRank {
    type Config = PageRankConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "pageRank",
            display_name: "PageRank",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Brin & Page's PageRank - random-walk-with-restart score per node. \
                 Higher scores mark nodes that more random walks pass through.",
            params: vec![
                ConfigParam {
                    key: "dampingFactor",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_DAMPING),
                    description: "Probability of following a random outgoing edge (1 - teleport prob).",
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
    ) -> Result<PageRankConfig, AlgorithmError> {
        Ok(PageRankConfig {
            damping: opt_float(params, "dampingFactor", DEFAULT_DAMPING)
                .map_err(AlgorithmError::from)?,
            max_iterations: opt_integer(params, "maxIterations", DEFAULT_MAX_ITERATIONS)
                .map_err(AlgorithmError::from)?,
            tolerance: opt_float(params, "tolerance", DEFAULT_TOLERANCE)
                .map_err(AlgorithmError::from)?,
            // internal: injected by the gds.* adapter
            gds_mode: matches!(params.get("__gds_mode"), Some(CypherValue::Bool(true))),
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &PageRankConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();

        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }

        // score buffers sized `cap`, indexed by NodeId even when the projection skips ids
        let inv_n = 1.0 / n_active as f64;
        // GDS's Pregel initializes every node at alpha = 1 - dampingFactor; when maxIterations
        // stops before convergence (common at the default 20) the initial value still shows in the
        // output
        let init = if cfg.gds_mode {
            1.0 - cfg.damping
        } else {
            inv_n
        };
        let mut scores: Vec<f64> = vec![0.0; cap];
        for &n in &nodes {
            scores[n.0 as usize] = init;
        }
        let mut new_scores: Vec<f64> = vec![0.0; cap];

        // out-degrees once, as f64 (no int->float in the inner loop)
        let mut out_deg: Vec<f64> = vec![0.0; cap];
        for &n in &nodes {
            out_deg[n.0 as usize] = graph.out_degree(n) as f64;
        }

        let d = cfg.damping;
        // GDS: per-node teleport (1-d); classic: (1-d)/N
        let one_minus_d_over_n = if cfg.gds_mode {
            1.0 - d
        } else {
            (1.0 - d) * inv_n
        };

        // active-row mask, built once so par_chunks_mut skips inactive slots in O(1)
        let mut active_mask_for_iter: Vec<bool> = vec![false; cap];
        for &n in &nodes {
            active_mask_for_iter[n.0 as usize] = true;
        }

        // GDS's Pregel counts the INITIALIZATION superstep toward maxIterations: `maxIterations:
        // 20` means 19 aggregation updates. With that, results match GDS's output exactly
        let update_steps = if cfg.gds_mode {
            (cfg.max_iterations - 1).max(0)
        } else {
            cfg.max_iterations
        };
        for _iter in 0..update_steps {
            cancel.check()?;

            // dangling mass: scores of out_degree == 0 nodes (recomputed each step)
            let dangling: f64 = nodes
                .par_iter()
                .map(|&n| {
                    let idx = n.0 as usize;
                    if out_deg[idx] == 0.0 {
                        scores[idx]
                    } else {
                        0.0
                    }
                })
                .sum();
            // GDS never redistributes dangling mass; it evaporates
            let dangling_base = if cfg.gds_mode {
                0.0
            } else {
                d * dangling * inv_n
            };

            // pull contributions for all active nodes in parallel; each node writes only its own
            // `new_scores` slot

            // per-node pull. The slice fast path skips the dyn-FnMut closure without filters
            // (pagerank's hot loop); writes go straight into new_scores via par_chunks_mut, no
            // intermediate Vec or serial copy-back
            let scores_ref: &[f64] = &scores;
            let out_deg_ref: &[f64] = &out_deg;
            let active_mask_ref: &[bool] = &active_mask_for_iter;
            new_scores
                .par_chunks_mut(1)
                .enumerate()
                .for_each(|(idx, slot)| {
                    if !active_mask_ref[idx] {
                        slot[0] = 0.0;
                        return;
                    }
                    if idx % CANCEL_CHECK_PERIOD == 0 {
                        let _ = cancel.check();
                    }
                    let v = NodeId(idx as u64);
                    let mut acc = 0.0;
                    if let Some(slice) = graph.incoming_adj_slice(v) {
                        for ep in slice {
                            let ui = ep.neighbor().0 as usize;
                            let od = out_deg_ref[ui];
                            if od > 0.0 {
                                acc += scores_ref[ui] / od;
                            }
                        }
                    } else {
                        graph.for_each_in(v, &mut |u, _eid, _w| {
                            let od = out_deg_ref[u.0 as usize];
                            if od > 0.0 {
                                acc += scores_ref[u.0 as usize] / od;
                            }
                        });
                    }
                    slot[0] = one_minus_d_over_n + d * acc + dangling_base;
                });

            // convergence: L1 distance between old and new scores over active nodes
            let delta: f64 = nodes
                .par_iter()
                .map(|&n| (new_scores[n.0 as usize] - scores[n.0 as usize]).abs())
                .sum();

            std::mem::swap(&mut scores, &mut new_scores);

            if delta < cfg.tolerance {
                break;
            }
        }

        // inactive slots -> NaN so the stream skips them (NodeScalarResult drops NaN rows)
        let mut out = vec![f64::NAN; cap];
        for &n in &nodes {
            out[n.0 as usize] = scores[n.0 as usize];
        }

        // keeps the AtomicU64 import in use
        let _ = AtomicU64::new(0);
        let _ = Ordering::Relaxed;

        Ok(NodeScalarResult::new("score", out))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(PageRank);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    fn run_pagerank(g: &Graph) -> Vec<f64> {
        let view: &(dyn GraphView + Sync) = g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = PageRankConfig {
            damping: 0.85,
            max_iterations: 50,
            tolerance: 1e-9,
            gds_mode: false,
        };
        let r = PageRank
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        r.values
    }

    #[test]
    fn ring_of_three_distributes_evenly() {
        // a -> b -> c -> a: a directed ring is symmetric
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, a, "T", Default::default()).unwrap();
        let scores = run_pagerank(&g);
        let third = 1.0 / 3.0;
        for &n in &[a, b, c] {
            assert!(
                (scores[n.0 as usize] - third).abs() < 1e-6,
                "expected {third}, got {}",
                scores[n.0 as usize]
            );
        }
    }

    #[test]
    fn star_center_outranks_leaves() {
        // 5 leaves point at the center, which accumulates score
        let mut g = Graph::new();
        let center = g.create_node(&["N"], Default::default());
        let mut leaves = vec![];
        for _ in 0..5 {
            let l = g.create_node(&["N"], Default::default());
            g.create_edge(l, center, "T", Default::default()).unwrap();
            leaves.push(l);
        }
        let scores = run_pagerank(&g);
        let center_score = scores[center.0 as usize];
        for l in leaves {
            assert!(
                center_score > scores[l.0 as usize],
                "center {center_score} should outrank leaf {}",
                scores[l.0 as usize]
            );
        }
    }

    #[test]
    fn scores_sum_to_one() {
        // PageRank is a probability distribution
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, d, "T", Default::default()).unwrap();
        g.create_edge(d, a, "T", Default::default()).unwrap();
        g.create_edge(a, c, "T", Default::default()).unwrap();
        let scores = run_pagerank(&g);
        let sum: f64 = [a, b, c, d].iter().map(|n| scores[n.0 as usize]).sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "scores should sum to 1, got {sum}"
        );
    }

    #[test]
    fn dangling_node_does_not_break_distribution() {
        // a -> b, a -> c; c is dangling (no out-edges)
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(a, c, "T", Default::default()).unwrap();
        let scores = run_pagerank(&g);
        let sum: f64 = [a, b, c].iter().map(|n| scores[n.0 as usize]).sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "scores should sum to 1, got {sum}"
        );
        // all scores positive
        for n in [a, b, c] {
            assert!(scores[n.0 as usize] > 0.0);
        }
    }
}
