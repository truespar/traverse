//! [`GraphView`] for the committed graph.

use std::borrow::Cow;
use std::sync::Arc;

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::error::{Error, Result};
use crate::interner::StringTable;
use crate::tx::view::GraphView;

use super::batch::{AdjBatch, ColumnBound, ColumnCmp, ColumnSink};
use super::columns::Entity;
use super::constraint::{ConstraintRegistry, ConstraintType};
use super::edge::Edge;
use super::index::{EdgeIndexCatalog, IndexCatalog};
use super::node::Node;
use super::property::{IndexKey, PropertyMap, PropertyValue};
use super::stats::GraphStatistics;
use super::types::{Direction, EdgeEndpoint, EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use super::{Graph, events, stats};

impl GraphView for Graph {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        Some(self)
    }

    fn adjacency_snapshot(&self) -> Option<crate::graph::adjacency::AdjacencySnapshot<'_>> {
        Some(Graph::adjacency_snapshot(self))
    }

    fn highest_node_id(&self) -> u64 {
        self.ids.next_node()
    }

    fn get_node(&self, id: NodeId) -> Result<Cow<'_, Node>> {
        self.nodes
            .get(&id)
            .map(Cow::Borrowed)
            .ok_or(Error::NodeNotFound(id))
    }

    fn get_edge(&self, id: EdgeId) -> Result<Cow<'_, Edge>> {
        Graph::get_edge(self, id)
    }

    fn has_label(&self, node: NodeId, label: LabelId) -> Result<bool> {
        Ok(Graph::has_label(self, node, label))
    }

    fn node_count(&self) -> usize {
        Graph::node_count(self)
    }

    fn edge_count(&self) -> usize {
        Graph::edge_count(self)
    }

    fn nodes_vec(&self) -> Vec<NodeId> {
        Graph::nodes_vec(self)
    }

    fn all_edges_vec(&self) -> Vec<EdgeId> {
        Graph::all_edges_vec(self)
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        Graph::nodes_by_label(self, label)
    }

    fn label_names(&self) -> Vec<String> {
        Graph::label_names(self)
    }

    fn relationship_type_names(&self) -> Vec<String> {
        self.edge_type_entries()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    fn property_key_names(&self) -> Vec<String> {
        Graph::schema_property_keys(self)
    }

    fn edges_by_type(&self, edge_type: &str) -> Vec<EdgeId> {
        let Some(raw) = self.strings.get(edge_type) else {
            return Vec::new();
        };
        match self.edge_type_index.get(&EdgeTypeId(raw)) {
            Some(set) => set.to_vec(),
            None => Vec::new(),
        }
    }

    fn neighbors(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<NodeId>> {
        Graph::neighbors(self, node, dir, filter)
    }

    fn edges_of(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<EdgeId>> {
        Graph::edges_of_ids(self, node, dir, filter)
    }

    fn get_node_property(&self, id: NodeId, key: &str) -> Result<Option<PropertyValue>> {
        Graph::get_node_property(self, id, key)
    }

    fn get_node_property_kid(
        &self,
        id: NodeId,
        kid: PropertyKeyId,
    ) -> Result<Option<PropertyValue>> {
        Graph::get_node_property_kid(self, id, kid)
    }

    fn get_edge_property(&self, id: EdgeId, key: &str) -> Result<Option<PropertyValue>> {
        Graph::get_edge_property(self, id, key)
    }

    fn batch_read_node_properties(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out: &mut [Option<PropertyValue>],
    ) -> Result<()> {
        Graph::batch_read_node_properties(self, node_ids, keys, out);
        Ok(())
    }

    fn batch_read_node_properties_i64(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out_vals: &mut [i64],
        out_valid: &mut [bool],
    ) -> Result<()> {
        Graph::batch_read_node_properties_i64(self, node_ids, keys, out_vals, out_valid);
        Ok(())
    }

    fn gather_node_props(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        Ok(Graph::gather_node_props(self, ids, keys, sinks))
    }

    fn gather_edge_props(
        &self,
        ids: &[EdgeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        Ok(Graph::gather_edge_props(self, ids, keys, sinks))
    }

    fn nodes_exist(&self, ids: &[NodeId], out: &mut Vec<bool>) {
        out.clear();
        out.extend(ids.iter().map(|id| self.nodes.contains_key(id)));
    }

    fn edges_exist(&self, ids: &[EdgeId], out: &mut Vec<bool>) {
        out.clear();
        out.extend(ids.iter().map(|&id| self.has_edge(id)));
    }

    fn nodes_have_label(&self, ids: &[NodeId], label: LabelId, out: &mut Vec<bool>) {
        out.clear();
        match self.label_index.get(&label) {
            Some(set) => out.extend(ids.iter().map(|id| set.contains(id))),
            None => out.resize(ids.len(), false),
        }
    }

    fn count_nodes_by_label(&self, label: &str) -> usize {
        self.strings
            .get(label)
            .and_then(|raw| self.label_index.get(&LabelId(raw)))
            .map_or(0, |s| s.len())
    }

    fn count_edges_by_type(&self, edge_type: &str) -> usize {
        self.strings
            .get(edge_type)
            .and_then(|raw| self.edge_type_index.get(&EdgeTypeId(raw)))
            .map_or(0, |s| s.len())
    }

    fn count_index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<usize> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(prop)?);
        Some(self.indexes.get_index(lid, pkid)?.lookup_count(value))
    }

    fn count_index_range(
        &self,
        label: &str,
        prop: &str,
        lower: std::ops::Bound<IndexKey>,
        upper: std::ops::Bound<IndexKey>,
    ) -> Option<usize> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(prop)?);
        Some(
            self.indexes
                .get_index(lid, pkid)?
                .range_count(lower.as_ref(), upper.as_ref()),
        )
    }

    fn index_range_estimate(
        &self,
        label: &str,
        prop: &str,
        lower: &std::ops::Bound<IndexKey>,
        upper: &std::ops::Bound<IndexKey>,
    ) -> Option<f64> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(prop)?);
        self.indexes
            .get_index(lid, pkid)?
            .range_estimate(lower.as_ref(), upper.as_ref())
    }

    fn expand_batch(
        &self,
        srcs: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut AdjBatch,
    ) -> Result<()> {
        Graph::expand_batch(self, srcs, dir, type_filter, out)
    }

    fn edges_with_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        Graph::edges_with_neighbor(self, node, dir, filter)
    }

    fn neighbors_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        Graph::neighbors_filtered(self, node, dir, type_filter)
    }

    fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        Graph::edge_endpoints_filtered(self, node, dir, type_filter)
    }

    fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        Graph::edges_between(self, from, to, dir, type_filter)
    }

    fn for_each_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        Graph::for_each_neighbor(self, node, dir, type_filter, f)
    }

    fn expand_frontier(
        &self,
        nodes: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut FxHashSet<NodeId>,
    ) -> Result<()> {
        Graph::expand_frontier(self, nodes, dir, type_filter, out)
    }

    fn degree(&self, node: NodeId, dir: Direction) -> Result<usize> {
        Graph::degree(self, node, dir)
    }

    fn degrees(&self, ids: &[NodeId], dir: Direction, out: &mut Vec<usize>) {
        out.clear();
        out.extend(
            ids.iter()
                .map(|&n| Graph::degree(self, n, dir).unwrap_or(0)),
        );
    }

    fn label_bitmap(&self, label: &str) -> Option<Vec<u64>> {
        Graph::label_bitmap(self, label)
    }

    fn self_loops(&self) -> Option<usize> {
        Some(Graph::self_loop_count(self))
    }

    fn count_nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: ColumnCmp,
        bound: &ColumnBound,
    ) -> Option<usize> {
        Graph::count_nodes_where(self, label, prop, cmp, bound)
    }

    fn nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: ColumnCmp,
        bound: &ColumnBound,
    ) -> Option<Vec<NodeId>> {
        Graph::nodes_where(self, label, prop, cmp, bound)
    }

    fn strings(&self) -> &StringTable {
        Graph::strings(self)
    }

    fn statistics(&self) -> Option<&GraphStatistics> {
        Graph::statistics(self)
    }

    fn create_node(&mut self, labels: &[&str], props: PropertyMap) -> Result<NodeId> {
        Ok(Graph::create_node(self, labels, props))
    }

    fn create_edge(
        &mut self,
        src: NodeId,
        tgt: NodeId,
        edge_type: &str,
        props: PropertyMap,
    ) -> Result<EdgeId> {
        Graph::create_edge(self, src, tgt, edge_type, props)
    }

    fn create_node_with_id(
        &mut self,
        id: NodeId,
        labels: &[&str],
        props: PropertyMap,
    ) -> Result<()> {
        Graph::create_node_with_id(self, id, labels, props);
        Ok(())
    }

    fn create_edge_with_id(
        &mut self,
        id: EdgeId,
        src: NodeId,
        tgt: NodeId,
        edge_type: &str,
        props: PropertyMap,
    ) -> Result<()> {
        Graph::create_edge_with_id(self, id, src, tgt, edge_type, props)?;
        Ok(())
    }

    fn update_node_properties(&mut self, id: NodeId, props: PropertyMap) -> Result<()> {
        Graph::update_node_properties(self, id, props)
    }

    fn update_edge_properties(&mut self, id: EdgeId, props: PropertyMap) -> Result<()> {
        Graph::update_edge_properties(self, id, props)
    }

    fn delete_node(&mut self, id: NodeId) -> Result<()> {
        Graph::delete_node(self, id)
    }

    fn delete_node_cascading(&mut self, id: NodeId) -> Result<()> {
        Graph::delete_node_cascading(self, id)
    }

    fn delete_edge(&mut self, id: EdgeId) -> Result<()> {
        Graph::delete_edge(self, id)
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        Graph::add_label(self, id, label)
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        Graph::remove_label(self, id, label)
    }

    fn get_node_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        Graph::get_node_mut(self, id)
    }

    fn get_edge_mut(&mut self, id: EdgeId) -> Result<&mut Edge> {
        Graph::get_edge_mut(self, id)
    }

    fn unindex_node_prop(&mut self, id: NodeId, key: &str) {
        // Fast path for single-property SET/REMOVE: only (label, key) b-tree entries can
        // change. Full bracket when anything broader may reference the property
        // (specialized or composite indexes) or trigger capture needs the full-map diff.
        if !self.targeted_node_prop_ok() {
            return GraphView::unindex_node(self, id);
        }
        let Some(pkid) = self.strings.get(key).map(PropertyKeyId) else {
            return; // key never interned: no index uses it
        };
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let Some(ik) = node
            .properties
            .get(pkid)
            .and_then(|v| IndexKey::from_property_value(&v))
        else {
            return;
        };
        let labels: SmallVec<[LabelId; 2]> = node.labels.clone();
        for lid in &labels {
            if let Some(idx) = self.indexes.get_index_mut(*lid, pkid) {
                idx.remove(&ik, id);
            }
        }
    }

    fn index_node_prop(&mut self, id: NodeId, key: &str) {
        if !self.targeted_node_prop_ok() {
            return GraphView::index_node(self, id);
        }
        let Some(pkid) = self.strings.get(key).map(PropertyKeyId) else {
            return;
        };
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let i = id.0 as usize;
        self.columns.refresh_key(
            Entity::Node,
            i,
            pkid,
            &node.properties,
            i < self.nodes.prefix_len(),
        );
        let Some(ik) = node
            .properties
            .get(pkid)
            .and_then(|v| IndexKey::from_property_value(&v))
        else {
            return;
        };
        let labels: SmallVec<[LabelId; 2]> = node.labels.clone();
        for lid in &labels {
            if let Some(idx) = self.indexes.get_index_mut(*lid, pkid) {
                idx.insert(ik.clone(), id);
            }
        }
    }

    fn unindex_edge_prop(&mut self, id: EdgeId, key: &str) {
        if !self.targeted_edge_prop_ok() {
            return GraphView::unindex_edge(self, id);
        }
        let Some(pkid) = self.strings.get(key).map(PropertyKeyId) else {
            return;
        };
        let Some(edge) = self.edges.get(&id) else {
            return;
        };
        let type_id = edge.edge_type;
        let Some(ik) = edge
            .properties
            .get(pkid)
            .and_then(|v| IndexKey::from_property_value(&v))
        else {
            return;
        };
        if let Some(idx) = self.edge_indexes.get_prop_index_mut(type_id, pkid) {
            idx.remove(&ik, id);
        }
    }

    fn index_edge_prop(&mut self, id: EdgeId, key: &str) {
        if !self.targeted_edge_prop_ok() {
            return GraphView::index_edge(self, id);
        }
        let Some(pkid) = self.strings.get(key).map(PropertyKeyId) else {
            return;
        };
        let Some(edge) = self.edges.get(&id) else {
            return;
        };
        let type_id = edge.edge_type;
        let i = id.0 as usize;
        self.columns.refresh_key(
            Entity::Edge,
            i,
            pkid,
            &edge.properties,
            i < self.edge_prefix(),
        );
        let Some(ik) = edge
            .properties
            .get(pkid)
            .and_then(|v| IndexKey::from_property_value(&v))
        else {
            return;
        };
        if let Some(idx) = self.edge_indexes.get_prop_index_mut(type_id, pkid) {
            idx.insert(ik, id);
        }
    }

    fn unindex_node(&mut self, id: NodeId) {
        // unindex -> mutate -> index bracket wraps every in-place property write:
        // snapshot here, diff at index_node. Gives triggers full property-event coverage.
        if events::capturing()
            && let Some(n) = self.nodes.get(&id)
        {
            events::snapshot_node(
                id,
                n.properties
                    .iter()
                    .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v.clone()))
                    .collect(),
            );
        }
        self.remove_node_from_indexes(id);
    }

    fn index_node(&mut self, id: NodeId) {
        self.add_node_to_indexes(id);
        if events::capturing()
            && let Some(n) = self.nodes.get(&id)
        {
            events::diff_node(
                id,
                n.properties
                    .iter()
                    .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v.clone()))
                    .collect(),
            );
        }
    }

    fn unindex_edge(&mut self, id: EdgeId) {
        if events::capturing()
            && let Some(e) = self.edges.get(&id)
        {
            events::snapshot_edge(
                id,
                e.properties
                    .iter()
                    .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v.clone()))
                    .collect(),
            );
        }
        if let Some(type_id) = self.edges.get(&id).map(|e| e.edge_type) {
            self.remove_edge_from_indexes(id, type_id);
        }
    }

    fn index_edge(&mut self, id: EdgeId) {
        if let Some(type_id) = self.edges.get(&id).map(|e| e.edge_type) {
            self.add_edge_to_indexes(id, type_id);
        }
        if events::capturing()
            && let Some(e) = self.edges.get(&id)
        {
            events::diff_edge(
                id,
                e.properties
                    .iter()
                    .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v.clone()))
                    .collect(),
            );
        }
    }

    fn create_index(&mut self, name: &str, label: &str, prop: &str) -> Result<()> {
        Graph::create_index(self, name, label, prop)
    }

    fn create_composite_index(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        is_relationship: bool,
    ) -> Result<()> {
        Graph::create_composite_index(self, name, label, properties, is_relationship)
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        Graph::drop_index(self, name)
    }

    fn index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        Graph::index_lookup(self, label, prop, value)
    }

    fn property_seek(&self, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        Graph::property_seek(self, prop, value)
    }

    fn property_seek_covers(&self, prop: &str) -> bool {
        Graph::property_seek_covers(self, prop)
    }

    fn index_range_lookup(
        &self,
        label: &str,
        prop: &str,
        lower: std::ops::Bound<IndexKey>,
        upper: std::ops::Bound<IndexKey>,
    ) -> Option<Vec<NodeId>> {
        Graph::index_range_lookup(self, label, prop, lower.as_ref(), upper.as_ref())
    }

    fn index_catalog(&self) -> &IndexCatalog {
        Graph::index_catalog(self)
    }

    fn index_size(&self, name: &str) -> Option<usize> {
        Graph::index_size(self, name)
    }

    fn create_vector_index(
        &mut self,
        name: &str,
        label: &str,
        property: &str,
        dim: usize,
        sim: crate::graph::vector::Similarity,
        params: crate::graph::vector::HnswParams,
    ) -> Result<()> {
        Graph::create_vector_index(self, name, label, property, dim, sim, params)
    }

    fn drop_vector_index(&mut self, name: &str) -> Result<()> {
        Graph::drop_vector_index(self, name)
    }

    fn has_vector_index(&self, name: &str) -> bool {
        Graph::has_vector_index(self, name)
    }

    fn query_vector_index(
        &self,
        name: &str,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Option<Vec<(NodeId, f64)>> {
        Graph::query_vector_index(self, name, query, k, ef)
    }

    fn vector_index_descriptors(&self) -> Vec<(String, String, String, usize, &'static str)> {
        Graph::vector_index_descriptors(self)
    }

    fn create_fulltext_index(
        &mut self,
        name: &str,
        labels: &[&str],
        properties: &[&str],
        analyzer: crate::graph::fulltext::Analyzer,
    ) -> Result<()> {
        Graph::create_fulltext_index(self, name, labels, properties, analyzer)
    }

    fn drop_fulltext_index(&mut self, name: &str) -> Result<()> {
        Graph::drop_fulltext_index(self, name)
    }

    fn has_fulltext_index(&self, name: &str) -> bool {
        Graph::has_fulltext_index(self, name)
    }

    fn query_fulltext_index(
        &self,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Option<Vec<(NodeId, f64)>> {
        Graph::query_fulltext_index(self, name, query, limit)
    }

    fn fulltext_index_descriptors(&self) -> Vec<(String, Vec<String>, Vec<String>, String)> {
        Graph::fulltext_index_descriptors(self)
    }

    fn create_point_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        Graph::create_point_index(self, name, label, property)
    }

    fn drop_point_index(&mut self, name: &str) -> Result<()> {
        Graph::drop_point_index(self, name)
    }

    fn has_point_index(&self, name: &str) -> bool {
        Graph::has_point_index(self, name)
    }

    fn has_point_index_for(&self, label: &str, property: &str) -> bool {
        Graph::has_point_index_for(self, label, property)
    }

    fn point_index_within_bbox(
        &self,
        label: &str,
        property: &str,
        ll: [f64; 3],
        ur: [f64; 3],
        srid: i64,
    ) -> Option<Vec<NodeId>> {
        Graph::point_index_within_bbox(self, label, property, ll, ur, srid)
    }

    fn point_index_within_distance(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        radius: f64,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        Graph::point_index_within_distance(self, label, property, center, radius, srid)
    }

    fn point_index_knn(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        k: usize,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        Graph::point_index_knn(self, label, property, center, k, srid)
    }

    fn point_index_descriptors(&self) -> Vec<(String, String, String)> {
        Graph::point_index_descriptors(self)
    }

    fn create_text_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        Graph::create_text_index(self, name, label, property)
    }

    fn drop_text_index(&mut self, name: &str) -> Result<()> {
        Graph::drop_text_index(self, name)
    }

    fn has_text_index(&self, name: &str) -> bool {
        Graph::has_text_index(self, name)
    }

    fn has_text_index_for(&self, label: &str, property: &str) -> bool {
        Graph::has_text_index_for(self, label, property)
    }

    fn text_index_candidates(
        &self,
        label: &str,
        property: &str,
        kind: crate::graph::text::TextQueryKind,
        pattern: &str,
    ) -> Option<Vec<NodeId>> {
        Graph::text_index_candidates(self, label, property, kind, pattern)
    }

    fn text_index_descriptors(&self) -> Vec<(String, String, String)> {
        Graph::text_index_descriptors(self)
    }

    fn create_composite_constraint(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        ctype: ConstraintType,
        is_relationship: bool,
    ) -> Result<()> {
        Graph::create_composite_constraint(self, name, label, properties, ctype, is_relationship)
    }

    fn intern_property_key(&mut self, key: &str) -> PropertyKeyId {
        Graph::intern_property_key(self, key)
    }

    fn analyze_statistics(&mut self, labels: &[String]) {
        let existing = self.stats.as_deref();
        let new_stats = stats::collect_statistics(self, labels, existing);
        self.stats = Some(Arc::new(new_stats));
    }

    fn create_constraint(
        &mut self,
        name: &str,
        label: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        Graph::create_constraint(self, name, label, prop, ctype)
    }

    fn create_relationship_constraint(
        &mut self,
        name: &str,
        edge_type: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        Graph::create_relationship_constraint(self, name, edge_type, prop, ctype)
    }

    fn drop_constraint(&mut self, name: &str) -> Result<()> {
        Graph::drop_constraint(self, name)
    }

    fn constraint_registry(&self) -> &ConstraintRegistry {
        &self.constraints
    }

    fn create_edge_index(&mut self, name: &str, edge_type: &str, prop: Option<&str>) -> Result<()> {
        Graph::create_edge_index(self, name, edge_type, prop)
    }

    fn drop_edge_index(&mut self, name: &str) -> Result<()> {
        Graph::drop_edge_index(self, name)
    }

    fn edge_index_lookup(
        &self,
        edge_type: &str,
        prop: &str,
        value: &IndexKey,
    ) -> Option<Vec<EdgeId>> {
        Graph::edge_index_lookup(self, edge_type, prop, value)
    }

    fn edge_index_catalog(&self) -> &EdgeIndexCatalog {
        Graph::edge_index_catalog(self)
    }
}
