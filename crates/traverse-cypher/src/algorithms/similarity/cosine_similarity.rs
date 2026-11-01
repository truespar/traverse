//! Cosine similarity over neighbor-membership vectors.
//!
//! # Definition
//!
//! Each node's undirected neighbor set as a 0/1 vector over node ids:
//!
//! ```text
//!   cos(u, v) = |N(u) intersect N(v)| / sqrt(|N(u)| * |N(v)|)
//! ```
//!
//! Jaccard with a geometric-mean denominator instead of the union size. Suits
//! recommendation and content similarity, where overlap matters more than
//! coverage.

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

use super::helpers::{intersect_size, neighbor_sets};

const DEFAULT_TOP_K: i64 = 10;
const DEFAULT_CUTOFF: f64 = 1e-42;

/// Cosine similarity between neighbor sets as 0/1 vectors (`traverse.cosineSimilarity.*`).
/// Stateless.
pub struct CosineSimilarity;

/// Parsed `cosineSimilarity` config.
pub struct CosineSimilarityConfig {
    /// Max pairs kept per source node (`topK`, default 10).
    pub top_k: i64,
    /// Pairs below this are dropped (`similarityCutoff`, default 1e-42, so only zeros go).
    pub similarity_cutoff: f64,
}

impl GraphAlgorithm for CosineSimilarity {
    type Config = CosineSimilarityConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "cosineSimilarity",
            display_name: "Cosine Similarity",
            category: Category::Similarity,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Cosine similarity on neighbor-membership vectors. |N(u) intersect N(v)| / \
                 sqrt(|N(u)|*|N(v)|).",
            params: vec![
                ConfigParam {
                    key: "topK",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(DEFAULT_TOP_K),
                    description: "Maximum pairs per source.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "similarityCutoff",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(DEFAULT_CUTOFF),
                    description: "Minimum cosine score to emit a pair.",
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
    ) -> Result<CosineSimilarityConfig, AlgorithmError> {
        Ok(CosineSimilarityConfig {
            top_k: opt_integer(params, "topK", DEFAULT_TOP_K).map_err(AlgorithmError::from)?,
            similarity_cutoff: opt_float(params, "similarityCutoff", DEFAULT_CUTOFF)
                .map_err(AlgorithmError::from)?,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &CosineSimilarityConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodePairResult { pairs: Vec::new() });
        }
        cancel.check()?;
        let adj = neighbor_sets(graph);
        cancel.check()?;
        let top_k = cfg.top_k.max(1) as usize;
        let cutoff = cfg.similarity_cutoff;

        // parallel candidate enumeration + scoring per source node
        let adj_ref: &[Vec<u32>] = &adj;
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
                    for &v in &adj_ref[w as usize] {
                        if v > u {
                            candidates.insert(v);
                        }
                    }
                }
                let nu = adj_ref[u_idx].len() as f64;
                let mut out: Vec<(u32, u32, f64)> = Vec::new();
                for &v in candidates.iter() {
                    let ni = intersect_size(&adj_ref[u_idx], &adj_ref[v as usize]) as f64;
                    if ni == 0.0 {
                        continue;
                    }
                    let nv = adj_ref[v as usize].len() as f64;
                    let denom = (nu * nv).sqrt();
                    if denom == 0.0 {
                        continue;
                    }
                    let score = ni / denom;
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
        let mut per_source: FxHashMap<u32, Vec<(u32, f64)>> = FxHashMap::default();
        for (u, v, score) in scored {
            push_top_k(&mut per_source, u, v, score, top_k);
            push_top_k(&mut per_source, v, u, score, top_k);
        }

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
        Ok(NodePairResult { pairs })
    }
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

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(CosineSimilarity);
}
