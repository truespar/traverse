//! `GraphView`: one interface over `Graph`, `ReadTx` and `WriteTx` for the Cypher executor.

use std::borrow::Cow;

use rustc_hash::FxHashSet;

use crate::error::Result;
use crate::graph::adjacency::AdjacencySnapshot;
use crate::graph::batch::{AdjBatch, ColumnBound, ColumnCmp, ColumnSink};
use crate::graph::constraint::{ConstraintRegistry, ConstraintType};
use crate::graph::edge::Edge;
use crate::graph::index::{EdgeIndexCatalog, IndexCatalog};
use crate::graph::node::Node;
use crate::graph::property::{IndexKey, PropertyMap, PropertyValue};
use crate::graph::stats::GraphStatistics;
use crate::graph::types::{
    Direction, EdgeEndpoint, EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId,
};
use crate::interner::StringTable;

/// Graph view implemented by `Graph`, `ReadTx` and `WriteTx`.
///
/// The executor uses `&mut dyn GraphView` for writes, `&dyn GraphView` for reads.
/// `Cow` returns carry both borrowed (direct graph) and owned (snapshot-rebuilt) values.
pub trait GraphView {
    // -- Statements and batches -----------------------------------------

    /// Changes made through this view so far, as a counter: a statement that failed after it
    /// moved left partial changes behind. 0 for views that don't count.
    fn write_count(&self) -> u64 {
        0
    }

    /// An explicit transaction, opened and ended by its client: `CALL { ... } IN TRANSACTIONS`
    /// can't run in one (its batches would commit the client's work).
    fn is_explicit_transaction(&self) -> bool {
        false
    }

    /// Can [`commit_batch`](Self::commit_batch) commit the work so far and carry on? Auto-commit
    /// write transactions can.
    fn can_commit_batches(&self) -> bool {
        false
    }

    /// Commits everything written so far and carries on as a fresh transaction (a
    /// `CALL { ... } IN TRANSACTIONS` batch).
    fn commit_batch(&mut self) -> Result<()> {
        Err(crate::error::Error::Unsupported(
            "this transaction can't commit in batches",
        ))
    }

    /// Discards everything written since the start or the last batch commit, and carries on (a
    /// failed batch).
    fn rollback_batch(&mut self) -> Result<()> {
        Err(crate::error::Error::Unsupported(
            "this transaction can't roll back batches",
        ))
    }

    // -- Reads ----------------------------------------------------------

    /// `Sync` reference for parallel reads: `Some` for `ReadTx` and `Graph`, `None` for `WriteTx`.
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        None
    }

    /// Identity of the visible data: two views returning the same `Some` see identical data, so
    /// results derived from one can be reused for the other (per-thread caches). `None` for views
    /// whose data can change (a writer, a bare `Graph`).
    fn data_version(&self) -> Option<(usize, u64)> {
        None
    }

    /// Node by ID.
    fn get_node(&self, id: NodeId) -> Result<Cow<'_, Node>>;

    /// Edge by ID.
    fn get_edge(&self, id: EdgeId) -> Result<Cow<'_, Edge>>;

    /// Whether the node has `label`; `Ok(false)` for missing nodes.
    fn has_label(&self, node: NodeId, label: LabelId) -> Result<bool> {
        match self.get_node(node) {
            Ok(n) => Ok(n.labels.contains(&label)),
            Err(_) => Ok(false),
        }
    }

    /// Node count.
    fn node_count(&self) -> usize;

    /// Edge count.
    fn edge_count(&self) -> usize;

    /// All node IDs.
    fn nodes_vec(&self) -> Vec<NodeId>;

    /// All edge IDs.
    fn all_edges_vec(&self) -> Vec<EdgeId>;

    /// Node IDs with `label`.
    fn nodes_by_label(&self, label: &str) -> Vec<NodeId>;

    /// Edge IDs of type `edge_type`. Uses the edge-type index when present, else
    /// scans all edges: correct but O(|E|), so implementations should fast-path it.
    fn edges_by_type(&self, edge_type: &str) -> Vec<EdgeId> {
        // default: filter all edges; implementations should override with an index lookup
        let target = match self.strings().get(edge_type) {
            Some(raw) => raw,
            None => return Vec::new(),
        };
        self.all_edges_vec()
            .into_iter()
            .filter(|eid| {
                self.get_edge(*eid)
                    .map(|e| e.edge_type.0 == target)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// All label names (`CALL db.labels()`). Default empty; `Graph` and transaction views override.
    fn label_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// All relationship type names (`CALL db.relationshipTypes()`). Default empty; `Graph` and
    /// transaction views override.
    fn relationship_type_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// All property key names (`CALL db.propertyKeys()`). Default empty; `Graph` and transaction
    /// views override.
    fn property_key_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Neighbor IDs of `node` in `direction`.
    fn neighbors(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<NodeId>>;

    /// Edge IDs of `node` in `direction`.
    fn edges_of(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<EdgeId>>;

    /// One node property, without cloning the node.
    fn get_node_property(&self, id: NodeId, key: &str) -> Result<Option<PropertyValue>> {
        let node = self.get_node(id)?;
        let kid = match self.strings().get(key) {
            Some(raw) => PropertyKeyId(raw),
            None => return Ok(None),
        };
        Ok(node.properties.get(kid))
    }

    /// Like `get_node_property` with a pre-resolved key (no string hash).
    fn get_node_property_kid(
        &self,
        id: NodeId,
        kid: PropertyKeyId,
    ) -> Result<Option<PropertyValue>> {
        let node = self.get_node(id)?;
        Ok(node.properties.get(kid))
    }

    /// One edge property, without cloning the edge.
    fn get_edge_property(&self, id: EdgeId, key: &str) -> Result<Option<PropertyValue>> {
        let edge = self.get_edge(id)?;
        let kid = match self.strings().get(key) {
            Some(raw) => PropertyKeyId(raw),
            None => return Ok(None),
        };
        Ok(edge.properties.get(kid))
    }

    /// Batch property read into a flat buffer: `out[i * keys.len() + j]` = `keys[j]`
    /// of `node_ids[i]`. Default calls `get_node()` per node; overrides amortize locking.
    fn batch_read_node_properties(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out: &mut [Option<PropertyValue>],
    ) -> Result<()> {
        let nk = keys.len();
        for (i, &nid) in node_ids.iter().enumerate() {
            let base = i * nk;
            match self.get_node(nid) {
                Ok(node) => {
                    node.properties.get_multi(keys, &mut out[base..base + nk]);
                }
                Err(_) => {
                    for j in 0..nk {
                        out[base + j] = None;
                    }
                }
            }
        }
        Ok(())
    }

    /// Typed i64 batch read: `out_vals[i * keys.len() + j]` = `keys[j]` of
    /// `node_ids[i]`; `out_valid[idx] = false` for missing nodes or keys and
    /// non-integers. Default: `get_node()` + `get_multi_int()` per node.
    fn batch_read_node_properties_i64(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out_vals: &mut [i64],
        out_valid: &mut [bool],
    ) -> Result<()> {
        let nk = keys.len();
        for (i, &nid) in node_ids.iter().enumerate() {
            let base = i * nk;
            match self.get_node(nid) {
                Ok(node) => {
                    node.properties.get_multi_int(
                        keys,
                        &mut out_vals[base..base + nk],
                        &mut out_valid[base..base + nk],
                    );
                }
                Err(_) => {
                    for j in 0..nk {
                        out_valid[base + j] = false;
                    }
                }
            }
        }
        Ok(())
    }

    // -- Batch access -----------------------------------------------------
    // The vectorized engine reads through these: one call per batch, typed columns out, no
    // per-entity clone. The defaults are correct over the per-entity methods; `Graph`,
    // `ReadTx` and `OptimisticTx` override them with one-lock implementations.

    /// Pushes `keys[j]` of every node in `ids` into `sinks[j]`, in order: exactly one push per
    /// node per sink, a null for a missing node or key. Returns how many ids named no node.
    fn gather_node_props(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        let mut missing = 0;
        for &id in ids {
            match self.get_node(id) {
                Ok(node) => node.properties.get_multi_into(keys, sinks),
                Err(_) => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
            }
        }
        Ok(missing)
    }

    /// [`gather_node_props`](Self::gather_node_props) for relationships.
    fn gather_edge_props(
        &self,
        ids: &[EdgeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        let mut missing = 0;
        for &id in ids {
            match self.get_edge(id) {
                Ok(edge) => edge.properties.get_multi_into(keys, sinks),
                Err(_) => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
            }
        }
        Ok(missing)
    }

    /// Whether each node in `ids` exists in this view; `out` is cleared and filled in order.
    fn nodes_exist(&self, ids: &[NodeId], out: &mut Vec<bool>) {
        out.clear();
        out.extend(ids.iter().map(|&id| self.get_node(id).is_ok()));
    }

    /// Whether each edge in `ids` exists in this view; `out` is cleared and filled in order.
    fn edges_exist(&self, ids: &[EdgeId], out: &mut Vec<bool>) {
        out.clear();
        out.extend(ids.iter().map(|&id| self.get_edge(id).is_ok()));
    }

    /// Whether each node in `ids` has `label` (`false` for a missing node); `out` is cleared
    /// and filled in order.
    fn nodes_have_label(&self, ids: &[NodeId], label: LabelId, out: &mut Vec<bool>) {
        out.clear();
        out.extend(
            ids.iter()
                .map(|&id| self.has_label(id, label).unwrap_or(false)),
        );
    }

    /// Nodes with `label`, counted without listing them.
    fn count_nodes_by_label(&self, label: &str) -> usize {
        self.nodes_by_label(label).len()
    }

    /// Edges of `edge_type`, counted without listing them.
    fn count_edges_by_type(&self, edge_type: &str) -> usize {
        self.edges_by_type(edge_type).len()
    }

    /// Entries of the `(label, prop)` index with exactly `value`, counted without listing them;
    /// `None` without such an index.
    fn count_index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<usize> {
        self.index_lookup(label, prop, value).map(|v| v.len())
    }

    /// The nodes with `label` whose `prop` satisfies `cmp` against `bound`, counted in storage;
    /// `None` when the view can't answer exactly that way.
    fn count_nodes_where(
        &self,
        _label: &str,
        _prop: &str,
        _cmp: ColumnCmp,
        _bound: &ColumnBound,
    ) -> Option<usize> {
        None
    }

    /// The nodes with `label` whose `prop` satisfies `cmp` against `bound`, listed in storage
    /// in id order; `None` when the view can't answer exactly that way.
    fn nodes_where(
        &self,
        _label: &str,
        _prop: &str,
        _cmp: ColumnCmp,
        _bound: &ColumnBound,
    ) -> Option<Vec<NodeId>> {
        None
    }

    /// The share of the `(label, prop)` index a key range covers, estimated from the key
    /// extremes; `None` without an index, or when the keys aren't numeric. For choosing a plan,
    /// not for counting.
    fn index_range_estimate(
        &self,
        _label: &str,
        _prop: &str,
        _lower: &std::ops::Bound<IndexKey>,
        _upper: &std::ops::Bound<IndexKey>,
    ) -> Option<f64> {
        None
    }

    /// Entries of the `(label, prop)` index in a key range, counted without listing them; `None`
    /// without such an index.
    fn count_index_range(
        &self,
        label: &str,
        prop: &str,
        lower: std::ops::Bound<IndexKey>,
        upper: std::ops::Bound<IndexKey>,
    ) -> Option<usize> {
        self.index_range_lookup(label, prop, lower, upper)
            .map(|v| v.len())
    }

    /// Edges of every source in `srcs` in `dir`, filtered by type, appended to `out` in source
    /// order. An undirected expansion lists a self-loop once. A missing source is an error.
    fn expand_batch(
        &self,
        srcs: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut AdjBatch,
    ) -> Result<()> {
        for (lane, &src) in srcs.iter().enumerate() {
            for ep in self.edges_with_neighbor(src, dir, None)? {
                if let Some(allowed) = type_filter
                    && !allowed.contains(&ep.edge_type)
                {
                    continue;
                }
                let outgoing = ep.source == src;
                // an undirected self-loop shows up in both adjacency lists
                if dir == Direction::Both && ep.neighbor == src && !outgoing {
                    continue;
                }
                out.push(lane as u32, ep.edge_id, ep.edge_type, ep.neighbor, outgoing);
            }
        }
        Ok(())
    }

    // -- Whole-graph adjacency -----------------------------------------

    /// Adjacency for whole-graph algorithms, fixed for the snapshot's life and exact for this
    /// view; `None` when the view can't provide one cheaply ([`AdjacencySnapshot::of`] then builds
    /// a projection from the per-node reads).
    fn adjacency_snapshot(&self) -> Option<AdjacencySnapshot<'_>> {
        None
    }

    /// One past the highest `NodeId.0` ever assigned; sizes dense per-node arrays.
    fn highest_node_id(&self) -> u64 {
        self.node_count() as u64
    }

    /// Topology (no properties) of the edges of `node`.
    fn edges_with_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        let edge_ids = self.edges_of(node, dir, filter)?;
        let mut result = Vec::with_capacity(edge_ids.len());
        for eid in edge_ids {
            let edge = self.get_edge(eid)?;
            let neighbor = match dir {
                Direction::Outgoing => edge.target,
                Direction::Incoming => edge.source,
                Direction::Both => {
                    if edge.source == node {
                        edge.target
                    } else {
                        edge.source
                    }
                }
            };
            result.push(EdgeEndpoint {
                edge_id: eid,
                edge_type: edge.edge_type,
                neighbor,
                source: edge.source,
                target: edge.target,
            });
        }
        Ok(result)
    }

    /// Neighbor IDs with a pre-resolved type filter. Default: edges_of + get_edge (correct, slow).
    fn neighbors_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        let edge_ids = self.edges_of(node, dir, None)?;
        let mut result = Vec::with_capacity(edge_ids.len());
        for eid in edge_ids {
            let edge = self.get_edge(eid)?;
            if let Some(allowed) = type_filter
                && !allowed.contains(&edge.edge_type)
            {
                continue;
            }
            let neighbor = match dir {
                Direction::Outgoing => edge.target,
                Direction::Incoming => edge.source,
                Direction::Both => {
                    if edge.source == node {
                        edge.target
                    } else {
                        edge.source
                    }
                }
            };
            result.push(neighbor);
        }
        Ok(result)
    }

    /// `(EdgeId, NodeId)` pairs with a pre-resolved type filter, without full get_edge(); the edge
    /// id serves path tracking and weights.
    fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        let edge_ids = self.edges_of(node, dir, None)?;
        let mut result = Vec::with_capacity(edge_ids.len());
        for eid in edge_ids {
            let edge = self.get_edge(eid)?;
            if let Some(allowed) = type_filter
                && !allowed.contains(&edge.edge_type)
            {
                continue;
            }
            let neighbor = match dir {
                Direction::Outgoing => edge.target,
                Direction::Incoming => edge.source,
                Direction::Both => {
                    if edge.source == node {
                        edge.target
                    } else {
                        edge.source
                    }
                }
            };
            result.push((eid, neighbor));
        }
        Ok(result)
    }

    /// Edges from `from` to `to` in `direction`, filtered by type: scans `from`'s adjacency for
    /// `to`.
    fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        let edge_ids = self.edges_of(from, dir, None)?;
        let mut result = Vec::new();
        for eid in edge_ids {
            let edge = self.get_edge(eid)?;
            if let Some(allowed) = type_filter
                && !allowed.contains(&edge.edge_type)
            {
                continue;
            }
            let neighbor = match dir {
                Direction::Outgoing => edge.target,
                Direction::Incoming => edge.source,
                Direction::Both => {
                    if edge.source == from {
                        edge.target
                    } else {
                        edge.source
                    }
                }
            };
            if neighbor == to {
                result.push(eid);
            }
        }
        Ok(result)
    }

    /// Calls `f` per neighbor with a pre-resolved type filter, no Vec. Default: edges_of + get_edge
    /// (correct, slow).
    fn for_each_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        let edge_ids = self.edges_of(node, dir, None)?;
        for eid in edge_ids {
            let edge = self.get_edge(eid)?;
            if let Some(allowed) = type_filter
                && !allowed.contains(&edge.edge_type)
            {
                continue;
            }
            let neighbor = match dir {
                Direction::Outgoing => edge.target,
                Direction::Incoming => edge.source,
                Direction::Both => {
                    if edge.source == node {
                        edge.target
                    } else {
                        edge.source
                    }
                }
            };
            f(neighbor);
        }
        Ok(())
    }

    /// Expands a whole frontier in one call, inserting neighbors into `out`. Default
    /// loops `for_each_neighbor`; ReadTx and Graph amortize locking and skip
    /// per-edge `dyn FnMut` dispatch.
    fn expand_frontier(
        &self,
        nodes: &[NodeId],
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut FxHashSet<NodeId>,
    ) -> Result<()> {
        for &nid in nodes {
            self.for_each_neighbor(nid, direction, type_filter, &mut |neighbor| {
                out.insert(neighbor);
            })?;
        }
        Ok(())
    }

    /// Degree of `node` in `dir`.
    fn degree(&self, node: NodeId, dir: Direction) -> Result<usize>;

    /// The nodes with `label` as a bitmap over node ids (bit `i` set: node `i` has the label),
    /// when the view can hand one out cheaply; `None` means list the ids instead.
    fn label_bitmap(&self, _label: &str) -> Option<Vec<u64>> {
        None
    }

    /// How many edges start and end on the same node, when the view knows exactly.
    fn self_loops(&self) -> Option<usize> {
        None
    }

    /// Degrees of `ids` in `dir` into `out` (0 for a missing node), in one pass.
    fn degrees(&self, ids: &[NodeId], dir: Direction, out: &mut Vec<usize>) {
        out.clear();
        out.extend(ids.iter().map(|&n| self.degree(n, dir).unwrap_or(0)));
    }

    /// String table.
    fn strings(&self) -> &StringTable;

    /// Graph statistics, if collected.
    fn statistics(&self) -> Option<&GraphStatistics>;

    // -- Writes ---------------------------------------------------------

    /// Creates a node.
    fn create_node(&mut self, labels: &[&str], props: PropertyMap) -> Result<NodeId>;

    /// Creates a directed edge.
    fn create_edge(
        &mut self,
        src: NodeId,
        tgt: NodeId,
        edge_type: &str,
        props: PropertyMap,
    ) -> Result<EdgeId>;

    /// Creates a node with an explicit ID, for redo-log replay only (later ops reference the
    /// recorded IDs). Default errors; `WriteTx`/`Graph` override.
    fn create_node_with_id(
        &mut self,
        _id: NodeId,
        _labels: &[&str],
        _props: PropertyMap,
    ) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Creates an edge with an explicit ID, for redo-log replay only.
    fn create_edge_with_id(
        &mut self,
        _id: EdgeId,
        _src: NodeId,
        _tgt: NodeId,
        _edge_type: &str,
        _props: PropertyMap,
    ) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Replaces all node properties.
    fn update_node_properties(&mut self, id: NodeId, props: PropertyMap) -> Result<()>;

    /// Replaces all edge properties.
    fn update_edge_properties(&mut self, id: EdgeId, props: PropertyMap) -> Result<()>;

    /// Deletes a node; fails while edges are attached.
    fn delete_node(&mut self, id: NodeId) -> Result<()>;

    /// Deletes a node and its edges.
    fn delete_node_cascading(&mut self, id: NodeId) -> Result<()>;

    /// Deletes an edge.
    fn delete_edge(&mut self, id: EdgeId) -> Result<()>;

    /// Adds a label to a node.
    fn add_label(&mut self, id: NodeId, label: &str) -> Result<()>;

    /// Removes a label from a node.
    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<()>;

    /// Mutable node by ID.
    fn get_node_mut(&mut self, id: NodeId) -> Result<&mut Node>;

    /// Mutable edge by ID.
    fn get_edge_mut(&mut self, id: EdgeId) -> Result<&mut Edge>;

    // -- Indexes --------------------------------------------------------

    /// Creates a named property index.
    fn create_index(&mut self, name: &str, label: &str, prop: &str) -> Result<()>;

    /// Composite (multi-property) b-tree index, node or relationship (`is_relationship`). Stored
    /// under a synthetic joined key; see `Graph::create_composite_index`.
    fn create_composite_index(
        &mut self,
        _name: &str,
        _label: &str,
        _properties: &[String],
        _is_relationship: bool,
    ) -> Result<()> {
        Err(crate::error::Error::IndexError(
            "composite indexes are not supported by this graph view".to_string(),
        ))
    }

    /// Drops a named property index.
    fn drop_index(&mut self, name: &str) -> Result<()>;

    /// Node IDs via a property index.
    fn index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>>;

    /// Nodes, whatever their labels, whose `prop` is `value`, through the label indexes on `prop`
    /// when they cover every node this view sees; `None` otherwise, and the caller scans.
    fn property_seek(&self, _prop: &str, _value: &IndexKey) -> Option<Vec<NodeId>> {
        None
    }

    /// Whether [`property_seek`](Self::property_seek) answers for `prop` as things stand.
    fn property_seek_covers(&self, _prop: &str) -> bool {
        false
    }

    /// Node IDs in a key range via a property index.
    fn index_range_lookup(
        &self,
        label: &str,
        prop: &str,
        lower: std::ops::Bound<IndexKey>,
        upper: std::ops::Bound<IndexKey>,
    ) -> Option<Vec<NodeId>>;

    /// Node-index metadata (descriptors, generation) as of this view.
    fn index_catalog(&self) -> &IndexCatalog;

    /// Entries in the node or edge property index `name` (a statistic: current, not snapshot).
    fn index_size(&self, _name: &str) -> Option<usize> {
        None
    }

    // -- Vector (HNSW) indexes --------------------------------------
    // No-op defaults for views without graph access; ReadTx / WriteTx / Graph override.

    /// Creates a vector (HNSW) index on a label's vector property.
    fn create_vector_index(
        &mut self,
        _name: &str,
        _label: &str,
        _property: &str,
        _dim: usize,
        _sim: crate::graph::vector::Similarity,
        _params: crate::graph::vector::HnswParams,
    ) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Drop a vector index by name.
    fn drop_vector_index(&mut self, _name: &str) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// True if a vector index of this name exists.
    fn has_vector_index(&self, _name: &str) -> bool {
        false
    }

    /// k approximate nearest neighbors of `query` via the named index, `(NodeId, score)` best
    /// first. `None` if no such index.
    fn query_vector_index(
        &self,
        _name: &str,
        _query: &[f32],
        _k: usize,
        _ef: Option<usize>,
    ) -> Option<Vec<(NodeId, f64)>> {
        None
    }

    /// Vector-index descriptors for SHOW INDEXES: `(name, label, property, dimensions,
    /// similarity_function)`.
    fn vector_index_descriptors(&self) -> Vec<(String, String, String, usize, &'static str)> {
        Vec::new()
    }

    // -- Full-text (BM25) indexes -----------------------------------
    // No-op defaults for views without graph access; ReadTx / WriteTx / Graph override.

    /// Creates a BM25 full-text index over string properties of one or more labels.
    fn create_fulltext_index(
        &mut self,
        _name: &str,
        _labels: &[&str],
        _properties: &[&str],
        _analyzer: crate::graph::fulltext::Analyzer,
    ) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Drop a full-text index by name.
    fn drop_fulltext_index(&mut self, _name: &str) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// True if a full-text index of this name exists.
    fn has_fulltext_index(&self, _name: &str) -> bool {
        false
    }

    /// BM25 search of the named index: up to `limit` `(NodeId, score)` best first. `None` if no
    /// such index.
    fn query_fulltext_index(
        &self,
        _name: &str,
        _query: &str,
        _limit: usize,
    ) -> Option<Vec<(NodeId, f64)>> {
        None
    }

    /// Full-text-index descriptors for SHOW INDEXES: `(name, labels, properties, analyzer)`.
    fn fulltext_index_descriptors(&self) -> Vec<(String, Vec<String>, Vec<String>, String)> {
        Vec::new()
    }

    // -- Point (spatial / R-tree) indexes ---------------------------
    // No-op defaults for views without graph access; ReadTx / WriteTx / Graph override.

    /// Creates an R-tree point index on `(label, property)`.
    fn create_point_index(&mut self, _name: &str, _label: &str, _property: &str) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Drop a point index by name.
    fn drop_point_index(&mut self, _name: &str) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// True if a point index of this name exists.
    fn has_point_index(&self, _name: &str) -> bool {
        false
    }

    /// Whether a point index covers `(label, property)` (plan-time check).
    fn has_point_index_for(&self, _label: &str, _property: &str) -> bool {
        false
    }

    /// `point.withinBBox` acceleration: nodes in the SRID-matching box, or `None` without a
    /// covering index (caller scans).
    fn point_index_within_bbox(
        &self,
        _label: &str,
        _property: &str,
        _ll: [f64; 3],
        _ur: [f64; 3],
        _srid: i64,
    ) -> Option<Vec<NodeId>> {
        None
    }

    /// `point.distance(...) <= r` acceleration: `(node, distance)` within the radius, or `None`
    /// without a covering index.
    fn point_index_within_distance(
        &self,
        _label: &str,
        _property: &str,
        _center: [f64; 3],
        _radius: f64,
        _srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        None
    }

    /// `ORDER BY point.distance(...) LIMIT k` acceleration: `k` nearest ascending, or `None`
    /// without a covering index.
    fn point_index_knn(
        &self,
        _label: &str,
        _property: &str,
        _center: [f64; 3],
        _k: usize,
        _srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        None
    }

    /// Point-index descriptors for SHOW INDEXES: `(name, label, property)`.
    fn point_index_descriptors(&self) -> Vec<(String, String, String)> {
        Vec::new()
    }

    // -- Text (trigram) indexes -------------------------------------
    // No-op defaults for views without graph access; ReadTx / WriteTx / Graph override.

    /// Creates a trigram text index on `(label, property)`.
    fn create_text_index(&mut self, _name: &str, _label: &str, _property: &str) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Drop a text index by name.
    fn drop_text_index(&mut self, _name: &str) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// True if a text index of this name exists.
    fn has_text_index(&self, _name: &str) -> bool {
        false
    }

    /// Whether a text index covers `(label, property)` (plan-time check).
    fn has_text_index_for(&self, _label: &str, _property: &str) -> bool {
        false
    }

    /// Candidates for a string predicate: a SUPERSET (caller re-checks), or `None` without a
    /// covering index.
    fn text_index_candidates(
        &self,
        _label: &str,
        _property: &str,
        _kind: crate::graph::text::TextQueryKind,
        _pattern: &str,
    ) -> Option<Vec<NodeId>> {
        None
    }

    /// Text-index descriptors for SHOW INDEXES: `(name, label, property)`.
    fn text_index_descriptors(&self) -> Vec<(String, String, String)> {
        Vec::new()
    }

    // -- In-place mutation index maintenance -------------------------
    // `SET` / `REMOVE` mutate in place via `get_node_mut` / `get_edge_mut`, skipping
    // the maintenance of the create/update/delete APIs. Callers MUST bracket them:
    // `unindex_*` BEFORE (removes entries keyed by OLD values), `index_*` AFTER
    // (re-adds under new values). No-ops for read-only views.

    /// Removes a node's entries from every index, using its current values.
    fn unindex_node(&mut self, _id: NodeId) {}
    /// (Re-)adds a node to every index from its current values.
    fn index_node(&mut self, _id: NodeId) {}
    /// [`unindex_node`](Self::unindex_node) for a SINGLE-property change:
    /// implementations may limit maintenance to indexes using `key`. Default runs the
    /// full bracket, so it's always correct.
    fn unindex_node_prop(&mut self, id: NodeId, _key: &str) {
        self.unindex_node(id);
    }
    /// See [`unindex_node_prop`](Self::unindex_node_prop).
    fn index_node_prop(&mut self, id: NodeId, _key: &str) {
        self.index_node(id);
    }
    /// Targeted single-property variant of [`unindex_edge`](Self::unindex_edge).
    fn unindex_edge_prop(&mut self, id: EdgeId, _key: &str) {
        self.unindex_edge(id);
    }
    /// See [`unindex_edge_prop`](Self::unindex_edge_prop).
    fn index_edge_prop(&mut self, id: EdgeId, _key: &str) {
        self.index_edge(id);
    }
    /// Removes an edge's entries from the edge property indexes.
    fn unindex_edge(&mut self, _id: EdgeId) {}
    /// (Re-)adds an edge to the edge property indexes.
    fn index_edge(&mut self, _id: EdgeId) {}

    /// COMPOSITE (multi-property) UNIQUE / KEY constraint: tuple uniqueness, plus per-key existence
    /// for KEY.
    fn create_composite_constraint(
        &mut self,
        _name: &str,
        _label: &str,
        _properties: &[String],
        _ctype: ConstraintType,
        _is_relationship: bool,
    ) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Interns a property key for InternedPropertyMap.
    fn intern_property_key(&mut self, key: &str) -> PropertyKeyId;

    /// Collects optimizer statistics; empty `labels` = all labels.
    fn analyze_statistics(&mut self, labels: &[String]);

    // -- Constraints ------------------------------------------------

    /// Creates a named constraint on `label.prop`.
    fn create_constraint(
        &mut self,
        name: &str,
        label: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()>;

    /// Creates a named constraint on `edge_type.prop`. Default errors (read-only views); writable
    /// views override.
    fn create_relationship_constraint(
        &mut self,
        _name: &str,
        _edge_type: &str,
        _prop: &str,
        _ctype: ConstraintType,
    ) -> Result<()> {
        Err(crate::error::Error::ReadOnlyTransaction)
    }

    /// Drops a named constraint.
    fn drop_constraint(&mut self, name: &str) -> Result<()>;

    /// Constraint registry.
    fn constraint_registry(&self) -> &ConstraintRegistry;

    // -- Edge Indexes ----------------------------------------------

    /// Creates a named edge index.
    fn create_edge_index(&mut self, name: &str, edge_type: &str, prop: Option<&str>) -> Result<()>;

    /// Drops a named edge index.
    fn drop_edge_index(&mut self, name: &str) -> Result<()>;

    /// Edge IDs via an edge property index.
    fn edge_index_lookup(
        &self,
        edge_type: &str,
        prop: &str,
        value: &IndexKey,
    ) -> Option<Vec<EdgeId>>;

    /// Edge-index metadata as of this view.
    fn edge_index_catalog(&self) -> &EdgeIndexCatalog;
}
