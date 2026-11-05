//! Property indexes on nodes and relationships (single and composite) and their
//! maintenance.

use smallvec::SmallVec;

use crate::error::{Error, Result};

use super::columns::Entity;
use super::index::{
    EdgeIndexCatalog, EdgeIndexDescriptor, EdgeIndexRegistry, IndexCatalog, IndexDescriptor,
    IndexRegistry,
};
use super::parts::COMPOSITE_PROP_SEP;
use super::property::{IndexKey, InternedPropertyMap, PropertyValue};
use super::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use super::{Graph, events};

impl Graph {
    /// Creates a named node property index on `label.property_key`, filled from existing nodes.
    pub fn create_index(&mut self, name: &str, label: &str, property_key: &str) -> Result<()> {
        let lid = LabelId(self.strings.intern(label));
        let pkid = PropertyKeyId(self.strings.intern(property_key));
        self.strings_dirty = true;
        let descriptor = IndexDescriptor {
            label_id: lid,
            property_key: pkid,
            name: name.to_string(),
        };
        self.indexes.create_index(descriptor)?;

        // the keys of the label's nodes, read on every core and sorted; then the tree in one go
        use rayon::prelude::*;
        let node_ids: Vec<NodeId> = self
            .label_index
            .get(&lid)
            .map(|set| set.to_vec())
            .unwrap_or_default();
        let mut entries: Vec<(IndexKey, NodeId)> = node_ids
            .par_iter()
            .filter_map(|&nid| {
                let node = self.nodes.get(&nid)?;
                let pv = node.properties.get(pkid)?;
                IndexKey::from_property_value(&pv).map(|key| (key, nid))
            })
            .collect();
        entries.par_sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        if let Some(idx) = self.indexes.get_index_mut(lid, pkid) {
            idx.extend_sorted(entries);
        }
        Ok(())
    }

    /// Composite (multi-property) b-tree index, stored as a plain
    /// [`PropertyIndex`](index::PropertyIndex)
    /// under a synthetic key (component names joined with `COMPOSITE_PROP_SEP`),
    /// so persistence, SHOW/DROP INDEX and lookups reuse the single-property code.
    /// Keys use [`IndexKey::composite_encode`]; entities missing any component (or
    /// with a null / non-scalar one) aren't indexed, as in Neo4j.
    pub fn create_composite_index(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        is_relationship: bool,
    ) -> Result<()> {
        if properties.len() < 2 {
            return Err(Error::IndexError(
                "composite indexes need at least two properties".to_string(),
            ));
        }
        if properties.iter().any(|p| p.contains(COMPOSITE_PROP_SEP)) {
            return Err(Error::IndexError(
                "property names may not contain the reserved separator".to_string(),
            ));
        }
        let synthetic = properties.join(&COMPOSITE_PROP_SEP.to_string());
        let comp_ids: Vec<PropertyKeyId> = properties
            .iter()
            .map(|p| PropertyKeyId(self.strings.intern(p)))
            .collect();
        self.strings_dirty = true;
        if is_relationship {
            let etid = EdgeTypeId(self.strings.intern(label));
            let pkid = PropertyKeyId(self.strings.intern(&synthetic));
            self.edge_indexes.create_index(EdgeIndexDescriptor {
                edge_type_id: etid,
                property_key: Some(pkid),
                name: name.to_string(),
            })?;
            let edge_ids: Vec<EdgeId> = self
                .edge_type_index
                .get(&etid)
                .map(|s| s.to_vec())
                .unwrap_or_default();
            for eid in edge_ids {
                let Some(edge) = self.edges.get(&eid) else {
                    continue;
                };
                let Some(key) = Self::composite_key_of(&edge.properties, &comp_ids) else {
                    continue;
                };
                if let Some(idx) = self.edge_indexes.get_prop_index_mut(etid, pkid) {
                    idx.insert(key, eid);
                }
            }
        } else {
            let lid = LabelId(self.strings.intern(label));
            let pkid = PropertyKeyId(self.strings.intern(&synthetic));
            self.indexes.create_index(IndexDescriptor {
                label_id: lid,
                property_key: pkid,
                name: name.to_string(),
            })?;
            let node_ids: Vec<NodeId> = self
                .label_index
                .get(&lid)
                .map(|s| s.to_vec())
                .unwrap_or_default();
            for nid in node_ids {
                let Some(node) = self.nodes.get(&nid) else {
                    continue;
                };
                let Some(key) = Self::composite_key_of(&node.properties, &comp_ids) else {
                    continue;
                };
                if let Some(idx) = self.indexes.get_index_mut(lid, pkid) {
                    idx.insert(key, nid);
                }
            }
        }
        Ok(())
    }

    /// Composite tuple key of an entity; `None` if any component is missing, null or non-scalar.
    pub(crate) fn composite_key_of(
        props: &InternedPropertyMap,
        components: &[PropertyKeyId],
    ) -> Option<IndexKey> {
        let mut parts = Vec::with_capacity(components.len());
        for pk in components {
            let pv = props.get(*pk)?;
            if matches!(pv, PropertyValue::Null) {
                return None;
            }
            parts.push(IndexKey::from_property_value(&pv)?);
        }
        Some(IndexKey::composite_encode(&parts))
    }

    /// Cheap pre-check: is ANY node index composite? Maintenance clones the full
    /// property map before [`Self::composite_node_entries`] on every property write,
    /// so that's skipped when there are none (the common case).
    pub(super) fn has_composite_node_indexes(&self) -> bool {
        self.indexes.descriptors().iter().any(|d| {
            self.strings
                .try_resolve(d.property_key.0)
                .is_some_and(|n| n.contains(COMPOSITE_PROP_SEP))
        })
    }

    /// Whether the targeted single-property maintenance fast path is safe for nodes:
    /// no fulltext/text/spatial/vector index, no composite index, no trigger capture
    /// (needs the full-map diff). Any doubt -> full bracket.
    pub(super) fn targeted_node_prop_ok(&self) -> bool {
        !events::capturing()
            && self.fulltext_indexes.is_empty()
            && self.text_indexes.is_empty()
            && self.spatial_indexes.is_empty()
            && self.vector_indexes.is_empty()
            && !self.has_composite_node_indexes()
    }

    /// Edge analog of [`Self::targeted_node_prop_ok`].
    pub(super) fn targeted_edge_prop_ok(&self) -> bool {
        !events::capturing()
            && self.fulltext_indexes.is_empty()
            && self.text_indexes.is_empty()
            && self.spatial_indexes.is_empty()
            && self.vector_indexes.is_empty()
            && !self.has_composite_edge_indexes()
    }

    /// Edge analog of [`Self::has_composite_node_indexes`].
    pub(super) fn has_composite_edge_indexes(&self) -> bool {
        self.edge_indexes.iter_prop().any(|((_, pkid), _)| {
            self.strings
                .try_resolve(pkid.0)
                .is_some_and(|n| n.contains(COMPOSITE_PROP_SEP))
        })
    }

    /// Composite node-index entries for one entity: each composite descriptor on one
    /// of `labels` whose full tuple is present. Composite descriptors are spotted by
    /// the separator in their synthetic property name.
    pub(super) fn composite_node_entries(
        &self,
        labels: &[LabelId],
        props: &InternedPropertyMap,
    ) -> Vec<(LabelId, PropertyKeyId, IndexKey)> {
        let mut out = Vec::new();
        for d in self.indexes.descriptors() {
            if !labels.contains(&d.label_id) {
                continue;
            }
            let Some(name) = self.strings.try_resolve(d.property_key.0) else {
                continue;
            };
            if !name.contains(COMPOSITE_PROP_SEP) {
                continue;
            }
            let comp_ids: Option<Vec<PropertyKeyId>> = name
                .split(COMPOSITE_PROP_SEP)
                .map(|part| self.strings.get(part).map(PropertyKeyId))
                .collect();
            let Some(comp_ids) = comp_ids else { continue };
            if let Some(key) = Self::composite_key_of(props, &comp_ids) {
                out.push((d.label_id, d.property_key, key));
            }
        }
        out
    }

    /// Composite edge-index entries, like [`Self::composite_node_entries`].
    pub(super) fn composite_edge_entries(
        &self,
        type_id: EdgeTypeId,
        props: &InternedPropertyMap,
    ) -> Vec<(PropertyKeyId, IndexKey)> {
        let mut out = Vec::new();
        for ((etid, pkid), _idx) in self.edge_indexes.iter_prop() {
            let (etid, pkid) = (*etid, *pkid);
            if etid != type_id {
                continue;
            }
            let Some(name) = self.strings.try_resolve(pkid.0) else {
                continue;
            };
            if !name.contains(COMPOSITE_PROP_SEP) {
                continue;
            }
            let comp_ids: Option<Vec<PropertyKeyId>> = name
                .split(COMPOSITE_PROP_SEP)
                .map(|part| self.strings.get(part).map(PropertyKeyId))
                .collect();
            let Some(comp_ids) = comp_ids else { continue };
            if let Some(key) = Self::composite_key_of(props, &comp_ids) {
                out.push((pkid, key));
            }
        }
        out
    }

    /// Drops a named property index.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        self.indexes.drop_index(name)
    }

    /// Node IDs via the index on `label.property_key`; `None` if there's no such index.
    pub fn index_lookup(
        &self,
        label: &str,
        property_key: &str,
        value: &IndexKey,
    ) -> Option<Vec<NodeId>> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(property_key)?);
        let idx = self.indexes.get_index(lid, pkid)?;
        Some(
            idx.lookup(value)
                .map(|ids| ids.to_vec())
                .unwrap_or_default(),
        )
    }

    /// Nodes, whatever their labels, whose `property_key` is `value`, through the label indexes on
    /// that property: `None` unless they cover every node (see
    /// [`property_seek_covers`](Self::property_seek_covers)), so a label-less `{key: value}` lookup
    /// needn't scan the graph.
    pub fn property_seek(&self, property_key: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        if self.unlabeled > 0 {
            return None;
        }
        // a key no node has ever used: nothing has it
        let Some(raw) = self.strings.get(property_key) else {
            return Some(Vec::new());
        };
        let pkid = PropertyKeyId(raw);
        let mut out = Vec::new();
        let mut sources = 0;
        for (lid, nodes) in &self.label_index {
            if nodes.is_empty() {
                continue;
            }
            if let Some(ids) = self.indexes.get_index(*lid, pkid)?.lookup(value) {
                out.extend_from_slice(ids);
            }
            sources += 1;
        }
        // a node with two indexed labels comes from both
        if sources > 1 {
            out.sort_unstable();
            out.dedup();
        }
        Some(out)
    }

    /// Whether the label indexes on `property_key` cover every node: none is without a label and
    /// every label in use has an index on it.
    pub fn property_seek_covers(&self, property_key: &str) -> bool {
        if self.unlabeled > 0 {
            return false;
        }
        let Some(raw) = self.strings.get(property_key) else {
            return true;
        };
        let pkid = PropertyKeyId(raw);
        self.label_index
            .iter()
            .all(|(lid, nodes)| nodes.is_empty() || self.indexes.has_index(*lid, pkid))
    }

    /// Looks up node IDs within a key range via a property index.
    pub fn index_range_lookup(
        &self,
        label: &str,
        property_key: &str,
        lower: std::ops::Bound<&IndexKey>,
        upper: std::ops::Bound<&IndexKey>,
    ) -> Option<Vec<NodeId>> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(property_key)?);
        let idx = self.indexes.get_index(lid, pkid)?;
        Some(idx.range_lookup(lower, upper))
    }

    /// Returns a reference to the index registry.
    pub fn index_registry(&self) -> &IndexRegistry {
        &self.indexes
    }

    /// Node-index metadata.
    pub fn index_catalog(&self) -> &IndexCatalog {
        self.indexes.catalog()
    }

    /// Entries in the node or edge property index `name`.
    pub fn index_size(&self, name: &str) -> Option<usize> {
        if let Some(d) = self.indexes.catalog().get_descriptor_by_name(name) {
            return self
                .indexes
                .get_index(d.label_id, d.property_key)
                .map(|i| i.total_entries());
        }
        let d = self.edge_indexes.catalog().get_descriptor_by_name(name)?;
        match d.property_key {
            Some(pk) => self
                .edge_indexes
                .get_prop_index(d.edge_type_id, pk)
                .map(|i| i.total_entries()),
            None => Some(
                self.edge_type_index
                    .get(&d.edge_type_id)
                    .map_or(0, |s| s.len()),
            ),
        }
    }

    // -- Index maintenance helpers ------------------------------------

    /// Adds index entries for a node: all labels x all properties.
    pub(super) fn add_node_to_indexes(&mut self, node_id: NodeId) {
        let node = match self.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };
        let i = node_id.0 as usize;
        self.columns.refresh(
            Entity::Node,
            i,
            &node.properties,
            i < self.nodes.prefix_len(),
        );
        let labels: SmallVec<[LabelId; 2]> = node.labels.clone();
        let props: Vec<(PropertyKeyId, IndexKey)> = node
            .properties
            .iter()
            .filter_map(|(k, v)| IndexKey::from_property_value(&v).map(|ik| (k, ik)))
            .collect();

        for lid in &labels {
            for (prop_kid, idx_key) in &props {
                if let Some(idx) = self.indexes.get_index_mut(*lid, *prop_kid) {
                    idx.insert(idx_key.clone(), node_id);
                }
            }
        }
        // Composite (multi-property) index entries.
        if let Some(node) = self
            .nodes
            .get(&node_id)
            .filter(|_| self.has_composite_node_indexes())
        {
            let node_props = node.properties.clone();
            for (lid, pkid, key) in self.composite_node_entries(&labels, &node_props) {
                if let Some(idx) = self.indexes.get_index_mut(lid, pkid) {
                    idx.insert(key, node_id);
                }
            }
        }
        // vector indexes keep their own HNSW storage keyed by name, not this b-tree map
        self.maintain_vector_indexes_for_node(node_id);
        self.maintain_fulltext_indexes_for_node(node_id);
        self.maintain_spatial_indexes_for_node(node_id);
        self.maintain_text_indexes_for_node(node_id);
    }

    /// Removes index entries for a node: all labels x all properties.
    pub(super) fn remove_node_from_indexes(&mut self, node_id: NodeId) {
        let node = match self.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };
        let labels: SmallVec<[LabelId; 2]> = node.labels.clone();
        let props: Vec<(PropertyKeyId, IndexKey)> = node
            .properties
            .iter()
            .filter_map(|(k, v)| IndexKey::from_property_value(&v).map(|ik| (k, ik)))
            .collect();

        for lid in &labels {
            for (prop_kid, idx_key) in &props {
                if let Some(idx) = self.indexes.get_index_mut(*lid, *prop_kid) {
                    idx.remove(idx_key, node_id);
                }
            }
        }
        // Composite (multi-property) index entries.
        if let Some(node) = self
            .nodes
            .get(&node_id)
            .filter(|_| self.has_composite_node_indexes())
        {
            let node_props = node.properties.clone();
            for (lid, pkid, key) in self.composite_node_entries(&labels, &node_props) {
                if let Some(idx) = self.indexes.get_index_mut(lid, pkid) {
                    idx.remove(&key, node_id);
                }
            }
        }
        self.remove_from_vector_indexes(node_id);
        self.remove_from_fulltext_indexes(node_id);
        self.remove_from_spatial_indexes(node_id);
        self.remove_from_text_indexes(node_id);
    }

    /// Index entries for one label (label added to a node).
    pub(super) fn add_node_to_indexes_for_label(&mut self, node_id: NodeId, label_id: LabelId) {
        let node = match self.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };
        let props: Vec<(PropertyKeyId, IndexKey)> = node
            .properties
            .iter()
            .filter_map(|(k, v)| IndexKey::from_property_value(&v).map(|ik| (k, ik)))
            .collect();

        for (prop_kid, idx_key) in &props {
            if let Some(idx) = self.indexes.get_index_mut(label_id, *prop_kid) {
                idx.insert(idx_key.clone(), node_id);
            }
        }
        if let Some(node) = self
            .nodes
            .get(&node_id)
            .filter(|_| self.has_composite_node_indexes())
        {
            let node_props = node.properties.clone();
            for (lid, pkid, key) in self.composite_node_entries(&[label_id], &node_props) {
                if let Some(idx) = self.indexes.get_index_mut(lid, pkid) {
                    idx.insert(key, node_id);
                }
            }
        }
    }

    /// Removes index entries for one label (label removed from a node).
    pub(super) fn remove_node_from_indexes_for_label(
        &mut self,
        node_id: NodeId,
        label_id: LabelId,
    ) {
        let node = match self.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };
        let props: Vec<(PropertyKeyId, IndexKey)> = node
            .properties
            .iter()
            .filter_map(|(k, v)| IndexKey::from_property_value(&v).map(|ik| (k, ik)))
            .collect();

        for (prop_kid, idx_key) in &props {
            if let Some(idx) = self.indexes.get_index_mut(label_id, *prop_kid) {
                idx.remove(idx_key, node_id);
            }
        }
        if let Some(node) = self
            .nodes
            .get(&node_id)
            .filter(|_| self.has_composite_node_indexes())
        {
            let node_props = node.properties.clone();
            for (lid, pkid, key) in self.composite_node_entries(&[label_id], &node_props) {
                if let Some(idx) = self.indexes.get_index_mut(lid, pkid) {
                    idx.remove(&key, node_id);
                }
            }
        }
    }

    // -- Edge Property Indexes ------------------------------------------

    /// Creates a named edge index. With `property_key`: a property index filled from
    /// existing edges of the type. Without: type-only, data lives in `edge_type_index`.
    pub fn create_edge_index(
        &mut self,
        name: &str,
        edge_type: &str,
        property_key: Option<&str>,
    ) -> Result<()> {
        let etid = EdgeTypeId(self.strings.intern(edge_type));
        let pkid = property_key.map(|pk| PropertyKeyId(self.strings.intern(pk)));
        self.strings_dirty = true;
        let descriptor = EdgeIndexDescriptor {
            edge_type_id: etid,
            property_key: pkid,
            name: name.to_string(),
        };
        self.edge_indexes.create_index(descriptor)?;

        // property index: populate from existing edges
        if let Some(pkid) = pkid {
            let edge_ids: Vec<EdgeId> = self
                .edge_type_index
                .get(&etid)
                .map(|set| set.to_vec())
                .unwrap_or_default();
            for eid in edge_ids {
                if let Some(edge) = self.edges.get(&eid)
                    && let Some(pv) = edge.properties.get(pkid)
                    && let Some(key) = IndexKey::from_property_value(&pv)
                    && let Some(idx) = self.edge_indexes.get_prop_index_mut(etid, pkid)
                {
                    idx.insert(key, eid);
                }
                // propless edges have no properties
            }
        }
        Ok(())
    }

    /// Drops a named edge index.
    pub fn drop_edge_index(&mut self, name: &str) -> Result<()> {
        self.edge_indexes.drop_index(name)
    }

    /// Looks up edge IDs via a property index on edges.
    pub fn edge_index_lookup(
        &self,
        edge_type: &str,
        property_key: &str,
        value: &IndexKey,
    ) -> Option<Vec<EdgeId>> {
        let etid = EdgeTypeId(self.strings.get(edge_type)?);
        let pkid = PropertyKeyId(self.strings.get(property_key)?);
        let idx = self.edge_indexes.get_prop_index(etid, pkid)?;
        Some(
            idx.lookup(value)
                .map(|ids| ids.to_vec())
                .unwrap_or_default(),
        )
    }

    /// Returns a reference to the edge index registry.
    pub fn edge_index_registry(&self) -> &EdgeIndexRegistry {
        &self.edge_indexes
    }

    /// Edge-index metadata.
    pub fn edge_index_catalog(&self) -> &EdgeIndexCatalog {
        self.edge_indexes.catalog()
    }

    // -- Edge index maintenance helpers ------------------------------

    /// Add matching edge property index entries for an edge.
    pub(super) fn add_edge_to_indexes(&mut self, edge_id: EdgeId, type_id: EdgeTypeId) {
        let i = edge_id.0 as usize;
        let in_prefix = i < self.edge_prefix();
        match self.edges.get(&edge_id) {
            Some(e) => self
                .columns
                .refresh(Entity::Edge, i, &e.properties, in_prefix),
            None => {
                self.columns
                    .refresh(Entity::Edge, i, &InternedPropertyMap::default(), in_prefix)
            }
        }
        if self.edge_indexes.iter_prop().next().is_none() {
            return; // no property indexes
        }
        let props: Vec<(PropertyKeyId, IndexKey)> = if let Some(edge) = self.edges.get(&edge_id) {
            edge.properties
                .iter()
                .filter_map(|(k, v)| IndexKey::from_property_value(&v).map(|ik| (k, ik)))
                .collect()
        } else {
            return; // propless: nothing to index
        };

        for (prop_kid, idx_key) in &props {
            if let Some(idx) = self.edge_indexes.get_prop_index_mut(type_id, *prop_kid) {
                idx.insert(idx_key.clone(), edge_id);
            }
        }
        if let Some(edge) = self
            .edges
            .get(&edge_id)
            .filter(|_| self.has_composite_edge_indexes())
        {
            let edge_props = edge.properties.clone();
            for (pkid, key) in self.composite_edge_entries(type_id, &edge_props) {
                if let Some(idx) = self.edge_indexes.get_prop_index_mut(type_id, pkid) {
                    idx.insert(key, edge_id);
                }
            }
        }
    }

    /// Remove matching edge property index entries for an edge.
    pub(super) fn remove_edge_from_indexes(&mut self, edge_id: EdgeId, type_id: EdgeTypeId) {
        if self.edge_indexes.iter_prop().next().is_none() {
            return; // no property indexes
        }
        let props: Vec<(PropertyKeyId, IndexKey)> = if let Some(edge) = self.edges.get(&edge_id) {
            edge.properties
                .iter()
                .filter_map(|(k, v)| IndexKey::from_property_value(&v).map(|ik| (k, ik)))
                .collect()
        } else {
            return; // propless: nothing to index
        };

        for (prop_kid, idx_key) in &props {
            if let Some(idx) = self.edge_indexes.get_prop_index_mut(type_id, *prop_kid) {
                idx.remove(idx_key, edge_id);
            }
        }
        if let Some(edge) = self
            .edges
            .get(&edge_id)
            .filter(|_| self.has_composite_edge_indexes())
        {
            let edge_props = edge.properties.clone();
            for (pkid, key) in self.composite_edge_entries(type_id, &edge_props) {
                if let Some(idx) = self.edge_indexes.get_prop_index_mut(type_id, pkid) {
                    idx.remove(&key, edge_id);
                }
            }
        }
    }
}
