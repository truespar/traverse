//! Property-vector k-nearest neighbors (`gds.knn` semantics).
//!
//! # Algorithm
//!
//! For every active node, the top-K most similar nodes by Euclidean similarity
//! over a numeric property vector: similarity from node features (e.g. age),
//! not graph structure, as in GDS's `gds.knn`.
//!
//! Brute force is O(n^2*d). GDS uses NN-Descent (Dong, Charikar & Li 2011),
//! exploring candidates via "your neighbor's neighbors are probably your
//! neighbors"; this is a simplified randomized variant:
//!
//! 1. start each node with K random candidates;
//! 2. for up to `maxIterations`:
//!    - score each node's K candidates AND their K candidates (neighbor-of-neighbor probe);
//!    - keep each node's best K;
//! 3. stop when no candidate changes.
//!
//! Typically >90% recall of exact k-NN within 3-5 iterations on smoothly
//! distributed properties, at O(n*K^2*d) per iteration instead of O(n^2*d).
//!
//! # Why separate from `knn`
//!
//! `traverse.knn` (Jaccard over neighbor sets) and `traverse.knnProperty`
//! (Euclidean over property vectors) answer different questions. Feature-based
//! recommendation, demographic user similarity and embedding lookups want the
//! property variant; GDS has used the `gds.knn` name for both over time, so they
//! stay separate here.

use std::collections::BTreeMap;

use rayon::prelude::*;
use rustc_hash::FxHashSet;
use traverse_core::graph::property::PropertyValue;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::{opt_float, opt_integer, parse_string};
use super::super::projection::AlgorithmGraph;
use super::super::result::{NodePairResult, NodePairScore};
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// k-nearest neighbours by Euclidean similarity `1 / (1 + distance)` over numeric node properties,
/// via NN-Descent sampling (`traverse.knnProperty.*`). Stateless.
pub struct KnnProperty;

/// Parsed `knnProperty` configuration.
pub struct KnnPropertyConfig {
    /// Numeric properties forming each node's feature vector (`nodeProperties`, required; one name
    /// or a list).
    pub node_properties: Vec<String>,
    /// Neighbours per source node (`topK`, default 10).
    pub top_k: i64,
    /// Max NN-Descent rounds (`maxIterations`, default 5); stops earlier when nothing changes.
    pub max_iterations: i64,
    /// Pairs below this score are dropped (`similarityCutoff`, default 0).
    pub similarity_cutoff: f64,
    /// Seed for the initial random neighbour sample (`randomSeed`, default 42).
    pub random_seed: u64,
}

impl GraphAlgorithm for KnnProperty {
    type Config = KnnPropertyConfig;
    type Output = NodePairResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "knnProperty",
            display_name: "k-Nearest Neighbors (by node properties)",
            category: Category::Similarity,
            output_kind: OutputKind::NodePair,
            stability: Stability::Beta,
            description: "Top-K most similar nodes by Euclidean similarity over numeric node properties. \
                 Uses NN-Descent-style random sampling - O(n*K^2*d) per iteration vs O(n^2*d) \
                 brute force. Matches the semantics of Neo4j GDS gds.knn (feature-space \
                 similarity, NOT structural similarity - for that use traverse.knn).",
            params: vec![
                ConfigParam {
                    key: "nodeProperties",
                    ty: ConfigParamType::StringList,
                    default: CypherValue::Null,
                    description: "Required. List of node property names to use as the feature vector. \
                         Properties must be numeric on every active node.",
                    advanced: false,
                    required: true,
                },
                ConfigParam {
                    key: "topK",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(10),
                    description: "Neighbors returned per source node.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "maxIterations",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(5),
                    description: "Max NN-Descent rounds. Convergence is detected automatically when no \
                         candidates change.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "similarityCutoff",
                    ty: ConfigParamType::Float,
                    default: CypherValue::Float(0.0),
                    description: "Drop pairs scoring below this threshold.",
                    advanced: false,
                    required: false,
                },
                ConfigParam {
                    key: "randomSeed",
                    ty: ConfigParamType::Integer,
                    default: CypherValue::Integer(42),
                    description: "Seed for the initial random sample.",
                    advanced: true,
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
    ) -> Result<KnnPropertyConfig, AlgorithmError> {
        let node_properties = match params.get("nodeProperties") {
            Some(CypherValue::List(l)) => l
                .iter()
                .filter_map(|v| match v {
                    CypherValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Some(CypherValue::String(s)) => vec![s.clone()],
            Some(v) => parse_string("nodeProperties", v)
                .map(|s| vec![s])
                .map_err(AlgorithmError::from)?,
            None => {
                return Err(AlgorithmError::Config(
                    "nodeProperties is required (list of property names)".into(),
                ));
            }
        };
        if node_properties.is_empty() {
            return Err(AlgorithmError::Config(
                "nodeProperties must list at least one property".into(),
            ));
        }
        let top_k = opt_integer(params, "topK", 10).map_err(AlgorithmError::from)?;
        let max_iterations =
            opt_integer(params, "maxIterations", 5).map_err(AlgorithmError::from)?;
        let similarity_cutoff =
            opt_float(params, "similarityCutoff", 0.0).map_err(AlgorithmError::from)?;
        let random_seed =
            opt_integer(params, "randomSeed", 42).map_err(AlgorithmError::from)? as u64;
        Ok(KnnPropertyConfig {
            node_properties,
            top_k,
            max_iterations,
            similarity_cutoff,
            random_seed,
        })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &KnnPropertyConfig,
        cancel: &CancellationToken,
    ) -> Result<NodePairResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            return Ok(NodePairResult { pairs: Vec::new() });
        }
        cancel.check()?;

        // feature vectors through the AlgorithmGraph property bridge (live view OR materialized
        // overlays, which makes fastRP.mutate -> knn chains work). Scalars add one dimension, LIST
        // properties (embeddings) all their numeric elements; missing / non-numeric values are 0.0,
        // Neo4j's neutral default
        let n = active.len();
        // dimension per property, from the first node that has it
        let dims: Vec<usize> = cfg
            .node_properties
            .iter()
            .map(|key| {
                active
                    .iter()
                    .find_map(|&v| graph.node_property(v, key))
                    .map(|pv| match pv {
                        PropertyValue::List(items) => items.len().max(1),
                        _ => 1,
                    })
                    .unwrap_or(1)
            })
            .collect();
        let d: usize = dims.iter().sum();
        let mut feats: Vec<f32> = vec![0.0; n * d];
        let scalar = |pv: &PropertyValue| -> f32 {
            match pv {
                PropertyValue::Float(f) => *f as f32,
                PropertyValue::Int(i) => *i as f32,
                PropertyValue::Bool(b) if *b => 1.0,
                _ => 0.0,
            }
        };
        for (i, &v) in active.iter().enumerate() {
            let mut at = i * d;
            for (j, key) in cfg.node_properties.iter().enumerate() {
                match graph.node_property(v, key) {
                    Some(PropertyValue::List(items)) => {
                        for (k, item) in items.iter().take(dims[j]).enumerate() {
                            feats[at + k] = scalar(item);
                        }
                    }
                    Some(pv) => feats[at] = scalar(&pv),
                    None => {}
                }
                at += dims[j];
            }
        }
        cancel.check()?;

        let k = (cfg.top_k as usize).max(1).min(n.saturating_sub(1));
        let max_iter = cfg.max_iterations.max(0) as usize;
        let cutoff = cfg.similarity_cutoff as f32;

        // per-node top-K of (similarity, neighbor index in active), flat: `topk[i * k .. (i+1) *
        // k]` = node i's best K, similarity descending
        let mut topk: Vec<(f32, u32)> = vec![(-1.0, u32::MAX); n * k];

        // 1) K random candidates per node, in parallel (each writes its own row)
        let seed = cfg.random_seed.max(1);
        let feats_ref: &[f32] = &feats;
        let init_rows: Vec<Vec<(f32, u32)>> = (0..n)
            .into_par_iter()
            .map(|i| {
                let mut rng = XorShift64Star::new(seed.wrapping_add(i as u64));
                let mut seen: FxHashSet<u32> = FxHashSet::default();
                seen.insert(i as u32);
                let mut row: Vec<(f32, u32)> = Vec::with_capacity(k);
                while row.len() < k {
                    let j = (rng.next_u64() as usize) % n;
                    if !seen.insert(j as u32) {
                        continue;
                    }
                    let sim = euclidean_similarity(feats_ref, i, j, d);
                    if sim < cutoff {
                        row.push((-1.0, u32::MAX));
                        continue;
                    }
                    row.push((sim, j as u32));
                }
                row.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                row
            })
            .collect();
        for (i, row) in init_rows.into_iter().enumerate() {
            topk[i * k..(i + 1) * k].copy_from_slice(&row);
        }
        cancel.check()?;

        // 2) NN-Descent iterations, parallel over nodes: workers read the `prev` snapshot (built
        //    once per iteration) and produce updated rows, applied at the iteration's end
        for _ in 0..max_iter {
            cancel.check()?;
            let prev = topk.clone();
            let prev_ref: &[(f32, u32)] = &prev;
            // an iteration's updates in parallel: None (unchanged) or Some(new_row) per node
            let updates: Vec<Option<Vec<(f32, u32)>>> = (0..n)
                .into_par_iter()
                .map(|i| {
                    let mut cand: FxHashSet<u32> = FxHashSet::default();
                    for slot in 0..k {
                        let nbr = prev_ref[i * k + slot].1;
                        if nbr == u32::MAX {
                            continue;
                        }
                        for inner_slot in 0..k {
                            let nn = prev_ref[nbr as usize * k + inner_slot].1;
                            if nn == u32::MAX || nn == i as u32 {
                                continue;
                            }
                            cand.insert(nn);
                        }
                    }
                    // local copy of v's row to mutate
                    let row_start = i * k;
                    let mut row: Vec<(f32, u32)> = prev_ref[row_start..row_start + k].to_vec();
                    let mut local_changed = false;
                    for j in cand {
                        let ji = j as usize;
                        let sim = euclidean_similarity(feats_ref, i, ji, d);
                        if sim < cutoff {
                            continue;
                        }
                        let worst_slot = k - 1;
                        if sim > row[worst_slot].0 {
                            if row.iter().any(|(_, idx)| *idx == j) {
                                continue;
                            }
                            row[worst_slot] = (sim, j);
                            // bubble the new entry up: the row was sorted, so insertion-sort the
                            // last slot backwards
                            for slot in (1..k).rev() {
                                if row[slot].0 > row[slot - 1].0 {
                                    row.swap(slot, slot - 1);
                                } else {
                                    break;
                                }
                            }
                            local_changed = true;
                        }
                    }
                    if local_changed { Some(row) } else { None }
                })
                .collect();
            let mut changed = false;
            for (i, opt_row) in updates.into_iter().enumerate() {
                if let Some(new_row) = opt_row {
                    topk[i * k..(i + 1) * k].copy_from_slice(&new_row);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        cancel.check()?;

        // 3) emit NodePair scores
        let mut pairs: Vec<NodePairScore> = Vec::with_capacity(n * k);
        for (i, &v) in active.iter().enumerate() {
            for slot in 0..k {
                let (sim, j) = topk[i * k + slot];
                if j == u32::MAX || sim.partial_cmp(&cutoff).is_none_or(|o| o.is_lt()) {
                    continue;
                }
                let target = active[j as usize];
                pairs.push(NodePairScore {
                    node1: v,
                    node2: target,
                    similarity: sim as f64,
                });
            }
        }

        Ok(NodePairResult { pairs })
    }
}

/// Euclidean similarity = 1 / (1 + L2 distance), in (0, 1].
#[inline]
fn euclidean_similarity(feats: &[f32], i: usize, j: usize, d: usize) -> f32 {
    let mut sumsq: f32 = 0.0;
    let ri = i * d;
    let rj = j * d;
    for k in 0..d {
        let diff = feats[ri + k] - feats[rj + k];
        sumsq += diff * diff;
    }
    1.0 / (1.0 + sumsq.sqrt())
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

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(KnnProperty);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use std::collections::BTreeMap;
    use traverse_core::graph::Graph;
    use traverse_core::graph::property::PropertyValue;
    use traverse_core::tx::view::GraphView;

    fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn close_property_values_yield_high_similarity() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], props(&[("v", PropertyValue::Float(1.0))]));
        let b = g.create_node(&["N"], props(&[("v", PropertyValue::Float(1.1))]));
        let _c = g.create_node(&["N"], props(&[("v", PropertyValue::Float(100.0))]));

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let cfg = KnnPropertyConfig {
            node_properties: vec!["v".into()],
            top_k: 1,
            max_iterations: 5,
            similarity_cutoff: 0.0,
            random_seed: 42,
        };
        let r = KnnProperty
            .run(&proj, &cfg, &CancellationToken::none())
            .unwrap();
        // a's best neighbor is b (closer in v-space than c=100)
        let a_pair = r.pairs.iter().find(|p| p.node1 == a).unwrap();
        assert_eq!(a_pair.node2, b);
    }
}
