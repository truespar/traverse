//! Topological link prediction: five classic scores plus Same-Community, all on
//! undirected neighbor sets built once from the projection.
//!
//! # Algorithms
//!
//! For a node pair `(u, v)` with neighbor sets `N(u), N(v)`:
//!
//! | Algorithm              | Score                                         |
//! |------------------------|-----------------------------------------------|
//! | Common Neighbors       | `|N(u) intersect N(v)|`                               |
//! | Total Neighbors        | `|N(u) union N(v)|`                               |
//! | Preferential Attachmt. | `|N(u)| * |N(v)|`                             |
//! | Adamic-Adar            | `sum_{w in N(u) intersect N(v)} 1 / log(|N(w)|)`           |
//! | Resource Allocation    | `sum_{w in N(u) intersect N(v)} 1 / |N(w)|`                |
//! | Same Community         | `1` if `comm(u) == comm(v)` else `0`          |
//!
//! # Notes
//!
//! * All share one two-hop candidate-pair walk: sum_w |N(w)|^2, far below n^2 brute
//!   force.
//! * Adamic-Adar / Resource Allocation walk the common neighbors once per pair
//!   (sorted intersection), accumulating in one pass.
//! * `topK` caps pairs per source; `cutoff` drops weak pairs.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer, parse_string};
use super::super::projection::AlgorithmGraph;
use super::super::result::{NodePairResult, NodePairScore};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

use rayon::prelude::*;
use rustc_hash::FxHashSet;

use super::helpers::{for_each_common_neighbor, intersect_size, neighbor_sets};

const DEFAULT_TOP_K: i64 = 10;
const DEFAULT_CUTOFF: f64 = 1e-42;

/// Base config shared by every topological score.
pub struct LpConfig {
    /// Max pairs kept per source node (`topK`, default 10).
    pub top_k: i64,
    /// Pairs below this are dropped (`cutoff`, default 1e-42: only zero scores go).
    pub cutoff: f64,
}

fn parse_lp_config(params: &BTreeMap<String, CypherValue>) -> Result<LpConfig, AlgorithmError> {
    Ok(LpConfig {
        top_k: opt_integer(params, "topK", DEFAULT_TOP_K).map_err(AlgorithmError::from)?,
        cutoff: opt_float(params, "cutoff", DEFAULT_CUTOFF).map_err(AlgorithmError::from)?,
    })
}

fn lp_params() -> Vec<ConfigParam> {
    vec![
        ConfigParam {
            key: "topK",
            ty: ConfigParamType::Integer,
            default: CypherValue::Integer(DEFAULT_TOP_K),
            description: "Maximum pairs per source.",
            advanced: false,
            required: false,
        },
        ConfigParam {
            key: "cutoff",
            ty: ConfigParamType::Float,
            default: CypherValue::Float(DEFAULT_CUTOFF),
            description: "Minimum score to emit a pair.",
            advanced: false,
            required: false,
        },
    ]
}

fn push_top_k(
    per_source: &mut FxHashMap<u32, Vec<(u32, f64)>>,
    src: u32,
    tgt: u32,
    score: f64,
    k: usize,
) {
    let entry = per_source
        .entry(src)
        .or_insert_with(|| Vec::with_capacity(k + 1));
    if entry.len() < k {
        entry.push((tgt, score));
        entry.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        return;
    }
    if score > entry.last().unwrap().1 {
        entry.pop();
        entry.push((tgt, score));
        entry.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    }
}

fn finalize(per_source: FxHashMap<u32, Vec<(u32, f64)>>) -> NodePairResult {
    let mut pairs: Vec<NodePairScore> = Vec::new();
    for (&u, list) in &per_source {
        for &(v, score) in list {
            if u < v {
                pairs.push(NodePairScore {
                    node1: NodeId(u as u64),
                    node2: NodeId(v as u64),
                    similarity: score,
                });
            }
        }
    }
    pairs.sort_unstable_by(|a, b| b.similarity.total_cmp(&a.similarity));
    NodePairResult { pairs }
}

/// Walks candidate pairs, scoring each with `scorer(u, v, adj)` into the
/// per-source top-K (both directions); `f64::NEG_INFINITY` skips a pair.
/// Parallel over sources (each worker enumerates its 2-hop candidates and
/// scores them); only the bidirectional scatter into per_source is serial.
fn run_pairwise(
    graph: &dyn AlgorithmGraph,
    cfg: &LpConfig,
    cancel: &CancellationToken,
    scorer: impl Fn(NodeId, NodeId, &[Vec<u32>]) -> f64 + Send + Sync,
) -> Result<NodePairResult, AlgorithmError> {
    let active = graph.active_nodes();
    if active.is_empty() {
        return Ok(NodePairResult { pairs: Vec::new() });
    }
    cancel.check()?;
    let adj = neighbor_sets(graph);
    cancel.check()?;
    let top_k = cfg.top_k.max(1) as usize;

    // phase 1: parallel candidate enumeration + scoring
    let adj_ref: &[Vec<u32>] = &adj;
    let cutoff = cfg.cutoff;
    let scored_pairs: Vec<(u32, u32, f64)> = active
        .par_iter()
        .map_init(FxHashSet::<u32>::default, |candidates, &u_node| {
            candidates.clear();
            let u = u_node.0 as u32;
            let u_idx = u_node.0 as usize;
            for &w in &adj_ref[u_idx] {
                for &v in &adj_ref[w as usize] {
                    if v > u {
                        candidates.insert(v);
                    }
                }
            }
            let mut out: Vec<(u32, u32, f64)> = Vec::new();
            for &v in candidates.iter() {
                let score = scorer(u_node, NodeId(v as u64), adj_ref);
                if score.is_finite() && score >= cutoff {
                    out.push((u, v, score));
                }
            }
            out
        })
        .reduce(Vec::new, |mut a, mut b| {
            a.append(&mut b);
            a
        });
    cancel.check()?;

    // phase 2: serial scatter into per-source bidirectional top-K, O(1) amortized per pair (two map
    // inserts, optional partial sort)
    let mut per_source: FxHashMap<u32, Vec<(u32, f64)>> = FxHashMap::default();
    for (u, v, score) in scored_pairs {
        push_top_k(&mut per_source, u, v, score, top_k);
        push_top_k(&mut per_source, v, u, score, top_k);
    }
    Ok(finalize(per_source))
}

// -- Common Neighbors ---------------------------------------------------

/// Link prediction by shared-neighbour count `|N(u) intersect N(v)|` (`traverse.commonNeighbors.*`).
/// Stateless.
pub struct CommonNeighbors;

impl GraphAlgorithm for CommonNeighbors {
    type Config = LpConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "commonNeighbors",
            display_name: "Common Neighbors",
            category: Category::LinkPrediction,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Score = |N(u) intersect N(v)| - number of mutual neighbors.",
            params: lp_params(),
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(&self, p: &BTreeMap<String, CypherValue>) -> Result<LpConfig, AlgorithmError> {
        parse_lp_config(p)
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LpConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        run_pairwise(graph, cfg, cancel, |u, v, adj| {
            intersect_size(&adj[u.0 as usize], &adj[v.0 as usize]) as f64
        })
    }
}

// -- Total Neighbors ----------------------------------------------------

/// Link prediction by combined neighbourhood size `|N(u) union N(v)|` (`traverse.totalNeighbors.*`).
/// Stateless.
pub struct TotalNeighbors;

impl GraphAlgorithm for TotalNeighbors {
    type Config = LpConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "totalNeighbors",
            display_name: "Total Neighbors",
            category: Category::LinkPrediction,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Score = |N(u) union N(v)| - size of the combined neighbor set.",
            params: lp_params(),
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(&self, p: &BTreeMap<String, CypherValue>) -> Result<LpConfig, AlgorithmError> {
        parse_lp_config(p)
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LpConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        run_pairwise(graph, cfg, cancel, |u, v, adj| {
            let nu = adj[u.0 as usize].len() as f64;
            let nv = adj[v.0 as usize].len() as f64;
            let ni = intersect_size(&adj[u.0 as usize], &adj[v.0 as usize]) as f64;
            nu + nv - ni
        })
    }
}

// -- Preferential Attachment -------------------------------------------

/// Link prediction by degree product `|N(u)| * |N(v)|` (`traverse.preferentialAttachment.*`).
/// Stateless.
pub struct PreferentialAttachment;

impl GraphAlgorithm for PreferentialAttachment {
    type Config = LpConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "preferentialAttachment",
            display_name: "Preferential Attachment",
            category: Category::LinkPrediction,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Score = |N(u)| * |N(v)| - high-degree pairs are more likely to link.",
            params: lp_params(),
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(&self, p: &BTreeMap<String, CypherValue>) -> Result<LpConfig, AlgorithmError> {
        parse_lp_config(p)
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LpConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        run_pairwise(graph, cfg, cancel, |u, v, adj| {
            (adj[u.0 as usize].len() * adj[v.0 as usize].len()) as f64
        })
    }
}

// -- Adamic-Adar --------------------------------------------------------

/// Link prediction by Adamic-Adar: shared neighbours weighted `1 / log(degree)`
/// (`traverse.adamicAdar.*`). Stateless.
pub struct AdamicAdar;

impl GraphAlgorithm for AdamicAdar {
    type Config = LpConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "adamicAdar",
            display_name: "Adamic-Adar",
            category: Category::LinkPrediction,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Score = sum_{w in N(u) intersect N(v)} 1 / log(|N(w)|). Discounts contributions from \
                 high-degree common neighbors.",
            params: lp_params(),
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(&self, p: &BTreeMap<String, CypherValue>) -> Result<LpConfig, AlgorithmError> {
        parse_lp_config(p)
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LpConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        run_pairwise(graph, cfg, cancel, |u, v, adj| {
            let mut score = 0.0;
            for_each_common_neighbor(&adj[u.0 as usize], &adj[v.0 as usize], |w| {
                let dw = adj[w as usize].len() as f64;
                if dw > 1.0 {
                    score += 1.0 / dw.ln();
                }
            });
            score
        })
    }
}

// -- Resource Allocation -----------------------------------------------

/// Link prediction by resource allocation: shared neighbours weighted `1 / degree`
/// (`traverse.resourceAllocation.*`). Stateless.
pub struct ResourceAllocation;

impl GraphAlgorithm for ResourceAllocation {
    type Config = LpConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "resourceAllocation",
            display_name: "Resource Allocation",
            category: Category::LinkPrediction,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Score = sum_{w in N(u) intersect N(v)} 1 / |N(w)|. Stronger discount than \
                 Adamic-Adar for high-degree common neighbors.",
            params: lp_params(),
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(&self, p: &BTreeMap<String, CypherValue>) -> Result<LpConfig, AlgorithmError> {
        parse_lp_config(p)
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &LpConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        run_pairwise(graph, cfg, cancel, |u, v, adj| {
            let mut score = 0.0;
            for_each_common_neighbor(&adj[u.0 as usize], &adj[v.0 as usize], |w| {
                let dw = adj[w as usize].len() as f64;
                if dw > 0.0 {
                    score += 1.0 / dw;
                }
            });
            score
        })
    }
}

// -- Same Community ----------------------------------------------------

/// Link prediction scoring 1 for nodes with the same community id, else 0
/// (`traverse.sameCommunity.*`). Stateless.
pub struct SameCommunity;

/// Parsed `sameCommunity` configuration.
pub struct SameCommunityConfig {
    /// Property holding the community id (`communityProperty`, default `communityId`).
    pub community_property: String,
}

impl GraphAlgorithm for SameCommunity {
    type Config = SameCommunityConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "sameCommunity",
            display_name: "Same Community",
            category: Category::LinkPrediction,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Score = 1 if the pair shares a `communityProperty` value, else 0. \
                 Emits one pair per same-community pair (potentially quadratic in the \
                 largest community).",
            params: vec![ConfigParam {
                key: "communityProperty",
                ty: ConfigParamType::String,
                default: CypherValue::String("communityId".into()),
                description: "Node property carrying the community label (integer).",
                advanced: false,
                required: false,
            }],
            output_columns: OutputKind::NodePair.default_columns().unwrap(),
            stats_columns: OutputKind::NodePair.default_stats_columns().unwrap(),
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<SameCommunityConfig, AlgorithmError> {
        let key = params
            .get("communityProperty")
            .map(|v| parse_string("communityProperty", v))
            .transpose()
            .map_err(AlgorithmError::from)?
            .unwrap_or_else(|| "communityId".into());
        Ok(SameCommunityConfig {
            community_property: key,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &SameCommunityConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodePairResult { pairs: Vec::new() });
        }
        cancel.check()?;

        // bucket nodes by community; missing / non-numeric values can't be same-community, skip
        let view = graph.graph();
        let mut buckets: FxHashMap<i64, Vec<NodeId>> = FxHashMap::default();
        for v in &active {
            let val = match view.get_node_property(*v, &cfg.community_property) {
                Ok(Some(PropertyValue::Int(i))) => i,
                Ok(Some(PropertyValue::Float(f))) if f.fract() == 0.0 => f as i64,
                _ => continue,
            };
            buckets.entry(val).or_default().push(*v);
        }
        cancel.check()?;

        // all pairs within each bucket: quadratic for huge communities, so filter with `nodeLabels`
        // on the projection if needed
        let mut pairs: Vec<NodePairScore> = Vec::new();
        for bucket in buckets.values() {
            for i in 0..bucket.len() {
                for j in (i + 1)..bucket.len() {
                    pairs.push(NodePairScore {
                        node1: bucket[i],
                        node2: bucket[j],
                        similarity: 1.0,
                    });
                }
            }
        }
        Ok(NodePairResult { pairs })
    }
}

/// Adds the link-prediction scorers and `sameCommunity` to the current thread's registry.
pub fn register() {
    super::super::procedure::register(CommonNeighbors);
    super::super::procedure::register(TotalNeighbors);
    super::super::procedure::register(PreferentialAttachment);
    super::super::procedure::register(AdamicAdar);
    super::super::procedure::register(ResourceAllocation);
    super::super::procedure::register(SameCommunity);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn common_neighbors_counts_shared() {
        // a - w - b; a - x - b -> CN(a, b) = 2
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let w = g.create_node(&["N"], Default::default());
        let x = g.create_node(&["N"], Default::default());
        let edge = |g: &mut Graph, u, v| {
            g.create_edge(u, v, "T", Default::default()).unwrap();
            g.create_edge(v, u, "T", Default::default()).unwrap();
        };
        edge(&mut g, a, w);
        edge(&mut g, a, x);
        edge(&mut g, b, w);
        edge(&mut g, b, x);
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = CommonNeighbors
            .run(
                &proj,
                &LpConfig {
                    top_k: 5,
                    cutoff: 0.0,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        let ab = r
            .pairs
            .iter()
            .find(|p| (p.node1 == a && p.node2 == b) || (p.node1 == b && p.node2 == a))
            .unwrap();
        assert_eq!(ab.similarity, 2.0);
    }

    #[test]
    fn preferential_attachment_multiplies_degrees() {
        let mut g = Graph::new();
        // a has 3 neighbors, b 2, sharing one (w)
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let w = g.create_node(&["N"], Default::default());
        let x = g.create_node(&["N"], Default::default());
        let y = g.create_node(&["N"], Default::default());
        let z = g.create_node(&["N"], Default::default());
        let edge = |g: &mut Graph, u, v| {
            g.create_edge(u, v, "T", Default::default()).unwrap();
            g.create_edge(v, u, "T", Default::default()).unwrap();
        };
        edge(&mut g, a, w);
        edge(&mut g, a, x);
        edge(&mut g, a, y);
        edge(&mut g, b, w);
        edge(&mut g, b, z);
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = PreferentialAttachment
            .run(
                &proj,
                &LpConfig {
                    top_k: 10,
                    cutoff: 0.0,
                },
                &CancellationToken::none(),
            )
            .unwrap();
        // PA(a, b) = 3 * 2 = 6
        let ab = r
            .pairs
            .iter()
            .find(|p| (p.node1 == a && p.node2 == b) || (p.node1 == b && p.node2 == a))
            .unwrap();
        assert_eq!(ab.similarity, 6.0);
    }
}
