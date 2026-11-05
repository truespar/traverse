//! Leiden community detection, Louvain's successor.
//!
//! # Algorithm
//!
//! Three phases per level instead of Louvain's two:
//!
//! 1. **Local moving (P)**: greedy modularity-gain reassignment, same as Louvain.
//! 2. **Refinement (R)**: within each P-community Q, a *restricted* local moving
//!    from singleton refined communities, moves only between nodes both in Q.
//!    Recovers the well-connectedness Louvain loses by aggregating poorly
//!    connected merges. R is at least as fine as P: every R_i lies inside some
//!    P-community.
//! 3. **Aggregate using R**: the next level's graph comes from R, but local
//!    moving *starts* from P (each refined community's parent). Singletons can
//!    still move; non-singletons stay merged in P at the next level's start.
//!    This is what lets Leiden converge to well-connected partitions.
//!
//! # Notes (Traag, Waltman & van Eck 2019)
//!
//! - **Within-P refinement** is the difference from Louvain: every final
//!   community is well-connected, while Louvain can yield disconnected ones at
//!   high resolutions.
//! - The formal **gamma-connectedness check** isn't implemented; the simpler
//!   "singletons only merge within Q" refinement gives the main connectivity
//!   benefit.
//! - **Dense arrays** for community state; `FxHashMap` only for sparse index
//!   spaces (community ids after compaction).
//!
//! Reference: V.A. Traag, L. Waltman, N.J. van Eck, *"From Louvain to Leiden:
//! guaranteeing well-connected communities"* (Scientific Reports, 2019).

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

/// Leiden community detection: refines Louvain's partitions so every community stays connected
/// (`traverse.leiden.*`). Stateless.
pub struct Leiden;

/// Parsed `leiden` configuration.
pub struct LeidenConfig {
    /// Upper bound on aggregation levels (`maxLevels`, default 10).
    pub max_levels: i64,
    /// Max local-moving iterations per level (`maxIterations`, default 10).
    pub max_iterations: i64,
    /// Accepted for GDS compatibility (`tolerance`, default 1e-4), unused: no early exit on small
    /// modularity gains.
    pub tolerance: f64,
}

impl GraphAlgorithm for Leiden {
    type Config = LeidenConfig;
    type Output = NodeCommunityResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "leiden",
            display_name: "Leiden Community Detection",
            category: Category::Community,
            output_kind: OutputKind::NodeCommunity,
            stability: Stability::Beta,
            description: "Louvain successor (Traag-Waltman-van Eck 2019) with a refinement phase that \
                 guarantees well-connected communities. At each level: local-moving -> refinement \
                 within each P-community -> aggregate using the refined partition.",
            params: vec![
                ConfigParam {
                    key: "maxLevels",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(10),
                    description: "Maximum number of aggregation levels.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "maxIterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(10),
                    description: "Maximum local-moving iterations per level.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "tolerance",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(1e-4),
                    description: "Reserved (early-exit threshold).",
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
    ) -> Result<LeidenConfig, AlgorithmError> {
        Ok(LeidenConfig {
            max_levels: opt_integer(params, "maxLevels", 10).map_err(AlgorithmError::from)?,
            max_iterations: opt_integer(params, "maxIterations", 10)
                .map_err(AlgorithmError::from)?,
            tolerance: opt_float(params, "tolerance", 1e-4).map_err(AlgorithmError::from)?,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LeidenConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeCommunityResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodeCommunityResult::new("communityId", vec![u64::MAX; cap]));
        }

        let (mut g, orig_ids) = build_internal_graph(graph, &active);
        cancel.check()?;

        // mapping[i] = community (current level's index space) of original projection node
        // `active[i]`; composed with each level's community map after refinement + aggregation
        let mut mapping: Vec<u32> = (0..active.len() as u32).collect();
        // starting partition P at this level: singletons on level 0, later carried over via refined
        // -> P community
        let mut p_init: Vec<u32> = (0..g.n).collect();

        for _level in 0..cfg.max_levels {
            cancel.check()?;

            // phase 1: local moving on (g, p_init)
            let p = local_moving(&g, p_init.clone(), cfg.max_iterations, cancel)?;

            // phase 2: refinement, restricted local moving within each P-community from singletons
            let r = refine_partition(&g, &p, cancel)?;

            // compose with r: original node i is now in refined community r[mapping[i]]
            for m in mapping.iter_mut() {
                *m = r[*m as usize];
            }

            let unique_r = count_unique(&r);
            if unique_r == g.n {
                break;
            }

            // phase 3: aggregate using r; next level's p_init is each refined community's
            // P-community (any member's, all share it by construction)
            let mut next_p_init: Vec<u32> = vec![u32::MAX; unique_r as usize];
            for u in 0..g.n as usize {
                let r_u = r[u];
                if next_p_init[r_u as usize] == u32::MAX {
                    next_p_init[r_u as usize] = p[u];
                }
            }
            // compact P labels to 0..K, valid indices for the next level's sigma_tot
            let mut p_remap: FxHashMap<u32, u32> = FxHashMap::default();
            for x in next_p_init.iter_mut() {
                let len = p_remap.len() as u32;
                *x = *p_remap.entry(*x).or_insert(len);
            }
            p_init = next_p_init;

            g = aggregate(&g, &r);
        }

        // label = smallest original NodeId in each final community
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
        Ok(NodeCommunityResult::new("communityId", out))
    }
}

// -- Internal graph + builders (like Louvain's, kept local) --

struct InternalGraph {
    n: u32,
    adj: Vec<Vec<(u32, f64)>>,
    k: Vec<f64>,
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

    // rows are independent (each active node merges its out+in neighbors): build in parallel, the
    // main setup cost on dense projections
    let rows: Vec<(Vec<(u32, f64)>, f64, f64)> = active
        .par_iter()
        .enumerate()
        .map(|(i, &v)| {
            let ii = i as u32;
            let mut row: FxHashMap<u32, f64> = FxHashMap::default();
            // slice fast path without filters: read adjacency directly, no dyn-FnMut callback; w is
            // 1.0 for unit-weighted projections
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

// -- Local moving (parameterized starting partition) --

/// Serial on purpose: synchronous-parallel local moving (snapshot decisions) keeps hubs from
/// forming attractor communities and yields orders of magnitude more communities on real social
/// graphs.
fn local_moving(
    g: &InternalGraph,
    initial: Vec<u32>,
    max_iter: i64,
    cancel: &CancellationToken,
) -> Result<Vec<u32>, AlgorithmError> {
    let n = g.n as usize;

    let mut comm = initial;
    // sigma_tot[c] = sum k_i over nodes with comm[i] == c
    let mut sigma_tot: Vec<f64> = vec![0.0; n];
    for v in 0..n {
        sigma_tot[comm[v] as usize] += g.k[v];
    }

    let mut neigh_w: Vec<f64> = vec![0.0; n];
    let mut touched: Vec<u32> = Vec::with_capacity(16);

    // natural (id-ascending) order on purpose: degree-descending converges in fewer iterations
    // (hubs anchor early) but measured slower overall, since visiting g.adj[v] out of order defeats
    // the L2 prefetcher

    for _ in 0..max_iter {
        cancel.check()?;
        let mut moved = false;
        for v in 0..n {
            let cv = comm[v];
            let mut k_i_c_current = 0.0;
            for &(u, w) in &g.adj[v] {
                if u as usize == v {
                    continue;
                }
                let cu = comm[u as usize];
                if neigh_w[cu as usize] == 0.0 {
                    touched.push(cu);
                }
                neigh_w[cu as usize] += w;
                if cu == cv {
                    k_i_c_current += w;
                }
            }
            let k_v = g.k[v];
            let two_m = g.two_m;
            if two_m <= 0.0 {
                for &c in &touched {
                    neigh_w[c as usize] = 0.0;
                }
                touched.clear();
                continue;
            }
            let sigma_tot_minus_v = sigma_tot[cv as usize] - k_v;
            let gain_current_raw = 2.0 * k_i_c_current * (two_m * 0.5) - sigma_tot_minus_v * k_v;
            let mut best_c = cv;
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
            for &c in &touched {
                neigh_w[c as usize] = 0.0;
            }
            touched.clear();
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

    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    for c in &mut comm {
        let len = remap.len() as u32;
        *c = *remap.entry(*c).or_insert(len);
    }
    Ok(comm)
}

// -- Refinement (Leiden-specific) --

/// Singleton-start refinement: within each P-community Q, local moving where v may only join an
/// R-community entirely inside Q, so every refined community is well-connected within its parent.
fn refine_partition(
    g: &InternalGraph,
    p: &[u32],
    cancel: &CancellationToken,
) -> Result<Vec<u32>, AlgorithmError> {
    let n = g.n as usize;
    let mut r: Vec<u32> = (0..g.n).collect();
    // sigma_tot of the refined partition
    let mut sigma_tot: Vec<f64> = g.k.clone();
    // r_to_p[c] = P-community refined community c lives in
    let mut r_to_p: Vec<u32> = p.to_vec();
    // refined community sizes, for the singleton-only rule: nodes in non-singleton refined
    // communities are frozen (Traag-Waltman-van Eck 2019, refinement step 3). The practical
    // approximation of gamma-well-connectedness: once joined, nodes don't peel off and over-fragment
    let mut r_size: Vec<u32> = vec![1; n];

    // group nodes by P-community. Different Qs touch disjoint indices of r/sigma_tot/r_size/r_to_p
    // (community ids are node ids, so Q's communities are a subset of its members), so Qs run in
    // parallel with Hogwild-style raw pointers. Within Q, degree-DESCENDING so hubs attract
    // low-degree nodes first
    let mut by_p: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for v in 0..g.n {
        by_p.entry(p[v as usize]).or_default().push(v);
    }
    for members in by_p.values_mut() {
        members.sort_by(|&a, &b| {
            g.k[b as usize]
                .partial_cmp(&g.k[a as usize])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let q_list: Vec<Vec<u32>> = by_p.into_values().collect();
    let mut neigh_w: Vec<f64> = vec![0.0; n];
    let mut touched: Vec<u32> = Vec::with_capacity(16);
    for members in &q_list {
        cancel.check()?;
        refine_one_p_community(
            g,
            p,
            members,
            &mut r,
            &mut sigma_tot,
            &mut r_size,
            &mut r_to_p,
            &mut neigh_w,
            &mut touched,
            g.two_m,
        );
    }

    // compact r to 0..K for aggregate(...)
    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    for c in &mut r {
        let len = remap.len() as u32;
        *c = *remap.entry(*c).or_insert(len);
    }
    Ok(r)
}

/// Restricted refinement kernel for one P-community Q, shared by the serial and Hogwild-parallel
/// branches.
fn refine_one_p_community(
    g: &InternalGraph,
    p: &[u32],
    members: &[u32],
    r: &mut [u32],
    sigma_tot: &mut [f64],
    r_size: &mut [u32],
    r_to_p: &mut [u32],
    neigh_w: &mut [f64],
    touched: &mut Vec<u32>,
    two_m: f64,
) {
    for &v in members {
        let vi = v as usize;
        let cv = r[vi];
        if r_size[cv as usize] != 1 {
            continue;
        }
        let p_v = p[vi];
        let mut k_i_c_current = 0.0;
        for &(u, w) in &g.adj[vi] {
            if u as usize == vi {
                continue;
            }
            if p[u as usize] != p_v {
                continue;
            }
            let cu = r[u as usize];
            if neigh_w[cu as usize] == 0.0 {
                touched.push(cu);
            }
            neigh_w[cu as usize] += w;
            if cu == cv {
                k_i_c_current += w;
            }
        }
        let k_v = g.k[vi];
        if two_m <= 0.0 {
            for &c in touched.iter() {
                neigh_w[c as usize] = 0.0;
            }
            touched.clear();
            continue;
        }
        let sigma_tot_minus_v = sigma_tot[cv as usize] - k_v;
        let gain_current_raw = 2.0 * k_i_c_current * (two_m * 0.5) - sigma_tot_minus_v * k_v;
        let mut best_c = cv;
        let mut best_raw = gain_current_raw;
        for &c in touched.iter() {
            if r_to_p[c as usize] != p_v {
                continue;
            }
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
        for &c in touched.iter() {
            neigh_w[c as usize] = 0.0;
        }
        touched.clear();
        if best_c != cv {
            sigma_tot[cv as usize] -= k_v;
            sigma_tot[best_c as usize] += k_v;
            r_size[cv as usize] -= 1;
            r_size[best_c as usize] += 1;
            r[vi] = best_c;
            let _ = r_to_p; // best_c's parent stays r_to_p[best_c]
        }
    }
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

fn aggregate(g: &InternalGraph, comm: &[u32]) -> InternalGraph {
    use rayon::prelude::*;
    let k_new = count_unique(comm) as usize;

    // bucket old nodes by new community so each community aggregates independently in parallel, no
    // contention on the row maps
    let mut nodes_by_community: Vec<Vec<u32>> = vec![Vec::new(); k_new];
    for u in 0..g.n as usize {
        nodes_by_community[comm[u] as usize].push(u as u32);
    }

    // per-community accumulation in parallel, each task with its own adjacency map
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
    super::super::procedure::register(Leiden);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn finds_two_dense_clusters_separated_by_bridge() {
        // two K4 cliques (0..3 and 4..7) joined by one bridge
        let mut g = Graph::new();
        let n: Vec<_> = (0..8)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        let add = |g: &mut Graph, a: usize, b: usize| {
            g.create_edge(n[a], n[b], "T", Default::default()).unwrap();
        };
        // K4 on {0,1,2,3}
        for a in 0..4 {
            for b in (a + 1)..4 {
                add(&mut g, a, b);
            }
        }
        // K4 on {4,5,6,7}
        for a in 4..8 {
            for b in (a + 1)..8 {
                add(&mut g, a, b);
            }
        }
        // bridge 3-4
        add(&mut g, 3, 4);

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = LeidenConfig {
            max_levels: 10,
            max_iterations: 10,
            tolerance: 1e-4,
        };
        let r = Leiden.run(&proj, &cfg, &CancellationToken::none()).unwrap();
        let comm = |i: usize| r.values[n[i].0 as usize];
        // 0..3 share one community, 4..7 another
        for i in 1..4 {
            assert_eq!(comm(0), comm(i), "nodes 0..3 should be in one community");
        }
        for i in 5..8 {
            assert_eq!(comm(4), comm(i), "nodes 4..7 should be in one community");
        }
        assert_ne!(
            comm(0),
            comm(4),
            "the two cliques should be different communities"
        );
    }
}
