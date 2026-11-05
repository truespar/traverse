//! In-memory property graph: [`Graph`] plus node, edge, property and traversal types.
//!
//! [`Graph`]'s methods live by concern: `crud` (create / read / update / delete), `access`
//! (traversal, scans, snapshots), `schema` (metadata, constraints, statistics), `prop_index` and
//! `search_index` (the index families), `parts` (the snapshot form) and `view` (its
//! [`GraphView`](crate::tx::view::GraphView)).

mod access;
pub mod adjacency;
pub mod batch;
mod columns;
pub mod compact_props;
pub mod constraint;
mod crud;
/// Edge struct and constructor.
pub mod edge;
pub mod events;
pub mod fulltext;
pub mod index;
mod load_check;
/// Node struct with label support.
pub mod node;
mod parts;
mod prop_index;
/// Property value types and property map alias.
pub mod property;
mod schema;
mod search_index;
pub mod spatial;
pub mod stats;
pub mod table;
#[cfg(test)]
mod tests;
pub mod text;
/// Core ID types and traversal direction.
pub mod types;
pub mod vector;
mod versioned;
mod view;

use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::interner::{SharedStrings, StringTable};
use crate::tx::mvcc::Delta as MvccDelta;

use self::adjacency::AdjacencyStore;
use self::constraint::ConstraintRegistry;
use self::edge::Edge;
use self::index::{EdgeIndexCatalog, EdgeIndexRegistry, IndexCatalog, IndexRegistry};
use self::node::Node;
use self::stats::GraphStatistics;
use self::table::{IdSet, IdTable};
use self::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};

pub(crate) use parts::COMPOSITE_PROP_SEP;
pub(crate) use parts::CatalogParts;
pub use parts::GraphParts;

// -- Memory cost constants ---------------------------------------------
// Conservative per-entity heap estimates; feed `estimated_memory` and the memory budget.

/// Base cost of a node: HashMap entry + Node struct + label_index + adjacency slot.
pub(crate) const NODE_BASE_COST: usize = 128;
/// Base cost of an edge: HashMap entry + Edge/ProplessEdge + type_index entry.
pub(crate) const EDGE_BASE_COST: usize = 64;
/// Cost of one adjacency entry (outgoing + incoming = 12 bytes x 2).
pub(crate) const ADJ_ENTRY_COST: usize = 24;
/// Cost of one label-index FxHashSet entry per label on a node.
pub(crate) const LABEL_INDEX_COST: usize = 32;
/// Cost per byte of CompactPropertyStore data.
pub(crate) const PROPERTY_BYTE: usize = 1;

/// Topology of a property-less edge: 20 bytes vs ~52 for a full `Edge`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProplessEdge {
    pub edge_type: EdgeTypeId,
    pub source: NodeId,
    pub target: NodeId,
}

/// Edge during load: property-less edges stay compact (28 bytes) instead of a
/// full `Edge` with an empty store (56+). Halves peak `edge_results` memory when
/// edges carry no properties.
pub(crate) enum LoadedEdge {
    /// No properties: topology only, no heap.
    Propless {
        id: EdgeId,
        edge_type: EdgeTypeId,
        source: NodeId,
        target: NodeId,
    },
    /// Has properties, fully deserialized.
    Full(Edge),
}

impl LoadedEdge {
    #[inline]
    pub(crate) fn id(&self) -> EdgeId {
        match self {
            LoadedEdge::Propless { id, .. } => *id,
            LoadedEdge::Full(e) => e.id,
        }
    }

    #[inline]
    pub(crate) fn source(&self) -> NodeId {
        match self {
            LoadedEdge::Propless { source, .. } => *source,
            LoadedEdge::Full(e) => e.source,
        }
    }

    #[inline]
    pub(crate) fn target(&self) -> NodeId {
        match self {
            LoadedEdge::Propless { target, .. } => *target,
            LoadedEdge::Full(e) => e.target,
        }
    }

    #[inline]
    pub(crate) fn edge_type(&self) -> EdgeTypeId {
        match self {
            LoadedEdge::Propless { edge_type, .. } => *edge_type,
            LoadedEdge::Full(e) => e.edge_type,
        }
    }
}

/// Metadata a snapshot reader captures at its start. Each part is copy-on-write in the graph, so
/// the captured `Arc`s never change under the reader while a writer interns strings, runs DDL or
/// re-collects statistics.
#[derive(Debug, Clone)]
pub(crate) struct MetaSnapshot {
    pub(crate) strings: Arc<StringTable>,
    pub(crate) index_catalog: Arc<IndexCatalog>,
    pub(crate) edge_index_catalog: Arc<EdgeIndexCatalog>,
    pub(crate) constraints: Arc<ConstraintRegistry>,
    pub(crate) stats: Option<Arc<GraphStatistics>>,
}

/// Next node and edge ids. Shared with the owning store, which reserves ids for optimistic
/// transactions without the writer lock; ids are never reused.
#[derive(Debug, Default)]
pub(crate) struct IdCounters {
    node: AtomicU64,
    edge: AtomicU64,
}

impl IdCounters {
    fn shared(node: u64, edge: u64) -> Arc<Self> {
        Arc::new(Self {
            node: AtomicU64::new(node),
            edge: AtomicU64::new(edge),
        })
    }

    /// One past the highest node id handed out.
    pub(crate) fn next_node(&self) -> u64 {
        self.node.load(Ordering::Relaxed)
    }

    /// One past the highest edge id handed out.
    pub(crate) fn next_edge(&self) -> u64 {
        self.edge.load(Ordering::Relaxed)
    }

    /// A fresh node id.
    pub(crate) fn reserve_node(&self) -> NodeId {
        NodeId(self.node.fetch_add(1, Ordering::Relaxed))
    }

    /// A fresh edge id.
    pub(crate) fn reserve_edge(&self) -> EdgeId {
        EdgeId(self.edge.fetch_add(1, Ordering::Relaxed))
    }

    /// One past the highest node id handed out.
    pub(crate) fn node_bound(&self) -> u64 {
        self.node.load(Ordering::Relaxed)
    }

    /// `n` fresh consecutive node ids, the first returned.
    pub(crate) fn reserve_nodes(&self, n: u64) -> NodeId {
        NodeId(self.node.fetch_add(n, Ordering::Relaxed))
    }

    /// `n` fresh consecutive edge ids, the first returned.
    pub(crate) fn reserve_edges(&self, n: u64) -> EdgeId {
        EdgeId(self.edge.fetch_add(n, Ordering::Relaxed))
    }

    fn bump_node(&self, used: NodeId) {
        self.node.fetch_max(used.0 + 1, Ordering::Relaxed);
    }

    fn bump_edge(&self, used: EdgeId) {
        self.edge.fetch_max(used.0 + 1, Ordering::Relaxed);
    }
}

/// In-memory property graph.
///
/// Nodes and edges with labels, types and properties, plus adjacency lists and
/// secondary indexes. Tracks dirty and deleted entities for incremental flush.
#[derive(Debug)]
pub struct Graph {
    nodes: IdTable<NodeId, Node>,
    edges: IdTable<EdgeId, Edge>,
    propless: IdTable<EdgeId, ProplessEdge>,
    adj_out: AdjacencyStore,
    adj_in: AdjacencyStore,
    label_index: FxHashMap<LabelId, IdSet<NodeId>>,
    edge_type_index: FxHashMap<EdgeTypeId, IdSet<EdgeId>>,
    strings: SharedStrings,
    indexes: IndexRegistry,
    edge_indexes: EdgeIndexRegistry,
    vector_indexes: crate::graph::vector::VectorIndexRegistry,
    fulltext_indexes: crate::graph::fulltext::FulltextIndexRegistry,
    spatial_indexes: crate::graph::spatial::SpatialIndexRegistry,
    text_indexes: crate::graph::text::TextIndexRegistry,
    constraints: Arc<ConstraintRegistry>,
    stats: Option<Arc<GraphStatistics>>,
    ids: Arc<IdCounters>,
    dirty_nodes: FxHashSet<NodeId>,
    dirty_edges: FxHashSet<EdgeId>,
    deleted_nodes: FxHashSet<NodeId>,
    deleted_edges: FxHashSet<EdgeId>,
    strings_dirty: bool,
    estimated_memory: usize,
    /// Schema metadata: all property keys ever used (additive, never shrinks).
    known_prop_keys: FxHashSet<PropertyKeyId>,
    /// Per-node MVCC delta chain heads, only for nodes with live deltas. Not serialized.
    pub(crate) node_delta_heads: FxHashMap<NodeId, AtomicPtr<MvccDelta>>,
    /// Per-edge MVCC delta chain heads, only for edges with live deltas. Not serialized.
    pub(crate) edge_delta_heads: FxHashMap<EdgeId, AtomicPtr<MvccDelta>>,
    /// Created-id runs and ghost memberships beside the chains. Not serialized.
    versions: crate::tx::versions::Versions,
    /// Typed property columns over the id prefix. Rebuilt on demand, never serialized.
    columns: columns::ColumnSet,
    /// Edges whose two ends are the same node.
    loops: usize,
    /// Nodes without a label: none means the label indexes on a property cover every node (see
    /// [`property_seek`](Self::property_seek)).
    unlabeled: usize,
    /// A bulk load into a database with no file yet: new records skip the dirty sets, since the
    /// first save rewrites the file whole.
    bulk: bool,
}

impl Clone for Graph {
    fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            propless: self.propless.clone(),
            adj_out: self.adj_out.clone(),
            adj_in: self.adj_in.clone(),
            label_index: self.label_index.clone(),
            edge_type_index: self.edge_type_index.clone(),
            strings: self.strings.clone(),
            indexes: self.indexes.clone(),
            edge_indexes: self.edge_indexes.clone(),
            vector_indexes: self.vector_indexes.clone(),
            fulltext_indexes: self.fulltext_indexes.clone(),
            spatial_indexes: self.spatial_indexes.clone(),
            text_indexes: self.text_indexes.clone(),
            constraints: self.constraints.clone(),
            stats: self.stats.clone(),
            ids: IdCounters::shared(self.ids.next_node(), self.ids.next_edge()),
            dirty_nodes: self.dirty_nodes.clone(),
            dirty_edges: self.dirty_edges.clone(),
            deleted_nodes: self.deleted_nodes.clone(),
            deleted_edges: self.deleted_edges.clone(),
            strings_dirty: self.strings_dirty,
            estimated_memory: self.estimated_memory,
            known_prop_keys: self.known_prop_keys.clone(),
            // delta chains are transient, never cloned
            node_delta_heads: FxHashMap::default(),
            edge_delta_heads: FxHashMap::default(),
            versions: Default::default(),
            columns: columns::ColumnSet::new(),
            loops: self.loops,
            unlabeled: self.unlabeled,
            bulk: false,
        }
    }
}

/// Entities modified or deleted since the last flush.
#[derive(Debug, Default)]
pub struct DirtySet {
    /// Nodes that were created or modified.
    pub dirty_nodes: FxHashSet<NodeId>,
    /// Edges that were created or modified.
    pub dirty_edges: FxHashSet<EdgeId>,
    /// Nodes that were deleted.
    pub deleted_nodes: FxHashSet<NodeId>,
    /// Edges that were deleted.
    pub deleted_edges: FxHashSet<EdgeId>,
    /// Whether the string table was modified.
    pub strings_dirty: bool,
}

impl DirtySet {
    /// Returns `true` if there are no changes tracked.
    pub fn is_empty(&self) -> bool {
        self.dirty_nodes.is_empty()
            && self.dirty_edges.is_empty()
            && self.deleted_nodes.is_empty()
            && self.deleted_edges.is_empty()
            && !self.strings_dirty
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    /// Creates a new, empty graph.
    pub fn new() -> Self {
        Self {
            nodes: IdTable::new(),
            edges: IdTable::new(),
            propless: IdTable::new(),
            adj_out: AdjacencyStore::new(),
            adj_in: AdjacencyStore::new(),
            label_index: FxHashMap::default(),
            edge_type_index: FxHashMap::default(),
            strings: SharedStrings::default(),
            indexes: IndexRegistry::new(),
            edge_indexes: EdgeIndexRegistry::new(),
            vector_indexes: Default::default(),
            fulltext_indexes: Default::default(),
            spatial_indexes: Default::default(),
            text_indexes: Default::default(),
            constraints: Default::default(),
            stats: None,
            ids: IdCounters::shared(0, 0),
            dirty_nodes: FxHashSet::default(),
            dirty_edges: FxHashSet::default(),
            deleted_nodes: FxHashSet::default(),
            deleted_edges: FxHashSet::default(),
            strings_dirty: false,
            estimated_memory: 0,
            known_prop_keys: FxHashSet::default(),
            node_delta_heads: FxHashMap::default(),
            edge_delta_heads: FxHashMap::default(),
            versions: Default::default(),
            columns: columns::ColumnSet::new(),
            loops: 0,
            unlabeled: 0,
            bulk: false,
        }
    }

    /// Delta chain head of a node, or null.
    #[inline]
    pub(crate) fn node_delta_head(&self, id: NodeId) -> *const MvccDelta {
        self.node_delta_heads
            .get(&id)
            .map_or(std::ptr::null(), |p| {
                p.load(std::sync::atomic::Ordering::Acquire)
            })
    }

    /// Delta chain head of an edge, or null.
    #[inline]
    pub(crate) fn edge_delta_head(&self, id: EdgeId) -> *const MvccDelta {
        self.edge_delta_heads
            .get(&id)
            .map_or(std::ptr::null(), |p| {
                p.load(std::sync::atomic::Ordering::Acquire)
            })
    }

    /// A node's delta head slot, inserted if missing.
    #[inline]
    pub(crate) fn node_delta_head_mut(&mut self, id: NodeId) -> &AtomicPtr<MvccDelta> {
        self.node_delta_heads
            .entry(id)
            .or_insert_with(|| AtomicPtr::new(std::ptr::null_mut()))
    }

    /// An edge's delta head slot, inserted if missing.
    #[inline]
    pub(crate) fn edge_delta_head_mut(&mut self, id: EdgeId) -> &AtomicPtr<MvccDelta> {
        self.edge_delta_heads
            .entry(id)
            .or_insert_with(|| AtomicPtr::new(std::ptr::null_mut()))
    }

    /// Any version state (delta chains, created runs, ghosts)? Without any, every snapshot sees
    /// the live graph and readers take the plain paths.
    #[inline]
    pub(crate) fn has_active_deltas(&self) -> bool {
        !self.node_delta_heads.is_empty()
            || !self.edge_delta_heads.is_empty()
            || !self.versions.is_empty()
    }

    /// Returns the estimated memory usage in bytes.
    pub fn estimated_memory(&self) -> usize {
        self.estimated_memory + self.columns.bytes()
    }

    /// Per-structure memory breakdown.
    pub fn memory_breakdown(&self) -> Vec<(String, usize)> {
        use std::mem::size_of;
        let mut out = Vec::new();

        // nodes HashMap: capacity * (key + value) + ctrl bytes
        let nodes_cap = self.nodes.capacity();
        let nodes_inline = nodes_cap * (size_of::<NodeId>() + size_of::<Node>()) + nodes_cap + 16;
        let nodes_heap: usize = self
            .nodes
            .values()
            .map(|n| {
                n.properties.data_capacity()  // actual heap allocation
            + if n.labels.spilled() { n.labels.len() * size_of::<LabelId>() } else { 0 }
            })
            .sum();
        out.push(("nodes_inline".into(), nodes_inline));
        out.push(("nodes_heap_props".into(), nodes_heap));

        // edges HashMap
        let edges_cap = self.edges.capacity();
        let edges_inline = edges_cap * (size_of::<EdgeId>() + size_of::<Edge>()) + edges_cap + 16;
        let edges_heap: usize = self
            .edges
            .values()
            .map(|e| e.properties.data_capacity())
            .sum();
        out.push(("edges_inline".into(), edges_inline));
        out.push(("edges_heap_props".into(), edges_heap));

        // propless HashMap
        let propless_cap = self.propless.capacity();
        let propless_size =
            propless_cap * (size_of::<EdgeId>() + size_of::<ProplessEdge>()) + propless_cap + 16;
        out.push(("propless".into(), propless_size));

        // adjacency stores
        let adj_out_size = self.adj_out.memory_usage();
        let adj_in_size = self.adj_in.memory_usage();
        out.push(("adj_out".into(), adj_out_size));
        out.push(("adj_in".into(), adj_in_size));

        // label_index
        let label_idx_size: usize = self
            .label_index
            .values()
            .map(|set| set.memory_usage() + 16)
            .sum();
        out.push(("label_index".into(), label_idx_size));

        // edge_type_index
        let edge_type_idx_size: usize = self
            .edge_type_index
            .values()
            .map(|set| set.memory_usage() + 16)
            .sum();
        out.push(("edge_type_index".into(), edge_type_idx_size));

        // property indexes
        let prop_idx_size: usize = self
            .indexes
            .iter()
            .map(|(_, idx)| {
                idx.total_entries() * (size_of::<NodeId>() + 48) // BTreeMap overhead per entry
            })
            .sum();
        out.push(("node_prop_indexes".into(), prop_idx_size));
        out.push(("prop_columns".into(), self.columns.bytes()));

        // edge property indexes
        let edge_prop_idx_size: usize = self
            .edge_indexes
            .iter_prop()
            .map(|(_, idx)| idx.total_entries() * (size_of::<EdgeId>() + 48))
            .sum();
        out.push(("edge_prop_indexes".into(), edge_prop_idx_size));

        out.push(("estimated_memory".into(), self.estimated_memory));

        out
    }

    // -- Dirty tracking ------------------------------------------------

    /// Returns `true` if the graph has any uncommitted changes.
    pub fn has_changes(&self) -> bool {
        !self.dirty_nodes.is_empty()
            || !self.dirty_edges.is_empty()
            || !self.deleted_nodes.is_empty()
            || !self.deleted_edges.is_empty()
            || self.strings_dirty
    }

    /// Bulk load into a database with no file yet: records created while this is on skip the
    /// dirty sets, since the first save rewrites the file whole.
    pub fn set_bulk(&mut self, on: bool) {
        self.bulk = on;
    }

    /// Takes the dirty set, leaving the graph's tracking state cleared.
    pub fn take_dirty(&mut self) -> DirtySet {
        DirtySet {
            dirty_nodes: std::mem::take(&mut self.dirty_nodes),
            dirty_edges: std::mem::take(&mut self.dirty_edges),
            deleted_nodes: std::mem::take(&mut self.deleted_nodes),
            deleted_edges: std::mem::take(&mut self.deleted_edges),
            strings_dirty: std::mem::replace(&mut self.strings_dirty, false),
        }
    }

    /// Clears the dirty tracking state without returning it.
    pub fn clear_dirty(&mut self) {
        self.dirty_nodes.clear();
        self.dirty_edges.clear();
        self.deleted_nodes.clear();
        self.deleted_edges.clear();
        self.strings_dirty = false;
    }
}
