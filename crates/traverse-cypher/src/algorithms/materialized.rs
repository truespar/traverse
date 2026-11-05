//! Materialized GDS projections: a compact DENSE-id CSR snapshot built once by
//! `gds.graph.project` and reused by every algorithm call.
//!
//! Why: with the filter-based `AlgorithmGraphView` every call pays for the whole
//! underlying graph, since engines size per-node arrays by `highest_node_id()`;
//! a large database means megabytes of zeroed arrays per call even for a tiny
//! projection. Here `highest_node_id() == node_count()`, so allocation is
//! O(projection).
//!
//! A materialized projection is a SNAPSHOT: database writes after
//! `gds.graph.project` stay invisible to algorithms until re-projection, as in
//! GDS (its catalog graphs are project-time snapshots too).
//!
//! Node ids inside are dense `0..n`; the `gds.*` adapter translates ids in
//! configs (orig -> dense) and results (dense -> orig). Edge ids stay ORIGINAL
//! (opaque to engines, useful in path output).

use rustc_hash::FxHashMap;
use traverse_core::graph::types::NodeId;
use traverse_core::tx::view::GraphView;

use super::projection::{AlgorithmGraph, DenseAdjacency};
use crate::error::CypherError;

/// Compact CSR snapshot of a projected subgraph.
pub struct MaterializedGraph {
    /// Original node id per dense index.
    pub orig_ids: Vec<u64>,
    /// Original id -> dense index.
    pub orig_to_dense: FxHashMap<u64, u32>,

    out_offsets: Vec<u32>,
    out_targets: Vec<u32>,
    out_edge_ids: Vec<u64>,
    out_weights: Option<Vec<f64>>,

    in_offsets: Vec<u32>,
    in_targets: Vec<u32>,
    in_edge_ids: Vec<u64>,
    in_weights: Option<Vec<f64>>,

    edge_count: usize,
    undirected: bool,
    /// Mutated property overlays (`gds.<algo>.mutate` results): property -> dense-indexed values.
    /// Served to algorithms via [`AlgorithmGraph::node_property`]; streamed / flushed / dropped by
    /// the `gds.graph.nodeProperties.*` procedures.
    pub overlays:
        std::sync::RwLock<std::collections::BTreeMap<String, Vec<crate::types::CypherValue>>>,
    /// Empty placeholder behind the trait's `graph()`; the adapter keeps property-reading
    /// algorithms off the materialized path.
    empty: traverse_core::graph::Graph,
}

impl std::fmt::Debug for MaterializedGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaterializedGraph")
            .field("nodes", &self.orig_ids.len())
            .field("edges", &self.edge_count)
            .field("undirected", &self.undirected)
            .finish()
    }
}

/// Counting-sort CSR over dense-id triples; weights are `Some` only if the caller says some edge
/// had one.
fn build_csr_weighted(
    pairs: &[(u32, u32, u64, f64)],
    n: usize,
    weighted: bool,
) -> (Vec<u32>, Vec<u32>, Vec<u64>, Option<Vec<f64>>) {
    let mut offsets = vec![0u32; n + 1];
    for &(s, _, _, _) in pairs {
        offsets[s as usize + 1] += 1;
    }
    for i in 0..n {
        offsets[i + 1] += offsets[i];
    }
    let total = pairs.len();
    let mut targets = vec![0u32; total];
    let mut edge_ids = vec![0u64; total];
    let mut weights = vec![1.0f64; total];
    let mut cursor = offsets.clone();
    for &(s, d, eid, w) in pairs {
        let at = cursor[s as usize] as usize;
        targets[at] = d;
        edge_ids[at] = eid;
        weights[at] = w;
        cursor[s as usize] += 1;
    }
    (offsets, targets, edge_ids, weighted.then_some(weights))
}

impl MaterializedGraph {
    /// From EXPLICIT rows: the GDS *Cypher projection* (`gds.graph.project.cypher` / anonymous
    /// `{nodeQuery, relationshipQuery}`). `nodes` are original ids, `edges` (source, target,
    /// weight) in original ids. Edge ids are synthesized (no backing edges); weights are baked if
    /// ANY edge has one (missing ones = 1.0).
    pub fn from_rows(nodes: Vec<u64>, edges: &[(u64, u64, Option<f64>)]) -> Result<Self, String> {
        let mut orig_ids = nodes;
        orig_ids.sort_unstable();
        orig_ids.dedup();
        let mut orig_to_dense =
            FxHashMap::with_capacity_and_hasher(orig_ids.len(), Default::default());
        for (i, &id) in orig_ids.iter().enumerate() {
            orig_to_dense.insert(id, i as u32);
        }
        let n = orig_ids.len();
        let any_weight = edges.iter().any(|(_, _, w)| w.is_some());
        let mut triples: Vec<(u32, u32, u64, f64)> = Vec::with_capacity(edges.len());
        for (i, &(s, t, w)) in edges.iter().enumerate() {
            let (Some(&ds), Some(&dt)) = (orig_to_dense.get(&s), orig_to_dense.get(&t)) else {
                return Err(format!(
                    "relationship ({s})->({t}) references a node outside the node query"
                ));
            };
            triples.push((ds, dt, i as u64, w.unwrap_or(1.0)));
        }
        let (out_offsets, out_targets, out_edge_ids, out_w) =
            build_csr_weighted(&triples, n, any_weight);
        let in_triples: Vec<(u32, u32, u64, f64)> =
            triples.iter().map(|&(s, t, e, w)| (t, s, e, w)).collect();
        let (in_offsets, in_targets, in_edge_ids, in_w) =
            build_csr_weighted(&in_triples, n, any_weight);
        Ok(Self {
            orig_ids,
            orig_to_dense,
            out_offsets,
            out_targets,
            out_edge_ids,
            out_weights: out_w,
            in_offsets,
            in_targets,
            in_edge_ids,
            in_weights: in_w,
            edge_count: triples.len(),
            undirected: false,
            overlays: std::sync::RwLock::new(Default::default()),
            empty: traverse_core::graph::Graph::new(),
        })
    }

    /// Approximate heap footprint (`gds.graph.list`).
    pub fn size_in_bytes(&self) -> usize {
        self.orig_ids.len() * 8
            + self.orig_to_dense.len() * 16
            + (self.out_offsets.len() + self.in_offsets.len()) * 4
            + (self.out_targets.len() + self.in_targets.len()) * 4
            + (self.out_edge_ids.len() + self.in_edge_ids.len()) * 8
            + self.out_weights.as_ref().map_or(0, |w| w.len() * 8)
            + self.in_weights.as_ref().map_or(0, |w| w.len() * 8)
    }

    /// Whether the snapshot carries relationship weights (a projected property, or the `weight`
    /// column of a Cypher projection).
    pub fn has_weights(&self) -> bool {
        self.out_weights.is_some()
    }

    /// The natural out- and in-lists as dense CSR arrays.
    fn dense_lists(&self) -> (DenseAdjacency<'_>, DenseAdjacency<'_>) {
        (
            DenseAdjacency {
                offsets: &self.out_offsets,
                targets: &self.out_targets,
            },
            DenseAdjacency {
                offsets: &self.in_offsets,
                targets: &self.in_targets,
            },
        )
    }

    /// `f` per relationship ending at dense node `i`, as stored.
    fn for_each_natural_in(&self, i: usize, f: &mut dyn FnMut(NodeId, u64, f64)) {
        let (a, b) = (self.in_offsets[i] as usize, self.in_offsets[i + 1] as usize);
        for k in a..b {
            let w = self.in_weights.as_ref().map_or(1.0, |ws| ws[k]);
            f(NodeId(self.in_targets[k] as u64), self.in_edge_ids[k], w);
        }
    }

    /// Snapshot of the live graph under the filters. Empty `labels` / `types` = all. `weight_prop`
    /// bakes per-edge weights (missing -> 1.0). `undirected` is GDS's UNDIRECTED orientation (see
    /// the CSR note below).
    pub fn build(
        graph: &dyn GraphView,
        labels: &[String],
        types: &[String],
        weight_prop: Option<&str>,
        undirected: bool,
    ) -> Result<Self, CypherError> {
        // 1. node set, sorted by original id for deterministic dense ids
        let mut nodes: Vec<u64> = if labels.is_empty() {
            graph.nodes_vec().into_iter().map(|n| n.0).collect()
        } else {
            let mut v: Vec<u64> = labels
                .iter()
                .flat_map(|l| graph.nodes_by_label(l))
                .map(|n| n.0)
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        nodes.sort_unstable();
        let orig_to_dense: FxHashMap<u64, u32> = nodes
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i as u32))
            .collect();
        let n = nodes.len();

        // 2. edges with both endpoints projected
        let type_names: Vec<String> = if types.is_empty() {
            graph.relationship_type_names()
        } else {
            types.to_vec()
        };
        // (source_dense, target_dense, edge_id, weight)
        let mut edges: Vec<(u32, u32, u64, f64)> = Vec::new();
        for t in &type_names {
            for eid in graph.edges_by_type(t) {
                let Ok(e) = graph.get_edge(eid) else { continue };
                let (Some(&s), Some(&d)) = (
                    orig_to_dense.get(&e.source.0),
                    orig_to_dense.get(&e.target.0),
                ) else {
                    continue;
                };
                let w = match weight_prop {
                    Some(p) => graph
                        .get_edge_property(eid, p)
                        .ok()
                        .flatten()
                        .and_then(|pv| match pv {
                            traverse_core::graph::property::PropertyValue::Int(i) => Some(i as f64),
                            traverse_core::graph::property::PropertyValue::Float(f) => Some(f),
                            _ => None,
                        })
                        .unwrap_or(1.0),
                    None => 1.0,
                };
                edges.push((s, d, eid.0, w));
            }
        }
        let edge_count = edges.len();

        // 3. CSR (counting sort by source)
        let build_csr = |pairs: &[(u32, u32, u64, f64)],
                         n: usize|
         -> (Vec<u32>, Vec<u32>, Vec<u64>, Vec<f64>) {
            let mut offsets = vec![0u32; n + 1];
            for &(s, _, _, _) in pairs {
                offsets[s as usize + 1] += 1;
            }
            for i in 0..n {
                offsets[i + 1] += offsets[i];
            }
            let total = pairs.len();
            let mut targets = vec![0u32; total];
            let mut edge_ids = vec![0u64; total];
            let mut weights = vec![1.0f64; total];
            let mut cursor = offsets.clone();
            for &(s, d, eid, w) in pairs {
                let at = cursor[s as usize] as usize;
                targets[at] = d;
                edge_ids[at] = eid;
                weights[at] = w;
                cursor[s as usize] += 1;
            }
            (offsets, targets, edge_ids, weights)
        };

        // natural out + natural in, as the live graph presents them; an UNDIRECTED projection
        // reads both as each node's out (and in) neighbors, see `is_undirected`
        let (out_offsets, out_targets, out_edge_ids, out_w) = build_csr(&edges, n);
        let in_pairs: Vec<(u32, u32, u64, f64)> =
            edges.iter().map(|&(s, d, eid, w)| (d, s, eid, w)).collect();
        let (in_offsets, in_targets, in_edge_ids, in_w) = build_csr(&in_pairs, n);

        let weighted = weight_prop.is_some();
        Ok(Self {
            orig_ids: nodes,
            orig_to_dense,
            out_offsets,
            out_targets,
            out_edge_ids,
            out_weights: weighted.then_some(out_w),
            in_offsets,
            in_targets,
            in_edge_ids,
            in_weights: weighted.then_some(in_w),
            edge_count,
            undirected,
            overlays: std::sync::RwLock::new(Default::default()),
            empty: traverse_core::graph::Graph::new(),
        })
    }
}

impl AlgorithmGraph for MaterializedGraph {
    fn node_count(&self) -> usize {
        self.orig_ids.len()
    }

    fn edge_count(&self) -> usize {
        self.edge_count
    }

    fn highest_node_id(&self) -> u64 {
        // dense ids: per-node arrays are sized by the PROJECTION, not the database (the point of
        // materializing)
        self.orig_ids.len() as u64
    }

    fn is_active(&self, node: NodeId) -> bool {
        (node.0 as usize) < self.orig_ids.len()
    }

    fn out_degree(&self, node: NodeId) -> u32 {
        let i = node.0 as usize;
        if i >= self.orig_ids.len() {
            return 0;
        }
        let out = self.out_offsets[i + 1] - self.out_offsets[i];
        if self.undirected {
            out + self.in_offsets[i + 1] - self.in_offsets[i]
        } else {
            out
        }
    }

    fn in_degree(&self, node: NodeId) -> u32 {
        let i = node.0 as usize;
        if i >= self.orig_ids.len() {
            return 0;
        }
        if self.undirected {
            return self.out_degree(node);
        }
        self.in_offsets[i + 1] - self.in_offsets[i]
    }

    fn for_each_out(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64)) {
        let i = node.0 as usize;
        if i >= self.orig_ids.len() {
            return;
        }
        let (a, b) = (
            self.out_offsets[i] as usize,
            self.out_offsets[i + 1] as usize,
        );
        for k in a..b {
            let w = self.out_weights.as_ref().map_or(1.0, |ws| ws[k]);
            f(NodeId(self.out_targets[k] as u64), self.out_edge_ids[k], w);
        }
        if self.undirected {
            self.for_each_natural_in(i, f);
        }
    }

    fn for_each_in(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64)) {
        let i = node.0 as usize;
        if i >= self.orig_ids.len() {
            return;
        }
        if self.undirected {
            return self.for_each_out(node, f);
        }
        self.for_each_natural_in(i, f);
    }

    fn in_adjacency(&self) -> Option<(DenseAdjacency<'_>, Option<DenseAdjacency<'_>>)> {
        let (out, natural_in) = self.dense_lists();
        // as `for_each_in`: undirected, the out-list, then the natural in-list
        Some(if self.undirected {
            (out, Some(natural_in))
        } else {
            (natural_in, None)
        })
    }

    fn out_adjacency(&self) -> Option<(DenseAdjacency<'_>, Option<DenseAdjacency<'_>>)> {
        let (out, natural_in) = self.dense_lists();
        // as `for_each_out`
        Some((out, self.undirected.then_some(natural_in)))
    }

    fn is_unit_weighted(&self) -> bool {
        self.out_weights.is_none()
    }

    /// GDS's UNDIRECTED orientation: each relationship lists at both ends, as an out and an in
    /// neighbor, so a node's neighbors are its natural out- and in-neighbors (a self-loop twice:
    /// GDS's degree counts it 2), and engines that union both directions read out alone.
    fn is_undirected(&self) -> bool {
        self.undirected
    }

    fn active_nodes(&self) -> Vec<NodeId> {
        (0..self.orig_ids.len() as u64).map(NodeId).collect()
    }

    fn graph(&self) -> &dyn GraphView {
        // property-reading algorithms use `node_property` (the overlay bridge); this only satisfies
        // the trait
        &self.empty
    }

    fn node_property(
        &self,
        node: NodeId,
        key: &str,
    ) -> Option<traverse_core::graph::property::PropertyValue> {
        let overlays = self.overlays.read().unwrap_or_else(|e| e.into_inner());
        overlays
            .get(key)
            .and_then(|vals| vals.get(node.0 as usize))
            .and_then(|v| v.to_property())
    }
}

/// A projection read with every relationship weighing 1.0. GDS algorithms run unweighted unless
/// their own configuration names `relationshipWeightProperty`, whatever weights the projection
/// holds.
pub(crate) struct Unweighted<'a>(pub(crate) &'a dyn AlgorithmGraph);

impl AlgorithmGraph for Unweighted<'_> {
    fn node_count(&self) -> usize {
        self.0.node_count()
    }

    fn edge_count(&self) -> usize {
        self.0.edge_count()
    }

    fn highest_node_id(&self) -> u64 {
        self.0.highest_node_id()
    }

    fn is_active(&self, node: NodeId) -> bool {
        self.0.is_active(node)
    }

    fn out_degree(&self, node: NodeId) -> u32 {
        self.0.out_degree(node)
    }

    fn in_degree(&self, node: NodeId) -> u32 {
        self.0.in_degree(node)
    }

    fn for_each_out(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64)) {
        self.0.for_each_out(node, &mut |v, e, _| f(v, e, 1.0));
    }

    fn for_each_in(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64)) {
        self.0.for_each_in(node, &mut |v, e, _| f(v, e, 1.0));
    }

    // no adjacency slices: their entries carry the weights; neighbor ids alone carry none

    fn in_adjacency(&self) -> Option<(DenseAdjacency<'_>, Option<DenseAdjacency<'_>>)> {
        self.0.in_adjacency()
    }

    fn out_adjacency(&self) -> Option<(DenseAdjacency<'_>, Option<DenseAdjacency<'_>>)> {
        self.0.out_adjacency()
    }

    fn is_unit_weighted(&self) -> bool {
        true
    }

    fn is_undirected(&self) -> bool {
        self.0.is_undirected()
    }

    fn active_nodes(&self) -> Vec<NodeId> {
        self.0.active_nodes()
    }

    fn graph(&self) -> &dyn GraphView {
        self.0.graph()
    }

    fn node_property(
        &self,
        node: NodeId,
        key: &str,
    ) -> Option<traverse_core::graph::property::PropertyValue> {
        self.0.node_property(node, key)
    }
}
