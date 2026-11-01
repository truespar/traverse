//! Graph projection for algorithms.
//!
//! Algorithms read through an [`AlgorithmGraph`]; the production impl
//! [`AlgorithmGraphView`] wraps the [`GraphView`] with:
//!
//! * an [`AdjacencySnapshot`]: zero-copy when the view can share its adjacency,
//!   else a CSR built once from the view's snapshot reads (exact under concurrent
//!   writes),
//! * a `NodeMask` for label/property subgraph filters,
//! * an `Option<FxHashSet<EdgeTypeId>>` relationship-type filter,
//! * an [`Orientation`] (Natural / Reverse / Undirected),
//! * a [`WeightConfig`] (Unit / Property).
//!
//! Hot path: `for_each_out` / `for_each_in` over the packed `&[AdjEntry]` slice;
//! unweighted per-edge cost is one bit test (node mask) + an optional hash lookup
//! (type filter), no allocation. Weights resolve through an enum, so the
//! unweighted case (constant 1.0) is an inline literal with no branch in the loop.

use rustc_hash::FxHashSet;

use traverse_core::graph::adjacency::{AdjEntry, AdjacencySnapshot};
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeTypeId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use super::AlgorithmError;
use super::config::{BaseConfig, Orientation};
use super::node_mask::NodeMask;

// -- AlgorithmGraph trait ------------------------------------------------

/// Read-only graph interface for algorithm hot loops. `for_each_out` takes `&mut dyn FnMut` so
/// signatures can use `&dyn AlgorithmGraph`; that's one indirect call per node (not per edge), the
/// edge loop stays inlined in the impl.
pub trait AlgorithmGraph: Send + Sync {
    /// Nodes in the **projection** (after the label filter).
    fn node_count(&self) -> usize;

    /// Edges in the **projection** (after the type filter); computed lazily.
    fn edge_count(&self) -> usize;

    /// One past the highest `NodeId.0` ever assigned; sizes dense per-node arrays (`vec![0.0;
    /// cap]`).
    fn highest_node_id(&self) -> u64;

    /// `node` is in the projection?
    fn is_active(&self, node: NodeId) -> bool;

    /// Out-degree in the projection (type filter, orientation-aware).
    fn out_degree(&self, node: NodeId) -> u32;

    /// In-degree in the projection (type filter, orientation-aware).
    fn in_degree(&self, node: NodeId) -> u32;

    /// Calls `f(neighbor, edge_id, weight)` per outbound neighbor of `node`, honoring node mask,
    /// type filter and orientation.
    fn for_each_out(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64));

    /// Inbound version of `for_each_out`.
    fn for_each_in(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64));

    /// **Unfiltered** outgoing adjacency of `node` when the projection is plain (no node mask, no
    /// type filter, natural orientation); `None` otherwise, and callers must use
    /// `for_each_out`/`for_each_out_raw`. For hot loops (random walks) reading neighbors as a slice
    /// with no callback.
    fn outgoing_adj_slice(&self, _node: NodeId) -> Option<&[AdjEntry]> {
        None
    }

    /// `outgoing_adj_slice` for incoming edges; `None` under any filter.
    fn incoming_adj_slice(&self, _node: NodeId) -> Option<&[AdjEntry]> {
        None
    }

    /// Every projected edge weighs 1.0 (`WeightConfig::Unit`)? Lets algorithms take cheaper unit
    /// paths (BFS SSSP instead of delta-stepping, BFS spanning tree instead of Kruskal). Default
    /// `false`; concrete projections override.
    fn is_unit_weighted(&self) -> bool {
        false
    }

    /// Orientation is `Undirected`: both directions of each edge show in `for_each_out` /
    /// `outgoing_adj_slice`. Algorithms visiting each undirected edge once (WCC) filter `v_idx <
    /// neighbor`. Default `false`; concrete projections override.
    fn is_undirected(&self) -> bool {
        false
    }

    /// All active nodes in `0..highest_node_id()` order, materialized once; algorithms zip it with
    /// their per-node state.
    fn active_nodes(&self) -> Vec<NodeId>;

    /// Underlying view, for the property reads/writes some algorithms need (node properties as
    /// input).
    fn graph(&self) -> &dyn GraphView;

    /// Node property as algorithm INPUT. Default reads the view; materialized projections serve
    /// mutated overlays (`gds.<algo>.mutate` results), which is what makes mutate chains
    /// (fastRP.mutate -> knn) work.
    fn node_property(
        &self,
        node: NodeId,
        key: &str,
    ) -> Option<traverse_core::graph::property::PropertyValue> {
        self.graph().get_node_property(node, key).ok().flatten()
    }
}

// -- Weight config --------------------------------------------------------

/// How edge weights resolve. `Unit` (default) is a constant `1.0` with no branch in the hot loop;
/// `Property` reads a pre-resolved `PropertyKeyId` from the edge.
#[derive(Debug, Clone, Default)]
pub enum WeightConfig {
    /// Every edge weighs 1.0.
    #[default]
    Unit,
    /// Per-edge weight from an edge property (interned id).
    Property(PropertyKeyId),
}

// -- AlgorithmGraphView (production impl) ---------------------------------

/// Projection of a `GraphView`, built by [`AlgorithmGraphView::new`]:
/// 1. takes the view's adjacency snapshot (or builds one);
/// 2. resolves label/edge-type/property names to interned ids;
/// 3. builds the `NodeMask` with one `nodes_by_label` per label (the label index
///    already exists), or an existence mask when node ids have gaps;
/// 4. caches the type filter as an `FxHashSet<EdgeTypeId>`.
pub struct AlgorithmGraphView<'g> {
    /// Always `Sync` (algorithms parallelize freely): `Graph` or `ReadTx`. `WriteTx` callers pass
    /// `as_sync_ref()`.
    view: &'g (dyn GraphView + Sync),
    /// Packed adjacency, fixed for the projection's life.
    adj: AdjacencySnapshot<'g>,
    /// Label filter: prunes nodes and the edges touching them.
    node_mask: Option<NodeMask>,
    /// Without a label filter, the ids that are live nodes when ids have gaps (deleted nodes,
    /// ids reserved by uncommitted transactions); prunes nothing else, no edge touches a gap.
    existing: Option<NodeMask>,
    edge_type_filter: Option<FxHashSet<EdgeTypeId>>,
    orientation: Orientation,
    weight: WeightConfig,
    highest_node_id: u64,
    /// Post-projection edge count, memoized lazily.
    cached_edge_count: std::sync::OnceLock<usize>,
}

impl<'g> AlgorithmGraphView<'g> {
    /// Builds a projection from a base config + view. `Err` if a label/type/property name doesn't
    /// resolve.
    pub fn new(
        view: &'g (dyn GraphView + Sync),
        base: &BaseConfig,
    ) -> Result<Self, AlgorithmError> {
        let adj = AdjacencySnapshot::of(view)?;
        let highest = adj.highest_node_id();

        let node_mask = if let Some(labels) = &base.node_labels {
            let mut mask = NodeMask::with_capacity(highest as usize);
            for label in labels {
                // reject unknown labels; `nodes_by_label` would silently return nothing
                view.strings()
                    .get(label)
                    .ok_or_else(|| AlgorithmError::UnknownLabel(label.clone()))?;
                for nid in view.nodes_by_label(label) {
                    mask.set(nid.0 as usize);
                }
            }
            Some(mask)
        } else {
            None
        };

        let edge_type_filter = if let Some(types) = &base.relationship_types {
            let strings = view.strings();
            let mut set: FxHashSet<EdgeTypeId> = FxHashSet::default();
            for t in types {
                let tid = strings
                    .get(t)
                    .ok_or_else(|| AlgorithmError::UnknownEdgeType(t.clone()))?;
                set.insert(EdgeTypeId(tid));
            }
            Some(set)
        } else {
            None
        };

        let weight = if let Some(prop) = &base.relationship_weight_property {
            let strings = view.strings();
            let pid = strings
                .get(prop)
                .ok_or_else(|| AlgorithmError::UnknownProperty(prop.clone()))?;
            WeightConfig::Property(PropertyKeyId(pid))
        } else {
            WeightConfig::Unit
        };

        let existing = if node_mask.is_none() && view.node_count() as u64 != highest {
            let mut mask = NodeMask::with_capacity(highest as usize);
            for nid in view.nodes_vec() {
                mask.set(nid.0 as usize);
            }
            Some(mask)
        } else {
            None
        };

        Ok(Self {
            view,
            adj,
            node_mask,
            existing,
            edge_type_filter,
            orientation: base.orientation,
            weight,
            highest_node_id: highest,
            cached_edge_count: std::sync::OnceLock::new(),
        })
    }

    /// Fully open projection (no filters, natural orientation, unit weights), for tests and simple
    /// algorithms.
    pub fn open(view: &'g (dyn GraphView + Sync)) -> Result<Self, AlgorithmError> {
        Self::new(view, &BaseConfig::default())
    }

    // -- Internal iteration helpers ---------------------------------------

    /// Weight of one edge. `Unit` compiles to a literal; the property path runs only with a
    /// configured weight key.
    #[inline(always)]
    fn weight_of(&self, edge_id: u64) -> f64 {
        match &self.weight {
            WeightConfig::Unit => 1.0,
            WeightConfig::Property(pid) => {
                match self
                    .view
                    .get_edge(traverse_core::graph::types::EdgeId(edge_id))
                {
                    Ok(edge) => match edge.properties.get(*pid) {
                        Some(PropertyValue::Float(f)) => f,
                        Some(PropertyValue::Int(i)) => i as f64,
                        _ => 0.0,
                    },
                    Err(_) => 0.0,
                }
            }
        }
    }

    /// `entry` passes the type filter?
    #[inline(always)]
    fn type_ok(&self, entry: &AdjEntry) -> bool {
        match &self.edge_type_filter {
            None => true,
            Some(set) => set.contains(&entry.edge_type()),
        }
    }

    /// `node` passes the node mask?
    #[inline(always)]
    fn node_ok(&self, node: NodeId) -> bool {
        match &self.node_mask {
            None => true,
            Some(m) => m.contains_node(node),
        }
    }

    /// Raw outbound entries, filters applied; backs `for_each_out` and `out_degree`.
    fn for_each_out_raw(&self, node: NodeId, f: &mut dyn FnMut(&AdjEntry)) {
        let entries = self.adj.outgoing(node);
        for ep in entries {
            if !self.type_ok(ep) {
                continue;
            }
            if !self.node_ok(ep.neighbor()) {
                continue;
            }
            f(ep);
        }
    }

    fn for_each_in_raw(&self, node: NodeId, f: &mut dyn FnMut(&AdjEntry)) {
        let entries = self.adj.incoming(node);
        for ep in entries {
            if !self.type_ok(ep) {
                continue;
            }
            if !self.node_ok(ep.neighbor()) {
                continue;
            }
            f(ep);
        }
    }
}

impl AlgorithmGraphView<'_> {
    /// No filter that would prune the packed adjacency, so `outgoing_adj_slice` can return the raw
    /// slice.
    #[inline]
    fn projection_is_plain(&self) -> bool {
        self.node_mask.is_none()
            && self.edge_type_filter.is_none()
            && self.orientation == Orientation::Natural
    }
}

impl AlgorithmGraph for AlgorithmGraphView<'_> {
    fn outgoing_adj_slice(&self, node: NodeId) -> Option<&[AdjEntry]> {
        if self.projection_is_plain() {
            Some(self.adj.outgoing(node))
        } else {
            None
        }
    }

    fn incoming_adj_slice(&self, node: NodeId) -> Option<&[AdjEntry]> {
        if self.projection_is_plain() {
            Some(self.adj.incoming(node))
        } else {
            None
        }
    }

    fn is_unit_weighted(&self) -> bool {
        matches!(self.weight, WeightConfig::Unit)
    }

    fn is_undirected(&self) -> bool {
        matches!(self.orientation, Orientation::Undirected)
    }

    fn node_count(&self) -> usize {
        match (&self.node_mask, &self.existing) {
            (Some(m), _) | (None, Some(m)) => m.count_ones(),
            (None, None) => self.highest_node_id as usize,
        }
    }

    fn edge_count(&self) -> usize {
        *self.cached_edge_count.get_or_init(|| {
            // sum of active out-degrees, halved for undirected projections so each edge counts once
            let mut sum: usize = 0;
            for n in 0..self.highest_node_id {
                let node = NodeId(n);
                if !self.node_ok(node) {
                    continue;
                }
                let mut k: usize = 0;
                self.for_each_out_raw(node, &mut |_| k += 1);
                sum += k;
            }
            if matches!(self.orientation, Orientation::Undirected) {
                sum / 2
            } else {
                sum
            }
        })
    }

    fn highest_node_id(&self) -> u64 {
        self.highest_node_id
    }

    #[inline]
    fn is_active(&self, node: NodeId) -> bool {
        match (&self.node_mask, &self.existing) {
            (Some(m), _) | (None, Some(m)) => m.contains_node(node),
            (None, None) => node.0 < self.highest_node_id,
        }
    }

    fn out_degree(&self, node: NodeId) -> u32 {
        match self.orientation {
            Orientation::Natural => {
                let mut k: u32 = 0;
                self.for_each_out_raw(node, &mut |_| k += 1);
                k
            }
            Orientation::Reverse => {
                let mut k: u32 = 0;
                self.for_each_in_raw(node, &mut |_| k += 1);
                k
            }
            Orientation::Undirected => {
                let mut k: u32 = 0;
                self.for_each_out_raw(node, &mut |_| k += 1);
                self.for_each_in_raw(node, &mut |ep| {
                    // skip self-loops (no double count)
                    if ep.neighbor() != node {
                        k += 1;
                    }
                });
                k
            }
        }
    }

    fn in_degree(&self, node: NodeId) -> u32 {
        match self.orientation {
            Orientation::Natural => {
                let mut k: u32 = 0;
                self.for_each_in_raw(node, &mut |_| k += 1);
                k
            }
            Orientation::Reverse => {
                let mut k: u32 = 0;
                self.for_each_out_raw(node, &mut |_| k += 1);
                k
            }
            Orientation::Undirected => self.out_degree(node),
        }
    }

    fn for_each_out(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64)) {
        match self.orientation {
            Orientation::Natural => {
                self.for_each_out_raw(node, &mut |ep| {
                    let eid = ep.edge_id().0;
                    f(ep.neighbor(), eid, self.weight_of(eid))
                });
            }
            Orientation::Reverse => {
                self.for_each_in_raw(node, &mut |ep| {
                    let eid = ep.edge_id().0;
                    f(ep.neighbor(), eid, self.weight_of(eid))
                });
            }
            Orientation::Undirected => {
                self.for_each_out_raw(node, &mut |ep| {
                    let eid = ep.edge_id().0;
                    f(ep.neighbor(), eid, self.weight_of(eid))
                });
                self.for_each_in_raw(node, &mut |ep| {
                    // self-loops were covered by the out pass
                    if ep.neighbor() != node {
                        let eid = ep.edge_id().0;
                        f(ep.neighbor(), eid, self.weight_of(eid));
                    }
                });
            }
        }
    }

    fn for_each_in(&self, node: NodeId, f: &mut dyn FnMut(NodeId, u64, f64)) {
        match self.orientation {
            Orientation::Natural => {
                self.for_each_in_raw(node, &mut |ep| {
                    let eid = ep.edge_id().0;
                    f(ep.neighbor(), eid, self.weight_of(eid))
                });
            }
            Orientation::Reverse => {
                self.for_each_out_raw(node, &mut |ep| {
                    let eid = ep.edge_id().0;
                    f(ep.neighbor(), eid, self.weight_of(eid))
                });
            }
            Orientation::Undirected => self.for_each_out(node, f),
        }
    }

    fn active_nodes(&self) -> Vec<NodeId> {
        match (&self.node_mask, &self.existing) {
            (Some(m), _) | (None, Some(m)) => m.iter_ones().map(|i| NodeId(i as u64)).collect(),
            (None, None) => (0..self.highest_node_id).map(NodeId).collect(),
        }
    }

    fn graph(&self) -> &dyn GraphView {
        self.view
    }
}
