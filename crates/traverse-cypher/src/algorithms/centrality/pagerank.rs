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
//! Each step first computes every node's share, `pr(u) / out_degree(u)`, so a
//! relationship costs one load and one add; on a GDS projection the pull reads
//! the snapshot's CSR directly ([`AlgorithmGraph::in_adjacency`]).
//!
//! Stops when the L1 distance between old and new scores drops below
//! `tolerance` (in GDS mode: when every score changes by less), or after
//! `maxIterations`.
//!
//! # Complexity
//!
//! O(maxIterations * m + N), m = projected edges. Memory: four `f64` per node.

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

/// PageRank by power iteration (`traverse.pageRank.*`). Stateless.
pub struct PageRank;

/// Parsed `pageRank` configuration.
pub struct PageRankConfig {
    /// Probability of following an out-edge instead of teleporting (`dampingFactor`, default 0.85).
    pub damping: f64,
    /// Max power-iteration steps (`maxIterations`, default 20).
    pub max_iterations: i64,
    /// Stops once the L1 change per step is below this (`tolerance`, default 1e-7); in GDS mode,
    /// once every node's change is.
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

        // per-node buffers sized `cap`, indexed by NodeId even when the projection skips ids;
        // inactive slots stay 0 in every one
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
        let mut new_scores: Vec<f64> = vec![0.0; cap];
        // what each node passes along each out-relationship, score / out-degree (0 for a sink):
        // one division per node and step, none per relationship
        let mut share: Vec<f64> = vec![0.0; cap];
        let mut out_deg: Vec<f64> = vec![0.0; cap];
        let mut active: Vec<bool> = vec![false; cap];
        for &n in &nodes {
            let i = n.0 as usize;
            scores[i] = init;
            out_deg[i] = graph.out_degree(n) as f64;
            active[i] = true;
        }

        let d = cfg.damping;
        let gds = cfg.gds_mode;
        // GDS: per-node teleport (1-d); classic: (1-d)/N
        let teleport = if gds { 1.0 - d } else { (1.0 - d) * inv_n };
        // GDS's Pregel counts the INITIALIZATION superstep toward maxIterations: `maxIterations:
        // 20` means 19 aggregation updates. With that, results match GDS's output exactly
        let update_steps = if gds {
            (cfg.max_iterations - 1).max(0)
        } else {
            cfg.max_iterations
        };
        let chunk = super::node_chunk(cap);
        let in_neighbors = super::InNeighbors::new(graph, cap);

        // the steps run on a pool worker: each pass below then starts there, instead of the calling
        // thread handing it to the pool and waiting, twice per step
        rayon::scope(|_| -> Result<(), AlgorithmError> {
            for _ in 0..update_steps {
                cancel.check()?;

                // shares, and the dangling mass (the sinks' scores) on the way
                let dangling: f64 = share
                    .par_chunks_mut(chunk)
                    .zip(scores.par_chunks(chunk))
                    .zip(out_deg.par_chunks(chunk))
                    .map(|((share, scores), out_deg)| {
                        let mut sinks = 0.0;
                        // selects, not branches: the loop vectorizes
                        for ((s, &x), &od) in share.iter_mut().zip(scores).zip(out_deg) {
                            let sink = od == 0.0;
                            *s = if sink { 0.0 } else { x / od };
                            sinks += if sink { x } else { 0.0 };
                        }
                        sinks
                    })
                    .sum();
                // GDS never redistributes dangling mass; it evaporates
                let dangling_base = if gds { 0.0 } else { d * dangling * inv_n };

                // pull: each node sums its in-neighbors' shares into its own slot (no atomics),
                // and the chunk's largest and total change come back with it
                let (scores_ref, share_ref, active_ref, in_neighbors) =
                    (&scores, &share, &active, &in_neighbors);
                let (max_change, total_change) = new_scores
                    .par_chunks_mut(chunk)
                    .enumerate()
                    .map(|(c, slots)| {
                        let first = c * chunk;
                        let (mut max, mut total) = (0.0f64, 0.0f64);
                        let range = first..first + slots.len();
                        in_neighbors.sums(share_ref, active_ref, range, |i, acc| {
                            let s = teleport + d * acc + dangling_base;
                            let change = (s - scores_ref[i]).abs();
                            total += change;
                            if change > max {
                                max = change;
                            }
                            slots[i - first] = s;
                        });
                        (max, total)
                    })
                    .reduce(|| (0.0, 0.0), |a, b| (a.0.max(b.0), a.1 + b.1));
                // GDS stops once every score changes by less than the tolerance, the classic
                // iteration once the L1 distance drops below it
                let delta = if gds { max_change } else { total_change };

                std::mem::swap(&mut scores, &mut new_scores);

                if delta < cfg.tolerance {
                    break;
                }
            }
            Ok(())
        })?;

        // inactive slots -> NaN so the stream skips them (NodeScalarResult drops NaN rows)
        let mut out = vec![f64::NAN; cap];
        for &n in &nodes {
            out[n.0 as usize] = scores[n.0 as usize];
        }
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
