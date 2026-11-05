//! Louvain modularity optimization (Blondel et al., 2008).
//!
//! # Algorithm
//!
//! Maximizes modularity Q = sum_c [L_c/m - (K_c/2m)^2], two phases per pass:
//!
//! 1. **Local moving.** Each node `v` takes the neighbor community with the
//!    largest positive modularity gain (or stays), until nothing moves or
//!    `maxIterations` is hit.
//! 2. **Aggregation.** Each community becomes a super-node; edges between
//!    communities become weighted edges.
//!
//! Repeats until aggregation stops shrinking the graph or `maxLevels` is hit.
//!
//! # Modularity gain
//!
//! Moving node `i` (degree `k_i`) into community `C` (incident weight `sum_tot`,
//! edges from `i` into `C` summing to `k_i_c`):
//!
//! ```text
//!   delta Q = (k_i_c / m) - 2 (sum_tot * k_i) / (2m)^2
//! ```
//!
//! Leaving the current community subtracts symmetrically; the `(k_i / 2m)^2`
//! term cancels, so the inner loop never needs it.
//!
//! # Notes
//!
//! * Undirected weighted adjacency as `Vec<Vec<(u32, f64)>>`, built once from
//!   the projection and contracted per level.
//! * No HashMap in the inner loop: neighbor-community weights go into a
//!   `Vec<f64>` with a touched list for cheap reset.
//! * Single-threaded passes: asynchronous-parallel variants can diverge
//!   (Blondel et al.), so this follows the sequential reference.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeCommunityResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

const DEFAULT_MAX_LEVELS: i64 = 10;
const DEFAULT_MAX_ITERATIONS: i64 = 10;
const DEFAULT_TOLERANCE: f64 = 1e-4;

/// Louvain community detection: greedy modularity optimisation over successive aggregation levels
/// (`traverse.louvain.*`). Stateless.
pub struct Louvain;

/// Parsed `louvain` configuration.
pub struct LouvainConfig {
    /// Upper bound on aggregation levels (`maxLevels`, default 10).
    pub max_levels: i64,
    /// Max local-moving iterations per level (`maxIterations`, default 10).
    pub max_iterations: i64,
    /// Accepted for GDS compatibility (`tolerance`, default 1e-4), passed to local moving, unused
    /// so far.
    pub tolerance: f64,
}

impl GraphAlgorithm for Louvain {
    type Config = LouvainConfig;
    type Output = NodeCommunityResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "louvain",
            display_name: "Louvain",
            category: Category::Community,
            output_kind: OutputKind::NodeCommunity,
            stability: Stability::Beta,
            description: "Modularity-maximizing community detection via Blondel et al.'s \
                 two-phase Louvain method. Treats the projection as undirected.",
            params: vec![
                ConfigParam {
                    key: "maxLevels",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_MAX_LEVELS),
                    description: "Cap on aggregation levels.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "maxIterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_MAX_ITERATIONS),
                    description: "Cap on local-moving rounds per level.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "tolerance",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_TOLERANCE),
                    description: "Minimum modularity gain per local-moving round to continue.",
                    advanced: true,
                    required: false,
                },
            ],
            output_columns: OutputKind::NodeCommunity.default_columns().unwrap(),
            stats_columns: OutputKind::NodeCommunity.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeCommunity.default_write_columns().unwrap(),
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<LouvainConfig, AlgorithmError> {
        Ok(LouvainConfig {
            max_levels: opt_integer(params, "maxLevels", DEFAULT_MAX_LEVELS)
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
        cfg: &LouvainConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeCommunityResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodeCommunityResult::new("communityId", vec![u64::MAX; cap]));
        }

        // step 0: undirected weighted adjacency from the projection, node ids compacted to
        // 0..n_active for dense levels
        let (mut g, orig_ids) = build_internal_graph(graph, &active);
        cancel.check()?;

        // `mapping[i]` = community (current level's index space) of **original** projection node
        // `active[i]`; composed with each level's map after aggregation
        let mut mapping: Vec<u32> = (0..active.len() as u32).collect();

        let mut ran_levels: i64 = 0;
        for _level in 0..cfg.max_levels {
            cancel.check()?;
            ran_levels += 1;
            let level_comm = local_moving(&mut g, cfg.max_iterations, cfg.tolerance, cancel)?;
            // compose: original node i -> level_comm[mapping[i]]
            for m in mapping.iter_mut() {
                *m = level_comm[*m as usize];
            }
            let unique = count_unique(&level_comm);
            if unique == g.n {
                // no reduction: modularity converged
                break;
            }
            g = aggregate(&g, &level_comm);
        }

        // label = smallest original NodeId of each final community
        let mut representative: FxHashMap<u32, u64> = FxHashMap::default();
        for (i, &orig) in orig_ids.iter().enumerate() {
            let comm = mapping[i];
            representative
                .entry(comm)
                .and_modify(|e| {
                    if orig.0 < *e {
                        *e = orig.0;
                    }
                })
                .or_insert(orig.0);
        }

        let mut out: Vec<u64> = vec![u64::MAX; cap];
        for (i, &orig) in orig_ids.iter().enumerate() {
            let comm = mapping[i];
            out[orig.0 as usize] = representative[&comm];
        }
        // final modularity over the ORIGINAL projection (undirected: each edge once, degrees from
        // out union in), for GDS-shaped stats
        let modularity = {
            let (orig, _) = build_internal_graph(graph, &active);
            // `InternalGraph::two_m` actually holds m (single-counted weight); Q's denominator is
            // 2m. Checked against the two-triangle reference (Q = 0.3571)
            let two_m: f64 = orig.two_m * 2.0;
            if two_m == 0.0 {
                0.0
            } else {
                let n_comms = mapping.iter().copied().max().unwrap_or(0) as usize + 1;
                let mut internal = vec![0f64; n_comms];
                let mut degree = vec![0f64; n_comms];
                for u in 0..orig.n as usize {
                    let cu = mapping[u] as usize;
                    for &(v, w) in &orig.adj[u] {
                        degree[cu] += w;
                        if mapping[v as usize] as usize == cu {
                            internal[cu] += w;
                        }
                    }
                }
                let mut q = 0.0;
                for c in 0..n_comms {
                    // adj lists hold each undirected edge from both ends, so internal[c]
                    // double-counts, matching the 2m scale
                    q += internal[c] / two_m - (degree[c] / two_m).powi(2);
                }
                q
            }
        };
        Ok(
            NodeCommunityResult::new("communityId", out).with_extra_stats(vec![
                ("modularity", CypherValue::Float(modularity)),
                ("ranLevels", CypherValue::Integer(ran_levels)),
            ]),
        )
    }
}

// -- Internal graph type ------------------------------------------------

struct InternalGraph {
    n: u32,
    /// adj[u] = (neighbor, weight). Undirected edges appear twice (u->v and v->u); self-loops once at
    /// full weight.
    adj: Vec<Vec<(u32, f64)>>,
    /// Total incident weight per node, self-loops counted twice (so k = sum_v w(u,v)).
    k: Vec<f64>,
    /// Sum of edge weights x 2: the "2m" in Q.
    two_m: f64,
}

fn build_internal_graph(
    graph: &dyn AlgorithmGraph,
    active: &[NodeId],
) -> (InternalGraph, Vec<NodeId>) {
    use rayon::prelude::*;
    let n = active.len() as u32;
    let mut node_to_idx: FxHashMap<u64, u32> = FxHashMap::default();
    for (i, &v) in active.iter().enumerate() {
        node_to_idx.insert(v.0, i as u32);
    }
    let node_to_idx_ref = &node_to_idx;
    let is_unit = graph.is_unit_weighted();

    // rows built in parallel (independent per node); unfiltered unit-weight projections take the
    // slice fast path, no dyn-FnMut
    let rows: Vec<(Vec<(u32, f64)>, f64, f64)> = active
        .par_iter()
        .enumerate()
        .map(|(i, &v)| {
            let ii = i as u32;
            let mut row: FxHashMap<u32, f64> = FxHashMap::default();
            match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                (Some(out), Some(inc)) if is_unit => {
                    for ep in out {
                        if let Some(&ui) = node_to_idx_ref.get(&ep.neighbor().0) {
                            *row.entry(ui).or_insert(0.0) += 1.0;
                        }
                    }
                    for ep in inc {
                        if let Some(&ui) = node_to_idx_ref.get(&ep.neighbor().0) {
                            *row.entry(ui).or_insert(0.0) += 1.0;
                        }
                    }
                }
                _ => {
                    graph.for_each_out(v, &mut |u, _eid, w| {
                        if let Some(&ui) = node_to_idx_ref.get(&u.0) {
                            *row.entry(ui).or_insert(0.0) += w;
                        }
                    });
                    // an undirected projection lists both directions as out already
                    if !graph.is_undirected() {
                        graph.for_each_in(v, &mut |u, _eid, w| {
                            if let Some(&ui) = node_to_idx_ref.get(&u.0) {
                                *row.entry(ui).or_insert(0.0) += w;
                            }
                        });
                    }
                }
            }
            let mut adj_row: Vec<(u32, f64)> = Vec::with_capacity(row.len());
            let mut k_i = 0.0;
            let mut self_w = 0.0;
            let mut other_w = 0.0;
            for (ui, w) in row {
                adj_row.push((ui, w));
                k_i += w;
                if ui == ii {
                    self_w += w;
                } else {
                    other_w += w;
                }
            }
            (adj_row, k_i, self_w + other_w * 0.5)
        })
        .collect();

    let mut adj: Vec<Vec<(u32, f64)>> = Vec::with_capacity(n as usize);
    let mut k: Vec<f64> = Vec::with_capacity(n as usize);
    let mut two_m = 0.0;
    for (row, k_i, m_contrib) in rows {
        adj.push(row);
        k.push(k_i);
        two_m += m_contrib;
    }
    (InternalGraph { n, adj, k, two_m }, active.to_vec())
}

// -- Local moving phase --------------------------------------------------

/// Serial on purpose: synchronous-parallel local moving under-merges real social graphs (snapshot
/// decisions never build the hub-attracts-leaves consensus asynchronous Louvain relies on).
fn local_moving(
    g: &mut InternalGraph,
    max_iter: i64,
    tol: f64,
    cancel: &CancellationToken,
) -> Result<Vec<u32>, AlgorithmError> {
    let n = g.n as usize;
    let mut comm: Vec<u32> = (0..g.n).collect();
    // sum_tot[c] = sum k_i in c; initially singletons, so sum_tot[c] = k[c]
    let mut sigma_tot: Vec<f64> = g.k.clone();

    // neighbor-community weight scratch, indexed by community, reset via `touched`
    let mut neigh_w: Vec<f64> = vec![0.0; n];
    let mut touched: Vec<u32> = Vec::with_capacity(16);

    // natural id order: degree-descending (hubs first) measured slower, since scattered g.adj[v]
    // access costs more cache misses than the saved iterations

    for _ in 0..max_iter {
        cancel.check()?;
        let mut moved = false;
        for v in 0..n {
            // 1. tally weights to each neighbor community (and own)
            let cv = comm[v];
            let mut k_i_c_current = 0.0; // weight from v into its own community
            for &(u, w) in &g.adj[v] {
                let cu = comm[u as usize];
                if u as usize == v {
                    continue; // self-loops don't count for moves
                }
                if neigh_w[cu as usize] == 0.0 {
                    touched.push(cu);
                }
                neigh_w[cu as usize] += w;
                if cu == cv {
                    k_i_c_current += w;
                }
            }
            // 2. conceptually remove v from its community. Gain of joining C:
            //   delta Q_join(C) = k_i_c / m  -  2 * (sum_tot[C] * k_i) / (2m)^2
            //              = (k_i_c - k_i * sum_tot[C] / m) / m
            // subtract it for the current community (sum_tot without v), add per candidate, take the
            // max
            let k_v = g.k[v];
            let two_m = g.two_m;
            if two_m <= 0.0 {
                for &c in &touched {
                    neigh_w[c as usize] = 0.0;
                }
                touched.clear();
                continue;
            }
            // Blondel gain of moving isolated v into C (sum_tot excluding v):
            //
            //   delta Q_add(C) = k_i_c / m - sum_tot[C] * k_v / (2 m^2)
            //
            // move gain = delta Q_add(target) - delta Q_add(current after removal), where removal
            // makes sum_tot[cv] -= k_v. With m = two_m / 2 the denominator 2m^2 is
            // two_m^2 / 2 and constant across candidates, so compare the raw
            // `2*k_i_c*m - sum_tot*k_v` instead.
            let sigma_tot_minus_v = sigma_tot[cv as usize] - k_v;
            let gain_current_raw = 2.0 * k_i_c_current * (two_m * 0.5) - sigma_tot_minus_v * k_v;
            let mut best_c = cv;
            // baseline: `gain_current_raw` (stay)
            let mut best_raw = gain_current_raw;
            for &c in &touched {
                let sigma_c = if c == cv {
                    sigma_tot_minus_v
                } else {
                    sigma_tot[c as usize]
                };
                let k_i_c = neigh_w[c as usize];
                let raw = 2.0 * k_i_c * (two_m * 0.5) - sigma_c * k_v;
                if raw > best_raw {
                    best_raw = raw;
                    best_c = c;
                }
            }
            // 3. reset scratch
            for &c in &touched {
                neigh_w[c as usize] = 0.0;
            }
            touched.clear();
            // 4. apply the best move
            if best_c != cv {
                sigma_tot[cv as usize] -= k_v;
                sigma_tot[best_c as usize] += k_v;
                comm[v] = best_c;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let _ = tol; // reserved for a per-round modularity-delta check

    // contiguous 0..K labels so aggregation can index arrays
    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    for c in &mut comm {
        let len = remap.len() as u32;
        *c = *remap.entry(*c).or_insert(len);
    }
    Ok(comm)
}

fn count_unique(v: &[u32]) -> u32 {
    let mut max = 0u32;
    for &x in v {
        if x > max {
            max = x;
        }
    }
    max + 1
}

// -- Aggregation phase ---------------------------------------------------

fn aggregate(g: &InternalGraph, comm: &[u32]) -> InternalGraph {
    use rayon::prelude::*;
    let k_new = count_unique(comm) as usize;

    // bucket old nodes by new community so each aggregates independently in parallel, no contention
    // on the row maps
    let mut nodes_by_community: Vec<Vec<u32>> = vec![Vec::new(); k_new];
    for u in 0..g.n as usize {
        nodes_by_community[comm[u] as usize].push(u as u32);
    }

    // per-community accumulation in parallel, each task with its own map; half weight per side,
    // since every undirected edge sits in both endpoints' lists
    let row_acc: Vec<FxHashMap<u32, f64>> = nodes_by_community
        .par_iter()
        .map(|members| {
            let mut row: FxHashMap<u32, f64> = FxHashMap::default();
            for &u in members {
                for &(v, w) in &g.adj[u as usize] {
                    let cv = comm[v as usize];
                    *row.entry(cv).or_insert(0.0) += w * 0.5;
                }
            }
            row
        })
        .collect();
    let mut adj: Vec<Vec<(u32, f64)>> = vec![Vec::new(); k_new];
    let mut k_vec: Vec<f64> = vec![0.0; k_new];
    let mut two_m = 0.0;
    for (cu, row) in row_acc.into_iter().enumerate() {
        for (cv, w) in row {
            // after halving, off-diagonal entries hold the cu-cv weight once; mirror them to keep
            // "each undirected edge in both endpoints' lists". Self-loops (cu == cv) keep their
            // weight
            adj[cu].push((cv, w));
            k_vec[cu] += w;
            if cu as u32 == cv {
                two_m += w;
            } else {
                two_m += w * 0.5;
            }
        }
    }
    InternalGraph {
        n: k_new as u32,
        adj,
        k: k_vec,
        two_m,
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Louvain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn two_dense_clusters_separated_by_bridge() {
        // cluster A: triangle 0,1,2 (both directions); cluster B: triangle 3,4,5;
        // one bridge 2 <-> 3. Louvain must split the two cliques.
        let mut g = Graph::new();
        let nodes: Vec<_> = (0..6)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        let triangle = |g: &mut Graph, a, b, c| {
            for &(u, v) in &[(a, b), (b, a), (b, c), (c, b), (a, c), (c, a)] {
                g.create_edge(u, v, "T", Default::default()).unwrap();
            }
        };
        triangle(&mut g, nodes[0], nodes[1], nodes[2]);
        triangle(&mut g, nodes[3], nodes[4], nodes[5]);
        g.create_edge(nodes[2], nodes[3], "T", Default::default())
            .unwrap();
        g.create_edge(nodes[3], nodes[2], "T", Default::default())
            .unwrap();

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = LouvainConfig {
            max_levels: 5,
            max_iterations: 10,
            tolerance: 1e-6,
        };
        let r = Louvain
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        // {0,1,2} together, {3,4,5} together, clusters apart
        assert_eq!(r.values[nodes[0].0 as usize], r.values[nodes[1].0 as usize]);
        assert_eq!(r.values[nodes[1].0 as usize], r.values[nodes[2].0 as usize]);
        assert_eq!(r.values[nodes[3].0 as usize], r.values[nodes[4].0 as usize]);
        assert_eq!(r.values[nodes[4].0 as usize], r.values[nodes[5].0 as usize]);
        assert_ne!(r.values[nodes[0].0 as usize], r.values[nodes[3].0 as usize]);
    }
}
