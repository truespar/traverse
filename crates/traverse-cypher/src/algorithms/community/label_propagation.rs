//! Label Propagation (Raghavan et al. 2007), parallel.
//!
//! # Algorithm
//!
//! Each node starts labelled with its own NodeId; every iteration it adopts the
//! most frequent label among its neighbors (edge-weighted), ties to the
//! smallest. Stops when labels settle or at `maxIterations`.
//!
//! # Synchronous vs asynchronous
//!
//! Synchronous (everyone reads the previous round) parallelizes fully but
//! converges poorly: about an order of magnitude more communities than
//! Raghavan's asynchronous algorithm (and Neo4j GDS) on real social graphs.
//! Asynchronous (reading labels written earlier in the same round) converges
//! properly but is inherently single-threaded: racing neighbors bring the sync
//! behaviour back.
//!
//! This uses **graph-colored parallel async**: color the graph once (adjacent
//! nodes differ), then per iteration process color classes in order, each class
//! in parallel. Same-color nodes share no edges, so reads see the latest labels
//! without races: asynchronous semantics on all cores.
//!
//! # Notes
//!
//! - **Greedy coloring** in degree-descending order stays close to the chromatic
//!   number; real social graphs need a few dozen colors, plenty of
//!   parallelism per class.
//! - **Slice fast path** without projection filters: tallies read the raw
//!   adjacency slice, no dyn-FnMut per edge.
//! - **Weighted votes** on the slice path treat unit weights as an implicit 1.0
//!   (`WeightConfig::Unit`).
//! - **Ties go to the smallest label**, so results are reproducible.

use std::collections::BTreeMap;

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::opt_integer;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeCommunityResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

const DEFAULT_MAX_ITERATIONS: i64 = 10;

/// Community detection by label propagation (Raghavan et al.), parallel
/// (`traverse.labelPropagation.*`). Stateless.
pub struct LabelPropagation;

/// Parsed `labelPropagation` configuration.
pub struct LabelPropagationConfig {
    /// Max propagation rounds (`maxIterations`, default 10); stops earlier when no label changes.
    pub max_iterations: i64,
}

impl GraphAlgorithm for LabelPropagation {
    type Config = LabelPropagationConfig;
    type Output = NodeCommunityResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "labelPropagation",
            display_name: "Label Propagation",
            category: Category::Community,
            output_kind: OutputKind::NodeCommunity,
            stability: Stability::Beta,
            description: "Synchronous label propagation: each node adopts the most-voted label \
                 among its neighbors, ties broken by smallest label.",
            params: vec![ConfigParam {
                key: "maxIterations",
                ty: ConfigParamType::Integer,
                default: CypherValue::Integer(DEFAULT_MAX_ITERATIONS),
                description: "Cap on propagation rounds.",
                advanced: false,
                required: false,
            }],
            output_columns: OutputKind::NodeCommunity.default_columns().unwrap(),
            stats_columns: OutputKind::NodeCommunity.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeCommunity.default_write_columns().unwrap(),
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<LabelPropagationConfig, AlgorithmError> {
        Ok(LabelPropagationConfig {
            max_iterations: opt_integer(params, "maxIterations", DEFAULT_MAX_ITERATIONS)
                .map_err(AlgorithmError::from)?,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LabelPropagationConfig,
        cancel: &CancellationToken,
    ) -> Result<NodeCommunityResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        if nodes.is_empty() {
            return Ok(NodeCommunityResult::new("communityId", vec![u64::MAX; cap]));
        }
        cancel.check()?;

        // initial labels: each node owns itself
        let mut labels: Vec<u64> = vec![u64::MAX; cap];
        for &n in &nodes {
            labels[n.0 as usize] = n.0;
        }
        cancel.check()?;

        // greedy degree-descending coloring once; same-color nodes share no edges, so a class
        // updates in parallel with async semantics intact
        let color_classes = greedy_color(graph, &nodes, cap);
        cancel.check()?;

        let unit_weight = graph.is_unit_weighted();
        for _ in 0..cfg.max_iterations {
            cancel.check()?;
            let changed = std::sync::atomic::AtomicBool::new(false);
            for class in &color_classes {
                // update a color class in parallel: workers read `labels` (latest from earlier
                // classes) and write their own slot via raw pointer; each node is in exactly one
                // class, so tasks in a class never share an index
                let labels_ptr = labels.as_mut_ptr() as usize;
                let labels_ref: &[u64] = &labels;
                class.par_iter().for_each(|&v| {
                    let new_label = compute_label(graph, v, labels_ref, unit_weight);
                    let vi = v.0 as usize;
                    if let Some(l) = new_label
                        && l != labels_ref[vi]
                    {
                        // SAFETY: vi is unique within `class` (coloring guarantee); no other
                        // task this iteration touches labels[vi].
                        unsafe {
                            *(labels_ptr as *mut u64).add(vi) = l;
                        }
                        changed.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
            if !changed.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }

        Ok(NodeCommunityResult::new("communityId", labels))
    }
}

/// v's best label from weighted neighbor labels; `None` if no neighbor has a label (v keeps its
/// own). Slice fast path when available.
fn compute_label(
    graph: &dyn AlgorithmGraph,
    v: traverse_core::graph::types::NodeId,
    labels: &[u64],
    unit_weight: bool,
) -> Option<u64> {
    let mut tally: FxHashMap<u64, f64> = FxHashMap::default();
    if unit_weight {
        if let (Some(out), Some(inc)) = (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
            for ep in out.iter().chain(inc.iter()) {
                let label = labels[ep.neighbor().0 as usize];
                if label != u64::MAX {
                    *tally.entry(label).or_insert(0.0) += 1.0;
                }
            }
        } else {
            graph.for_each_in(v, &mut |u, _eid, _w| {
                let label = labels[u.0 as usize];
                if label != u64::MAX {
                    *tally.entry(label).or_insert(0.0) += 1.0;
                }
            });
            // an undirected projection lists both directions as out already
            if !graph.is_undirected() {
                graph.for_each_out(v, &mut |u, _eid, _w| {
                    let label = labels[u.0 as usize];
                    if label != u64::MAX {
                        *tally.entry(label).or_insert(0.0) += 1.0;
                    }
                });
            }
        }
    } else {
        graph.for_each_in(v, &mut |u, _eid, w| {
            let label = labels[u.0 as usize];
            if label != u64::MAX {
                *tally.entry(label).or_insert(0.0) += w;
            }
        });
        // an undirected projection lists both directions as out already
        if !graph.is_undirected() {
            graph.for_each_out(v, &mut |u, _eid, w| {
                let label = labels[u.0 as usize];
                if label != u64::MAX {
                    *tally.entry(label).or_insert(0.0) += w;
                }
            });
        }
    }
    if tally.is_empty() {
        return None;
    }
    let mut best_label = u64::MAX;
    let mut best_w = f64::NEG_INFINITY;
    for (&l, &w) in &tally {
        if w > best_w || (w == best_w && l < best_label) {
            best_w = w;
            best_label = l;
        }
    }
    Some(best_label)
}

/// Greedy degree-descending coloring as `Vec<Vec<NodeId>>` color classes. Slice fast path when
/// available, else `for_each_out`/`for_each_in`. The color count depends on the graph (lower bound:
/// max clique size).
fn greedy_color(
    graph: &dyn AlgorithmGraph,
    nodes: &[traverse_core::graph::types::NodeId],
    cap: usize,
) -> Vec<Vec<traverse_core::graph::types::NodeId>> {
    // total degree descending: hubs have the most constraints, color them first
    let mut ordered: Vec<traverse_core::graph::types::NodeId> = nodes.to_vec();
    ordered.sort_by(|a, b| {
        let degree = |n| {
            // an undirected projection's out-degree counts both directions already
            graph.out_degree(n)
                + if graph.is_undirected() {
                    0
                } else {
                    graph.in_degree(n)
                }
        };
        let (da, db) = (degree(*a), degree(*b));
        db.cmp(&da)
    });
    let mut color_of: Vec<u32> = vec![u32::MAX; cap];
    // per-node scratch of colors taken by v's neighbors, reset via a touched list
    let mut forbidden: Vec<u32> = vec![u32::MAX; 64];
    let mut touched: Vec<u32> = Vec::with_capacity(64);
    let mut iter_round: u32 = 0;
    let mut max_color: u32 = 0;
    for v in ordered {
        iter_round += 1;
        // in and out neighbors; slice path when plain
        match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
            (Some(out), Some(inc)) => {
                for ep in out.iter().chain(inc.iter()) {
                    let c = color_of[ep.neighbor().0 as usize];
                    if c == u32::MAX {
                        continue;
                    }
                    if (c as usize) >= forbidden.len() {
                        forbidden.resize(c as usize + 1, u32::MAX);
                    }
                    if forbidden[c as usize] != iter_round {
                        forbidden[c as usize] = iter_round;
                        touched.push(c);
                    }
                }
            }
            _ => {
                graph.for_each_out(v, &mut |u, _, _| {
                    let c = color_of[u.0 as usize];
                    if c == u32::MAX {
                        return;
                    }
                    if (c as usize) >= forbidden.len() {
                        forbidden.resize(c as usize + 1, u32::MAX);
                    }
                    if forbidden[c as usize] != iter_round {
                        forbidden[c as usize] = iter_round;
                        touched.push(c);
                    }
                });
                // an undirected projection lists both directions as out already
                if !graph.is_undirected() {
                    graph.for_each_in(v, &mut |u, _, _| {
                        let c = color_of[u.0 as usize];
                        if c == u32::MAX {
                            return;
                        }
                        if (c as usize) >= forbidden.len() {
                            forbidden.resize(c as usize + 1, u32::MAX);
                        }
                        if forbidden[c as usize] != iter_round {
                            forbidden[c as usize] = iter_round;
                            touched.push(c);
                        }
                    });
                }
            }
        }
        // smallest free color
        let mut c: u32 = 0;
        loop {
            if (c as usize) >= forbidden.len() {
                forbidden.resize(c as usize + 1, u32::MAX);
            }
            if forbidden[c as usize] != iter_round {
                break;
            }
            c += 1;
        }
        color_of[v.0 as usize] = c;
        if c > max_color {
            max_color = c;
        }
        touched.clear();
    }
    // bucket nodes by color
    let mut classes: Vec<Vec<traverse_core::graph::types::NodeId>> =
        vec![Vec::new(); (max_color + 1) as usize];
    for &n in nodes {
        let c = color_of[n.0 as usize];
        if c != u32::MAX {
            classes[c as usize].push(n);
        }
    }
    classes
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(LabelPropagation);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn dense_cluster_collapses_to_one_label() {
        // triangle: every pair linked both ways
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        for &(u, v) in &[(a, b), (b, a), (b, c), (c, b), (a, c), (c, a)] {
            g.create_edge(u, v, "T", Default::default()).unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = LabelPropagation
            .run(
                &proj,
                &LabelPropagationConfig { max_iterations: 20 },
                &CancellationToken::none(),
            )
            .unwrap();
        assert_eq!(r.values[a.0 as usize], r.values[b.0 as usize]);
        assert_eq!(r.values[b.0 as usize], r.values[c.0 as usize]);
    }
}
