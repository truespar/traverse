//! Graph statistics for cost-based optimization: collected by
//! `collect_statistics()`, stored on `Graph::stats`, used by the Cypher optimizer
//! for cardinality estimates and join ordering.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

use super::Graph;
use super::property::PropertyValue;
use super::types::Direction;

/// Whole-graph statistics from `collect_statistics()`.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct GraphStatistics {
    /// Total number of nodes.
    pub total_nodes: u64,
    /// Total number of edges.
    pub total_edges: u64,
    /// Per-label stats by label name.
    pub label_stats: HashMap<String, LabelStats>,
    /// Per-edge-type stats by type name.
    pub edge_type_stats: HashMap<String, EdgeTypeStats>,
    /// Per-property histograms, keyed `"Label.property"`.
    pub property_stats: HashMap<String, PropertyStats>,
    /// Bumped on each collection.
    pub generation: u64,
}

/// Stats of one label.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct LabelStats {
    /// Nodes with the label.
    pub count: u64,
    /// Average degree of nodes with the label.
    pub avg_degree: f64,
}

/// Stats of one edge type.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct EdgeTypeStats {
    /// Edges of the type.
    pub count: u64,
    /// Distinct source nodes.
    pub distinct_sources: u64,
    /// Distinct target nodes.
    pub distinct_targets: u64,
}

/// Stats of one property on nodes of a label.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct PropertyStats {
    /// Non-null values.
    pub non_null_count: u64,
    /// Distinct values.
    pub distinct_count: u64,
    /// Equi-depth histogram, if computed.
    pub histogram: Option<Histogram>,
    /// Minimum (sortable bound).
    pub min_value: Option<HistogramBound>,
    /// Maximum (sortable bound).
    pub max_value: Option<HistogramBound>,
}

/// Equi-depth histogram for cardinality estimates.
#[derive(Debug, Clone, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct Histogram {
    /// Bucket upper bounds.
    pub bounds: Vec<HistogramBound>,
    /// Approximate rows per bucket.
    pub rows_per_bucket: f64,
    /// Distinct values across the histogram.
    pub num_distinct: u64,
}

/// Histogram bucket bound.
#[derive(Debug, Clone, PartialEq, Archive, RkyvSerialize, RkyvDeserialize)]
pub enum HistogramBound {
    /// Boolean bound.
    Bool(bool),
    /// Integer bound.
    Int(i64),
    /// Float bound.
    Float(i64, u32),
    /// String bound.
    String(String),
}

impl HistogramBound {
    /// Bound from a scalar property value.
    pub fn from_property_value(pv: &PropertyValue) -> Option<Self> {
        match pv {
            PropertyValue::Bool(b) => Some(Self::Bool(*b)),
            PropertyValue::Int(i) => Some(Self::Int(*i)),
            PropertyValue::Float(f) => {
                // integer bits for deterministic ordering
                let bits = f.to_bits();
                Some(Self::Float(bits as i64, 0))
            }
            PropertyValue::String(s) => Some(Self::String(s.clone())),
            _ => None,
        }
    }
}

impl PartialOrd for HistogramBound {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a.partial_cmp(b),
            (Self::Int(a), Self::Int(b)) => a.partial_cmp(b),
            (Self::Float(a, _), Self::Float(b, _)) => {
                let fa = f64::from_bits(*a as u64);
                let fb = f64::from_bits(*b as u64);
                fa.partial_cmp(&fb)
            }
            (Self::String(a), Self::String(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

/// Collects optimizer statistics. Non-empty `labels` limits collection to those
/// (partial analyze); `existing` stats get merged.
pub fn collect_statistics(
    graph: &Graph,
    labels: &[String],
    existing: Option<&GraphStatistics>,
) -> GraphStatistics {
    let generation = existing.map_or(1, |s| s.generation + 1);
    let total_nodes = graph.node_count() as u64;
    let total_edges = graph.edge_count() as u64;

    // start from existing stats or empty
    let mut label_stats = existing.map(|s| s.label_stats.clone()).unwrap_or_default();
    let mut edge_type_stats = existing
        .map(|s| s.edge_type_stats.clone())
        .unwrap_or_default();
    let mut property_stats = existing
        .map(|s| s.property_stats.clone())
        .unwrap_or_default();

    // labels to analyze
    let all_label_names = graph.label_names();
    let target_labels: Vec<&String> = if labels.is_empty() {
        all_label_names.iter().collect()
    } else {
        labels
            .iter()
            .filter(|l| all_label_names.contains(l))
            .collect()
    };

    for label_name in &target_labels {
        let node_ids = graph.nodes_by_label(label_name);
        let count = node_ids.len() as u64;
        let mut total_degree: u64 = 0;
        for &nid in &node_ids {
            total_degree += graph.degree(nid, Direction::Both).unwrap_or(0) as u64;
        }
        let avg_degree = if count > 0 {
            total_degree as f64 / count as f64
        } else {
            0.0
        };
        label_stats.insert((*label_name).clone(), LabelStats { count, avg_degree });

        // property stats for this label
        let mut prop_values: FxHashMap<String, Vec<PropertyValue>> = FxHashMap::default();
        for &nid in &node_ids {
            if let Ok(node) = graph.get_node(nid) {
                for (key_id, value) in node.properties.iter() {
                    let key = graph.strings.resolve(key_id.0).to_string();
                    prop_values.entry(key).or_default().push(value);
                }
            }
        }

        for (prop_key, values) in &prop_values {
            let stats_key = format!("{}.{}", label_name, prop_key);
            let non_null_count = values
                .iter()
                .filter(|v| !matches!(v, PropertyValue::Null))
                .count() as u64;

            let mut distinct_values: FxHashSet<String> = FxHashSet::default();
            let mut bounds: Vec<HistogramBound> = Vec::new();
            for v in values {
                if !matches!(v, PropertyValue::Null) {
                    distinct_values.insert(format!("{v:?}"));
                    if let Some(b) = HistogramBound::from_property_value(v) {
                        bounds.push(b);
                    }
                }
            }
            let distinct_count = distinct_values.len() as u64;

            bounds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            let min_value = bounds.first().cloned();
            let max_value = bounds.last().cloned();

            // equi-depth histogram, up to 64 buckets
            let histogram = if bounds.len() >= 2 {
                let num_buckets = bounds.len().min(64);
                let rows_per_bucket = bounds.len() as f64 / num_buckets as f64;
                let mut hist_bounds = Vec::with_capacity(num_buckets);
                for i in 1..=num_buckets {
                    let idx = ((i as f64 * rows_per_bucket) as usize).min(bounds.len()) - 1;
                    hist_bounds.push(bounds[idx].clone());
                }
                Some(Histogram {
                    bounds: hist_bounds,
                    rows_per_bucket,
                    num_distinct: distinct_count,
                })
            } else {
                None
            };

            property_stats.insert(
                stats_key,
                PropertyStats {
                    non_null_count,
                    distinct_count,
                    histogram,
                    min_value,
                    max_value,
                },
            );
        }
    }

    // edge type stats, always (they're cheap)
    if labels.is_empty() {
        edge_type_stats.clear();
        for (type_name, edge_ids) in graph.edge_type_entries() {
            let count = edge_ids.len() as u64;
            let mut sources: FxHashSet<super::types::NodeId> = FxHashSet::default();
            let mut targets: FxHashSet<super::types::NodeId> = FxHashSet::default();
            for eid in edge_ids {
                if let Ok(edge) = graph.get_edge(eid) {
                    sources.insert(edge.source);
                    targets.insert(edge.target);
                }
            }
            edge_type_stats.insert(
                type_name,
                EdgeTypeStats {
                    count,
                    distinct_sources: sources.len() as u64,
                    distinct_targets: targets.len() as u64,
                },
            );
        }
    }

    GraphStatistics {
        total_nodes,
        total_edges,
        label_stats,
        edge_type_stats,
        property_stats,
        generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;
    use std::collections::BTreeMap;

    fn empty_props() -> BTreeMap<String, PropertyValue> {
        BTreeMap::new()
    }

    fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn collect_empty_graph() {
        let g = Graph::new();
        let stats = collect_statistics(&g, &[], None);
        assert_eq!(stats.total_nodes, 0);
        assert_eq!(stats.total_edges, 0);
        assert!(stats.label_stats.is_empty());
        assert!(stats.edge_type_stats.is_empty());
    }

    #[test]
    fn collect_basic_stats() {
        let mut g = Graph::new();
        let a = g.create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        let b = g.create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        );
        g.create_node(&["Movie"], empty_props());
        g.create_edge(a, b, "KNOWS", empty_props()).unwrap();

        let stats = collect_statistics(&g, &[], None);
        assert_eq!(stats.total_nodes, 3);
        assert_eq!(stats.total_edges, 1);
        assert_eq!(stats.label_stats["Person"].count, 2);
        assert_eq!(stats.label_stats["Movie"].count, 1);
        assert_eq!(stats.edge_type_stats["KNOWS"].count, 1);
        assert!(stats.property_stats.contains_key("Person.name"));
    }

    #[test]
    fn partial_analyze() {
        let mut g = Graph::new();
        g.create_node(&["Person"], empty_props());
        g.create_node(&["Movie"], empty_props());

        let stats = collect_statistics(&g, &["Person".to_string()], None);
        assert!(stats.label_stats.contains_key("Person"));
        assert!(!stats.label_stats.contains_key("Movie"));
    }

    #[test]
    fn incremental_analyze_merges() {
        let mut g = Graph::new();
        g.create_node(&["Person"], empty_props());
        g.create_node(&["Movie"], empty_props());

        let stats1 = collect_statistics(&g, &["Person".to_string()], None);
        assert_eq!(stats1.generation, 1);

        let stats2 = collect_statistics(&g, &["Movie".to_string()], Some(&stats1));
        assert_eq!(stats2.generation, 2);
        assert!(stats2.label_stats.contains_key("Person"));
        assert!(stats2.label_stats.contains_key("Movie"));
    }

    #[test]
    fn property_stats_with_histogram() {
        let mut g = Graph::new();
        for i in 0..100 {
            g.create_node(&["Item"], props(&[("value", PropertyValue::Int(i))]));
        }

        let stats = collect_statistics(&g, &[], None);
        let ps = &stats.property_stats["Item.value"];
        assert_eq!(ps.non_null_count, 100);
        assert_eq!(ps.distinct_count, 100);
        assert!(ps.histogram.is_some());
        let hist = ps.histogram.as_ref().unwrap();
        assert!(hist.bounds.len() <= 64);
    }
}
