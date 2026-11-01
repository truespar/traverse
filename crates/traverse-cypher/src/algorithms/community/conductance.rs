//! Conductance, a community-quality metric.
//!
//! # Definition
//!
//! For a community C subset of V:
//!
//! ```text
//!   phi(C) = cut(C, V\C) / min( vol(C), vol(V\C) )
//! ```
//!
//! `cut(C, V\C)` is the weight of edges leaving C, `vol(C) = sum_{v in C} deg(v)`
//! its volume. phi in [0, 1], smaller = more cohesive. For a balanced partition
//! the denominator is `vol(C)`.
//!
//! The canonical objective of spectral clustering, and the quantity bounded by
//! the Cheeger inequality.
//!
//! # Output
//!
//! StatsResult: `communityCount`, `minConductance`, `maxConductance`,
//! `meanConductance`.

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

/// Conductance of each community in an existing assignment, a cut-based quality metric
/// (`traverse.conductance.*`). Stateless.
pub struct Conductance;

/// Parsed `conductance` config.
pub struct ConductanceConfig {
    /// Node property holding each node's community id (`communityProperty`, required).
    pub community_property: String,
}

impl GraphAlgorithm for Conductance {
    type Config = ConductanceConfig;
    type Output = StatsResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "conductance",
            display_name: "Conductance",
            category: Category::Community,
            output_kind: OutputKind::StatsOnly,
            stability: Stability::Beta,
            description: "Conductance phi(C) = cut(C) / min(vol(C), vol(V\\C)) per community, returning \
                 aggregate stats (min, max, mean) across all communities. Smaller is better.",
            params: vec![ConfigParam {
                key: "communityProperty",
                ty: ConfigParamType::String,
                default: CypherValue::Null,
                description: "Required. Node property name holding the community id.",
                advanced: false,
                required: true,
            }],
            output_columns: &[],
            stats_columns: &[
                "communityCount",
                "minConductance",
                "maxConductance",
                "meanConductance",
                "computeMillis",
            ],
            write_columns: &[],
        }
    }

    fn parse_config(
        &self,
        params: &BTreeMap<String, CypherValue>,
    ) -> Result<ConductanceConfig, AlgorithmError> {
        let v = params
            .get("communityProperty")
            .ok_or_else(|| AlgorithmError::Config("communityProperty is required".into()))?;
        let community_property =
            parse_string("communityProperty", v).map_err(AlgorithmError::from)?;
        Ok(ConductanceConfig { community_property })
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        cfg: &ConductanceConfig,
        cancel: &CancellationToken,
    ) -> Result<StatsResult, AlgorithmError> {
        let active = graph.active_nodes();
        if active.is_empty() {
            let mut stats: BTreeMap<String, CypherValue> = BTreeMap::new();
            stats.insert("communityCount".into(), CypherValue::Integer(0));
            stats.insert("minConductance".into(), CypherValue::Float(0.0));
            stats.insert("maxConductance".into(), CypherValue::Float(0.0));
            stats.insert("meanConductance".into(), CypherValue::Float(0.0));
            return Ok(StatsResult { stats });
        }

        let view = graph.graph();
        let mut comm_idx: FxHashMap<NodeId, u32> = FxHashMap::default();
        let mut comm_map: FxHashMap<String, u32> = FxHashMap::default();
        let mut next: u32 = 0;
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
                let i = next;
                next += 1;
                i
            });
            comm_idx.insert(n, idx);
        }
        let n_comm = next as usize;
        if n_comm == 0 {
            let mut stats: BTreeMap<String, CypherValue> = BTreeMap::new();
            stats.insert("communityCount".into(), CypherValue::Integer(0));
            stats.insert("minConductance".into(), CypherValue::Float(0.0));
            stats.insert("maxConductance".into(), CypherValue::Float(0.0));
            stats.insert("meanConductance".into(), CypherValue::Float(0.0));
            return Ok(StatsResult { stats });
        }
        cancel.check()?;

        // per-community volume (incident edge weight), total volume and cut (edges leaving c);
        // parallel fold-reduce with thread-local (volume, cut, total_vol), summed element-wise
        use rayon::prelude::*;
        let is_unit = graph.is_unit_weighted();
        let comm_idx_ref = &comm_idx;
        let (volume, cut, total_vol): (Vec<f64>, Vec<f64>, f64) = active
            .par_iter()
            .fold(
                || (vec![0.0_f64; n_comm], vec![0.0_f64; n_comm], 0.0_f64),
                |(mut volume, mut cut, mut total), &u| {
                    let cu = comm_idx_ref[&u];
                    let mut visit = |v: NodeId, w: f64| {
                        let cv = match comm_idx_ref.get(&v) {
                            Some(&c) => c,
                            None => return,
                        };
                        let half = w * 0.5;
                        volume[cu as usize] += half;
                        volume[cv as usize] += half;
                        total += half;
                        if cu != cv {
                            cut[cu as usize] += half;
                            cut[cv as usize] += half;
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
                    (volume, cut, total)
                },
            )
            .reduce(
                || (vec![0.0_f64; n_comm], vec![0.0_f64; n_comm], 0.0_f64),
                |(mut v1, mut c1, t1), (v2, c2, t2)| {
                    for i in 0..n_comm {
                        v1[i] += v2[i];
                        c1[i] += c2[i];
                    }
                    (v1, c1, t1 + t2)
                },
            );

        // conductance per community, then aggregate
        let mut min_c = f64::INFINITY;
        let mut max_c = 0.0;
        let mut sum_c = 0.0;
        let mut active_communities = 0usize;
        for c in 0..n_comm {
            let vol_c = volume[c];
            if vol_c == 0.0 {
                continue;
            }
            let vol_complement = total_vol - vol_c;
            let denom = vol_c.min(vol_complement);
            if denom <= 0.0 {
                continue;
            }
            let phi = cut[c] / denom;
            if phi < min_c {
                min_c = phi;
            }
            if phi > max_c {
                max_c = phi;
            }
            sum_c += phi;
            active_communities += 1;
        }
        let mean_c = if active_communities > 0 {
            sum_c / active_communities as f64
        } else {
            0.0
        };
        if !min_c.is_finite() {
            min_c = 0.0;
        }

        let mut stats: BTreeMap<String, CypherValue> = BTreeMap::new();
        stats.insert("communityCount".into(), CypherValue::Integer(n_comm as i64));
        stats.insert("minConductance".into(), CypherValue::Float(min_c));
        stats.insert("maxConductance".into(), CypherValue::Float(max_c));
        stats.insert("meanConductance".into(), CypherValue::Float(mean_c));
        Ok(StatsResult { stats })
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Conductance);
}
