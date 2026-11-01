//! CELF (Cost-Effective Lazy Forward) influence maximization.
//!
//! # Problem
//!
//! Pick k seeds S maximizing expected spread sigma(S) under the Independent Cascade
//! model: each newly active node tries once to activate each out-neighbor with
//! probability `p`. sigma(S) is the expected final active-set size, estimated by
//! Monte Carlo over R trials.
//!
//! # Algorithm (Leskovec et al. 2007, "Cost-effective outbreak detection")
//!
//! sigma is monotone submodular, so the marginal gain `delta(v | S) = sigma(S union {v}) - sigma(S)`
//! never grows as S grows. CELF uses that:
//!
//! 1. Compute `delta(v | {})` for every v; push (delta, v, 0) on a max-heap.
//! 2. Each of k rounds:
//!    a. pop (delta_top, v, last_updated);
//!    b. if `last_updated == round` the gain is fresh: accept v, sigma(S) += delta_top;
//!    c. else recompute delta(v | S), reinsert with the current round, repeat.
//!
//! Up to ~700x fewer spread evaluations than naive greedy, with the same
//! (1 - 1/e) approximation guarantee.
//!
//! # Notes
//!
//! - **Parallel Monte Carlo**: a candidate's R simulations are independent
//!   (rayon); each is an O(m) forward propagation over the active frontier.
//! - **Dense activation buffer** per trial, faster than a HashSet on typical graphs.
//! - **CELF++** (one extra `prev_best` cache) would save more evaluations; not
//!   implemented.

use std::collections::{BTreeMap, BinaryHeap};

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

/// Influence maximisation with CELF: lazy greedy seed selection under the independent-cascade model
/// (`traverse.celf.*`). Stateless.
pub struct Celf;

/// Parsed `celf` configuration.
pub struct CelfConfig {
    /// Seeds to select (`seedSetSize`, default 10, >= 1).
    pub k: usize,
    /// Chance an active node activates each neighbour in a cascade (`propagationProbability`,
    /// default 0.1, in `[0, 1]`).
    pub propagation_probability: f64,
    /// Cascade simulations per marginal-gain estimate (`monteCarloSamples`, default 100, >= 1).
    pub monte_carlo_samples: usize,
    /// Simulation seed, for reproducible runs (`randomSeed`, default 42).
    pub random_seed: u64,
}

impl GraphAlgorithm for Celf {
    type Config = CelfConfig;
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "celf",
            display_name: "CELF Influence Maximization",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Pick k seed nodes that maximize expected influence spread under the Independent \
                 Cascade model, via Cost-Effective Lazy Forward greedy (Leskovec et al. 2007). \
                 Marginal gain via parallel Monte-Carlo simulation. Output: per-node marginal \
                 contribution if chosen as a seed, 0 otherwise.",
            params: vec![
                ConfigParam {
                    key: "seedSetSize",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(10),
                    description: "Number of seed nodes to select.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "propagationProbability",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(0.1),
                    description: "Per-edge activation probability in the IC model.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "monteCarloSamples",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(100),
                    description: "Number of IC simulations per marginal-gain estimate.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "randomSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(42),
                    description: "Seed for reproducible Monte-Carlo draws.",
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
    ) -> Result<CelfConfig, AlgorithmError> {
        let k = opt_integer(params, "seedSetSize", 10).map_err(AlgorithmError::from)? as usize;
        let p = opt_float(params, "propagationProbability", 0.1).map_err(AlgorithmError::from)?;
        let mc =
            opt_integer(params, "monteCarloSamples", 100).map_err(AlgorithmError::from)? as usize;
        let seed = opt_integer(params, "randomSeed", 42).map_err(AlgorithmError::from)? as u64;
        if k == 0 {
            return Err(AlgorithmError::Config("seedSetSize must be >= 1".into()));
        }
        if !(0.0..=1.0).contains(&p) {
            return Err(AlgorithmError::Config(
                "propagationProbability must be in [0, 1]".into(),
            ));
        }
        if mc == 0 {
            return Err(AlgorithmError::Config(
                "monteCarloSamples must be >= 1".into(),
            ));
        }
        Ok(CelfConfig {
            k,
            propagation_probability: p,
            monte_carlo_samples: mc,
            random_seed: seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &CelfConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodeScalarResult::new("marginalGain", vec![f64::NAN; cap]));
        }
        let k = cfg.k.min(active.len());
        cancel.check()?;

        // packed out-adjacency (IC needs only edge existence + the probability), collected once in
        // parallel for the Monte Carlo loop
        let mut out_adj: Vec<Vec<NodeId>> = vec![Vec::new(); cap];
        let rows: Vec<(usize, Vec<NodeId>)> = active
            .par_iter()
            .map(|&u| {
                let mut list: Vec<NodeId> = Vec::new();
                if let Some(slice) = graph.outgoing_adj_slice(u) {
                    for ep in slice {
                        list.push(NodeId(ep.neighbor().0));
                    }
                } else {
                    graph.for_each_out(u, &mut |v, _eid, _w| {
                        list.push(v);
                    });
                }
                (u.0 as usize, list)
            })
            .collect();
        for (idx, list) in rows {
            out_adj[idx] = list;
        }

        // 1) initial gains sigma({v}) for every v (S = {})
        let p_act = cfg.propagation_probability;
        let mc = cfg.monte_carlo_samples;
        let seed_root = cfg.random_seed;

        // pass `cancel` down so a timeout or memory ceiling interrupts ic_spread mid-simulation,
        // not after the client is gone
        let initial: Result<Vec<(f64, NodeId)>, AlgorithmError> = active
            .par_iter()
            .map(|&v| -> Result<(f64, NodeId), AlgorithmError> {
                let g = ic_spread(&out_adj, &[v], cap, p_act, mc, seed_root ^ v.0, cancel)?;
                Ok((g, v))
            })
            .collect();
        let initial = initial?;

        // 2) CELF lazy greedy over a max-heap keyed by (gain, round); BinaryHeap is a max-heap, so
        //    a totally ordered float puts the largest gain on top
        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(initial.len());
        for (g, v) in initial {
            heap.push(HeapEntry {
                gain: OrderedF64(g),
                node: v,
                last_updated: 0,
            });
        }

        let mut seed_set: Vec<NodeId> = Vec::with_capacity(k);
        let mut seed_gains: Vec<f64> = Vec::with_capacity(k);
        let mut sigma_s: f64 = 0.0;
        // chosen nodes: skip their stale heap entries (never reinserted, but older entries may
        // remain)
        let mut chosen: Vec<bool> = vec![false; cap];

        for round in 1..=k {
            loop {
                cancel.check()?;
                let top = match heap.pop() {
                    Some(t) => t,
                    None => break,
                };
                let v_idx = top.node.0 as usize;
                if v_idx < cap && chosen[v_idx] {
                    continue;
                }
                if top.last_updated == round {
                    // fresh gain: accept v
                    seed_set.push(top.node);
                    seed_gains.push(top.gain.0);
                    if v_idx < cap {
                        chosen[v_idx] = true;
                    }
                    sigma_s += top.gain.0;
                    break;
                }
                // stale: recompute delta(v | S), reinsert
                let mut candidate_seeds = seed_set.clone();
                candidate_seeds.push(top.node);
                let sigma_new = ic_spread(
                    &out_adj,
                    &candidate_seeds,
                    cap,
                    p_act,
                    mc,
                    seed_root
                        .wrapping_add(round as u64)
                        .wrapping_add(top.node.0),
                    cancel,
                )?;
                let new_gain = (sigma_new - sigma_s).max(0.0);
                heap.push(HeapEntry {
                    gain: OrderedF64(new_gain),
                    node: top.node,
                    last_updated: round,
                });
            }
        }

        // scatter into NodeScalar: seeds carry their marginal gain, others NaN (like other
        // centrality outputs)
        let mut values = vec![f64::NAN; cap];
        for (i, &seed) in seed_set.iter().enumerate() {
            values[seed.0 as usize] = seed_gains[i];
        }
        Ok(NodeScalarResult::new("marginalGain", values))
    }
}

/// Estimates sigma(S) under Independent Cascade with R Monte Carlo trials: forward BFS from S, each
/// activation flipping an inactive neighbor with probability p. Returns the average activated
/// count.
fn ic_spread(
    out_adj: &[Vec<NodeId>],
    seeds: &[NodeId],
    cap: usize,
    p: f64,
    r: usize,
    rng_seed: u64,
    cancel: &CancellationToken,
) -> Result<f64, AlgorithmError> {
    if seeds.is_empty() || p <= 0.0 {
        return Ok(seeds.len() as f64);
    }
    // R trials in parallel; each checks cancel once before starting (a cheap thread-local read), so
    // orphaned work stops per trial
    let threshold = (p.clamp(0.0, 1.0) * u64::MAX as f64) as u64;
    let total: Result<u64, AlgorithmError> = (0..r)
        .into_par_iter()
        .map_init(
            // per-worker scratch: epoch bitset (u32 compared to a per-trial epoch, no memset per
            // trial) + reusable frontiers
            || {
                (
                    vec![0u32; cap],
                    Vec::<NodeId>::with_capacity(64),
                    Vec::<NodeId>::with_capacity(64),
                    0u32,
                )
            },
            |scratch, trial| -> Result<u64, AlgorithmError> {
                cancel.check()?;
                let (activated, frontier, next, epoch) = scratch;
                *epoch = epoch.wrapping_add(1);
                if *epoch == 0 {
                    for a in activated.iter_mut() {
                        *a = 0;
                    }
                    *epoch = 1;
                }
                let ep = *epoch;
                let mut rng = XorShift64Star::new(rng_seed.wrapping_add(trial as u64).max(1));
                frontier.clear();
                next.clear();
                for &s in seeds {
                    let si = s.0 as usize;
                    if si < cap && activated[si] != ep {
                        activated[si] = ep;
                        frontier.push(s);
                    }
                }
                let mut count: u64 = frontier.len() as u64;
                while !frontier.is_empty() {
                    for &u in frontier.iter() {
                        let ui = u.0 as usize;
                        if ui >= out_adj.len() {
                            continue;
                        }
                        for &v in &out_adj[ui] {
                            let vi = v.0 as usize;
                            if vi >= cap || activated[vi] == ep {
                                continue;
                            }
                            if rng.next_u64() < threshold {
                                activated[vi] = ep;
                                count += 1;
                                next.push(v);
                            }
                        }
                    }
                    frontier.clear();
                    std::mem::swap(frontier, next);
                }
                Ok(count)
            },
        )
        .try_reduce(|| 0u64, |a, b| Ok(a + b));
    Ok(total? as f64 / r as f64)
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

#[derive(Clone, Copy)]
struct HeapEntry {
    gain: OrderedF64,
    node: NodeId,
    last_updated: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.gain == other.gain && self.node == other.node
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap: order by gain DESC
        self.gain.cmp(&other.gain)
    }
}

#[derive(Debug, Clone, Copy)]
struct OrderedF64(f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Celf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn hub_node_is_first_seed_in_star_graph() {
        // star: center 0 linked to 1..10. With p=1.0 node 0 infects every leaf; a leaf only itself
        let mut g = Graph::new();
        let n: Vec<_> = (0..11)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 1..11 {
            g.create_edge(n[0], n[i], "T", Default::default()).unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Celf
            .run(
                &proj,
                &CelfConfig {
                    k: 1,
                    propagation_probability: 1.0,
                    monte_carlo_samples: 5,
                    random_seed: 42,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // only node 0 has a non-NaN marginalGain
        assert!(r.values[n[0].0 as usize].is_finite());
        for i in 1..11 {
            assert!(r.values[n[i].0 as usize].is_nan());
        }
        // spread 11 (center + 10 leaves) at p=1
        assert!((r.values[n[0].0 as usize] - 11.0).abs() < 1e-9);
    }
}
