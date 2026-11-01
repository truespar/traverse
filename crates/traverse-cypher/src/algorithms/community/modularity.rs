//! Modularity of a community assignment.
//!
//! # Definition
//!
//! Undirected weighted graph, weight matrix A, assignment c, total weight
//! m = (1/2) sum A_ij:
//!
//! ```text
//!   Q = (1 / 2m) * sum_{i,j} (A_ij - k_i * k_j / 2m) * delta(c_i, c_j)
//! ```
//!
//! Q in [-0.5, 1]; above ~0.3 usually means real community structure. This is
//! the Newman-Girvan modularity Louvain and Leiden optimize.
//!
//! # Inputs
//!
//! - `communityProperty` (required): node property holding the community id
//!   (any scalar). Nodes without it are their own singleton community.
//! - Weights come from the projection's `relationshipWeightProperty`
//!   (unweighted = 1).
//!
//! # Output
//!
//! StatsResult with one field, `modularity`; `.stats` is the natural mode.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::config::parse_string;
use super::super::projection::AlgorithmGraph;
use super::super::result::StatsResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, ConfigParam, ConfigParamType, GraphAlgorithm,
    OutputKind, Stability,
};

/// Modularity of an existing community assignment (`traverse.modularity.*`). Stateless.
pub struct Modularity;

/// Parsed `modularity` config.
pub struct ModularityConfig {
    /// Node property holding each node's community id (`communityProperty`, required).
    pub community_property: String,
}

impl GraphAlgorithm for Modularity {
    type Config = ModularityConfig;
    type Output = StatsResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "modularity",
            display_name: "Modularity Score",
            category: Category::Community,
            output_kind: OutputKind::StatsOnly,
            stability: Stability::Beta,
            description: "Newman-Girvan modularity Q of a given community assignment. \
                 Reads each node's community id from the `communityProperty` property; \
                 returns a single scalar in [-0.5, 1].",
            params: vec![ConfigParam {
                key: "communityProperty",
                ty: ConfigParamType::String,
                default: CypherValue::Null,
                description: "Required. Node property name holding the community id.",
                advanced: false,
                required: true,
            }],
            output_columns: &[],
            stats_columns: &["modularity", "communityCount", "computeMillis"],
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<ModularityConfig, AlgorithmError> {
        let v = params
            .get("communityProperty")
            .ok_or_else(|| AlgorithmError::Config("communityProperty is required".into()))?;
        let community_property =
            parse_string("communityProperty", v).map_err(AlgorithmError::from)?;
        Ok(ModularityConfig { community_property })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &ModularityConfig,
        cancel: &CancellationToken,
    ) -> Result<StatsResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            let mut stats: BTreeMap<String, CypherValue> = BTreeMap::new();
            stats.insert("modularity".into(), CypherValue::Float(0.0));
            stats.insert("communityCount".into(), CypherValue::Integer(0));
            return Ok(StatsResult { stats });
        }

        // map nodes to contiguous community indices; nodes without the property get their own
        // singleton bucket
        let view = graph.graph();
        let mut comm_idx: FxHashMap<NodeId, u32> = FxHashMap::default();
        let mut comm_map: FxHashMap<String, u32> = FxHashMap::default();
        let mut next_synthetic: u32 = 0;
        for &n in &active {
            let raw = match view.get_node_property(n, &cfg.community_property) {
                Ok(Some(PropertyValue::Int(i))) => Some(format!("i:{i}")),
                Ok(Some(PropertyValue::String(s))) => Some(format!("s:{s}")),
                Ok(Some(PropertyValue::Bool(b))) => Some(format!("b:{b}")),
                Ok(Some(PropertyValue::Float(f))) => Some(format!("f:{}", f.to_bits())),
                _ => None,
            };
            let key = raw.unwrap_or_else(|| format!("__singleton_{}", n.0));
            let idx = *comm_map.entry(key).or_insert_with(|| {
                let i = next_synthetic;
                next_synthetic += 1;
                i
            });
            comm_idx.insert(n, idx);
        }
        let n_comm = next_synthetic as usize;
        cancel.check()?;

        // m, k[v] and sum_in (intra-community weight) via a parallel fold; community indices are
        // dense (0..n_comm) so k and intra are Vec<f64>; per-worker accumulators keep the edge loop
        // sync-free, then an element-wise reduce
        use rayon::prelude::*;
        let is_unit = graph.is_unit_weighted();
        let comm_idx_ref = &comm_idx;
        let (k, intra, total_weight): (Vec<f64>, Vec<f64>, f64) = active
            .par_iter()
            .fold(
                || (vec![0.0_f64; n_comm], vec![0.0_f64; n_comm], 0.0_f64),
                |(mut k, mut intra, mut total), &u| {
                    let cu = comm_idx_ref[&u];
                    let mut visit = |v: NodeId, w: f64| {
                        let cv = match comm_idx_ref.get(&v) {
                            Some(&c) => c,
                            None => return,
                        };
                        let half = w * 0.5;
                        k[cu as usize] += half;
                        k[cv as usize] += half;
                        total += half;
                        if cu == cv {
                            intra[cu as usize] += half;
                        }
                    };
                    match graph.outgoing_adj_slice(u) {
                        Some(slice) if is_unit => {
                            for ep in slice {
                                visit(NodeId(ep.neighbor().0), 1.0);
                            }
                        }
                        _ => {
                            graph.for_each_out(u, &mut |v, _eid, w| visit(v, w));
                        }
                    }
                    (k, intra, total)
                },
            )
            .reduce(
                || (vec![0.0_f64; n_comm], vec![0.0_f64; n_comm], 0.0_f64),
                |(mut k1, mut intra1, t1), (k2, intra2, t2)| {
                    for i in 0..n_comm {
                        k1[i] += k2[i];
                        intra1[i] += intra2[i];
                    }
                    (k1, intra1, t1 + t2)
                },
            );

        let m = total_weight;
        if m <= 0.0 {
            let mut stats: BTreeMap<String, CypherValue> = BTreeMap::new();
            stats.insert("modularity".into(), CypherValue::Float(0.0));
            stats.insert("communityCount".into(), CypherValue::Integer(n_comm as i64));
            return Ok(StatsResult { stats });
        }

        // Q = sum_c [ (intra_c / m) - (k_c / 2m)^2 ]; intra_c is the weight within c (each edge once),
        // hence / m; k_c is the strength sum of c, expected fraction (k_c / 2m)^2
        let two_m = 2.0 * m;
        let mut q: f64 = 0.0;
        for c in 0..n_comm {
            let kc = k[c];
            let in_c = intra[c];
            let frac_in = in_c / m;
            let expected = (kc / two_m).powi(2);
            q += frac_in - expected;
        }

        let mut stats: BTreeMap<String, CypherValue> = BTreeMap::new();
        stats.insert("modularity".into(), CypherValue::Float(q));
        stats.insert("communityCount".into(), CypherValue::Integer(n_comm as i64));
        Ok(StatsResult { stats })
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Modularity);
}
