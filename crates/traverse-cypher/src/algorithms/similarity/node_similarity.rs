//! Node Similarity: Jaccard similarity of neighbor sets.
//!
//! # Definition
//!
//! For nodes `u, v` with undirected neighbor sets `N(u), N(v)`:
//!
//! ```text
//!   J(u, v) = |N(u) intersect N(v)| / |N(u) union N(v)|
//! ```
//!
//! Pairs with `J = 0` (nothing shared) aren't emitted.
//!
//! # Notes
//!
//! * **Two-hop candidates**: only pairs sharing a neighbor, from
//!   `union_{w in N(u)} N(w)` per source `u` (see `helpers.rs`); sum_w |N(w)|^2, the
//!   GDS NodeSimilarity bound, far below n^2 brute force.
//! * **Sorted-list intersection** for the numerator: no HashMap, no per-pair
//!   allocation.
//! * `similarityCutoff` drops weak pairs early; `topK` caps pairs per source
//!   (standard GDS knobs).

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer};
use super::super::projection::AlgorithmGraph;
use super::super::result::{NodePairResult, NodePairScore};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

use rayon::prelude::*;
use rustc_hash::FxHashSet;

use super::helpers::intersect_size;

const DEFAULT_TOP_K: i64 = 10;
const DEFAULT_CUTOFF: f64 = 1e-42;

/// Jaccard similarity of neighbour sets for every pair sharing a neighbour
/// (`traverse.nodeSimilarity.*`). Stateless.
pub struct NodeSimilarity;

/// Parsed `nodeSimilarity` configuration.
pub struct NodeSimilarityConfig {
    /// Max pairs per source node, best first (`topK`, default 10).
    pub top_k: i64,
    /// Pairs below this are dropped (`similarityCutoff`, default 1e-42: only zero scores go).
    pub similarity_cutoff: f64,
    /// Neighbor-set orientation: UNDIRECTED is the traverse.* default; the gds.* adapter passes
    /// NATURAL (GDS projections are directed unless declared otherwise).
    pub orientation: crate::algorithms::config::Orientation,
}

impl GraphAlgorithm for NodeSimilarity {
    type Config = NodeSimilarityConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "nodeSimilarity",
            display_name: "Node Similarity (Jaccard)",
            category: Category::Similarity,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Jaccard similarity on undirected neighbor sets, computed for every \
                 two-hop pair. Filtered by similarityCutoff and topK per source.",
            params: vec![
                ConfigParam {
                    key: "topK",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_TOP_K),
                    description: "Maximum pairs per source node (sorted descending by score).",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "similarityCutoff",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_CUTOFF),
                    description: "Minimum Jaccard score to emit a pair.",
                    advanced: false,
                    required: false,
                },
            ],
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<NodeSimilarityConfig, AlgorithmError> {
        let orientation = match params.get("orientation") {
            Some(CypherValue::String(s)) => crate::algorithms::config::Orientation::parse(s)
                .ok_or_else(|| {
                    AlgorithmError::Config(format!(
                        "orientation must be NATURAL, REVERSE or UNDIRECTED, got `{s}`"
                    ))
                })?,
            _ => crate::algorithms::config::Orientation::Undirected,
        };
        Ok(NodeSimilarityConfig {
            top_k: opt_integer(params, "topK", DEFAULT_TOP_K).map_err(AlgorithmError::from)?,
            similarity_cutoff: opt_float(params, "similarityCutoff", DEFAULT_CUTOFF)
                .map_err(AlgorithmError::from)?,
            orientation,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &NodeSimilarityConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodePairResult { pairs: Vec::new() });
        }
        cancel.check()?;
        let adj = super::helpers::neighbor_sets_oriented(graph, cfg.orientation);
        cancel.check()?;

        let top_k = cfg.top_k.max(1) as usize;
        let cutoff = cfg.similarity_cutoff;

        // per-source candidates + Jaccard in parallel (much faster on dense graphs); scoring is
        // pure, so `adj` is shared immutably
        let adj_ref: &[Vec<u32>] = &adj;
        // candidates SHARE a neighbor (both lists contain some w). Undirected `adj` is its own
        // reverse; directed needs the inverse (u in rev[w] <=> w in adj[u]) or co-in-neighbors are
        // missed
        let rev_owned: Option<Vec<Vec<u32>>> =
            if cfg.orientation == crate::algorithms::config::Orientation::Undirected {
                None
            } else {
                let mut rev: Vec<Vec<u32>> = vec![Vec::new(); adj.len()];
                for (u, list) in adj.iter().enumerate() {
                    for &w in list {
                        rev[w as usize].push(u as u32);
                    }
                }
                Some(rev)
            };
        let rev_ref: &[Vec<u32>] = rev_owned.as_deref().unwrap_or(adj_ref);
        let scored: Vec<(u32, u32, f64)> = active
            .par_iter()
            .map_init(FxHashSet::<u32>::default, |candidates, &u_node| {
                // stopped: skip the remaining nodes; the check below reports it
                if cancel.is_stopped() {
                    return Vec::new();
                }
                candidates.clear();
                let u = u_node.0 as u32;
                let u_idx = u_node.0 as usize;
                for &w in &adj_ref[u_idx] {
                    for &v in &rev_ref[w as usize] {
                        if v > u {
                            candidates.insert(v);
                        }
                    }
                }
                let nu = adj_ref[u_idx].len() as f64;
                let mut out: Vec<(u32, u32, f64)> = Vec::new();
                // ascending-id candidate order: top-K keeps the FIRST on ties, so hash-set
                // order would make tie membership random; GDS keeps the lowest ids
                let mut ordered: Vec<u32> = candidates.iter().copied().collect();
                ordered.sort_unstable();
                for &v in ordered.iter() {
                    let ni = intersect_size(&adj_ref[u_idx], &adj_ref[v as usize]) as f64;
                    if ni == 0.0 {
                        continue;
                    }
                    let nv = adj_ref[v as usize].len() as f64;
                    let union = nu + nv - ni;
                    if union == 0.0 {
                        continue;
                    }
                    let score = ni / union;
                    if score < cutoff {
                        continue;
                    }
                    out.push((u, v, score));
                }
                out
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            });
        cancel.check()?;
        // per-source top-K over the TOTAL order (score DESC, target id ASC): GDS's deterministic
        // tie behaviour, independent of insertion order
        let mut per_source: FxHashMap<u32, Vec<(u32, f64)>> = FxHashMap::default();
        for &(u, v, score) in &scored {
            per_source.entry(u).or_default().push((v, score));
            per_source.entry(v).or_default().push((u, score));
        }
        for list in per_source.values_mut() {
            list.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            list.truncate(top_k);
        }

        cancel.check()?;
        let mut pairs: Vec<NodePairScore> = Vec::new();
        for (&u, list) in &per_source {
            for &(v, score) in list {
                // GDS's topK is per-source and DIRECTIONAL: every source yields its own top-K, so
                // mutual pairs appear as (a,b) and (b,a). Deduplicating with `u < v` would drop
                // pairs that made one endpoint's top-K but not the other's
                pairs.push(NodePairScore {
                    node1: NodeId(u as u64),
                    node2: NodeId(v as u64),
                    similarity: score,
                });
            }
        }
        pairs.sort_unstable_by(|a, b| b.similarity.total_cmp(&a.similarity));

        Ok(NodePairResult { pairs })
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(NodeSimilarity);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn co_neighbored_pair_has_high_jaccard() {
        // a - w - b ; a - x - b ; c apart from {a, b}
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let w = g.create_node(&["N"], Default::default());
        let x = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        let y = g.create_node(&["N"], Default::default());
        let make_edge = |g: &mut Graph, u, v| {
            g.create_edge(u, v, "T", Default::default()).unwrap();
            g.create_edge(v, u, "T", Default::default()).unwrap();
        };
        make_edge(&mut g, a, w);
        make_edge(&mut g, a, x);
        make_edge(&mut g, b, w);
        make_edge(&mut g, b, x);
        make_edge(&mut g, c, y);

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = NodeSimilarity
            .run(
                &proj,
                &NodeSimilarityConfig {
                    top_k: 5,
                    similarity_cutoff: 1e-9,
                    orientation: crate::algorithms::config::Orientation::Undirected,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // a and b share w and x: |N(a) intersect N(b)| = 2, |N(a) union N(b)| = 2 => J = 1
        let ab = r
            .pairs
            .iter()
            .find(|p| (p.node1 == a && p.node2 == b) || (p.node1 == b && p.node2 == a));
        assert!(ab.is_some(), "expected pair (a, b)");
        assert!((ab.unwrap().similarity - 1.0).abs() < 1e-9);
    }
}
