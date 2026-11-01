//! B-tree property indexes for node and edge lookups.
//!
//! [`IndexRegistry`] holds named node property indexes, each a [`PropertyIndex`]
//! mapping [`IndexKey`] -> [`NodeId`]s. [`EdgeIndexRegistry`] holds edge indexes
//! (property and type-only), property ones backed by [`EdgePropertyIndex`].

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use smallvec::SmallVec;

use super::property::IndexKey;
use super::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use crate::error::{Error, Result};

/// Property index metadata.
#[derive(Debug, Clone)]
pub struct IndexDescriptor {
    /// Label nodes must have to be indexed.
    pub label_id: LabelId,
    /// Indexed property key (interned).
    pub property_key: PropertyKeyId,
    /// User-visible index name.
    pub name: String,
}

/// One B-tree property index.
#[derive(Debug, Clone)]
pub struct PropertyIndex {
    /// Descriptor for this index.
    pub descriptor: IndexDescriptor,
    tree: BTreeMap<IndexKey, SmallVec<[NodeId; 1]>>,
}

impl PropertyIndex {
    /// Empty index.
    pub fn new(descriptor: IndexDescriptor) -> Self {
        Self {
            descriptor,
            tree: BTreeMap::new(),
        }
    }

    /// Adds `node_id` under `key`.
    pub fn insert(&mut self, key: IndexKey, node_id: NodeId) {
        self.tree.entry(key).or_default().push(node_id);
    }

    /// Removes `node_id` from `key`'s entry.
    pub fn remove(&mut self, key: &IndexKey, node_id: NodeId) {
        if let Some(ids) = self.tree.get_mut(key) {
            ids.retain(|id| *id != node_id);
            if ids.is_empty() {
                self.tree.remove(key);
            }
        }
    }

    /// Node IDs with exactly `key`.
    pub fn lookup(&self, key: &IndexKey) -> Option<&SmallVec<[NodeId; 1]>> {
        self.tree.get(key)
    }

    /// Distinct keys.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Index empty?
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Indexed node entries across all keys.
    pub fn total_entries(&self) -> usize {
        self.tree.values().map(|v| v.len()).sum()
    }

    /// Node IDs in a key range.
    pub fn range_lookup(&self, lower: Bound<&IndexKey>, upper: Bound<&IndexKey>) -> Vec<NodeId> {
        self.tree
            .range((lower, upper))
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect()
    }
}

/// Node-index metadata without the trees: what planners and SHOW INDEXES read. Replaced (never
/// mutated) on create/drop, so a reader's `Arc` stays valid and stable.
#[derive(Debug, Clone, Default)]
pub struct IndexCatalog {
    /// By name.
    descriptors: Vec<IndexDescriptor>,
    keys: FxHashSet<(LabelId, PropertyKeyId)>,
    generation: u64,
}

impl IndexCatalog {
    /// Bumped on each index change; part of the plan cache key.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// All index descriptors, by name.
    pub fn descriptors(&self) -> &[IndexDescriptor] {
        &self.descriptors
    }

    /// Index exists for `(label, property)`?
    pub fn has_index(&self, label_id: LabelId, property_key: PropertyKeyId) -> bool {
        self.keys.contains(&(label_id, property_key))
    }

    /// Index with this name exists?
    pub fn has_index_by_name(&self, name: &str) -> bool {
        self.get_descriptor_by_name(name).is_some()
    }

    /// Descriptor by name.
    pub fn get_descriptor_by_name(&self, name: &str) -> Option<&IndexDescriptor> {
        self.descriptors.iter().find(|d| d.name == name)
    }
}

/// All property indexes of a graph.
#[derive(Debug, Clone, Default)]
pub struct IndexRegistry {
    /// (label_id, property_key_id) -> index.
    indexes: FxHashMap<(LabelId, PropertyKeyId), PropertyIndex>,
    /// Index name -> (label_id, property_key_id).
    name_map: FxHashMap<String, (LabelId, PropertyKeyId)>,
    /// Metadata snapshot, rebuilt on every create/drop.
    catalog: Arc<IndexCatalog>,
}

impl IndexRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current generation (bumped on each index change).
    pub fn generation(&self) -> u64 {
        self.catalog.generation
    }

    /// Metadata snapshot.
    pub fn catalog(&self) -> &Arc<IndexCatalog> {
        &self.catalog
    }

    fn refresh_catalog(&mut self) {
        let mut descriptors: Vec<IndexDescriptor> = self
            .indexes
            .values()
            .map(|i| i.descriptor.clone())
            .collect();
        descriptors.sort_by(|a, b| a.name.cmp(&b.name));
        self.catalog = Arc::new(IndexCatalog {
            keys: self.indexes.keys().copied().collect(),
            descriptors,
            generation: self.catalog.generation + 1,
        });
    }

    /// Creates an index; errors if the name is taken.
    pub fn create_index(&mut self, descriptor: IndexDescriptor) -> Result<()> {
        let key = (descriptor.label_id, descriptor.property_key);
        if self.name_map.contains_key(&descriptor.name) {
            return Err(Error::IndexError(format!(
                "index '{}' already exists",
                descriptor.name
            )));
        }
        if self.indexes.contains_key(&key) {
            return Err(Error::IndexError(
                "an index already exists for that label/property combination".to_string(),
            ));
        }
        self.name_map.insert(descriptor.name.clone(), key);
        self.indexes.insert(key, PropertyIndex::new(descriptor));
        self.refresh_catalog();
        Ok(())
    }

    /// Drops an index by name; errors if missing.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let key = self
            .name_map
            .remove(name)
            .ok_or_else(|| Error::IndexError(format!("index '{name}' not found")))?;
        self.indexes.remove(&key);
        self.refresh_catalog();
        Ok(())
    }

    /// Index for `(label, property)`, if any.
    pub fn get_index(
        &self,
        label_id: LabelId,
        property_key: PropertyKeyId,
    ) -> Option<&PropertyIndex> {
        self.indexes.get(&(label_id, property_key))
    }

    /// Mutable index for `(label, property)`.
    pub fn get_index_mut(
        &mut self,
        label_id: LabelId,
        property_key: PropertyKeyId,
    ) -> Option<&mut PropertyIndex> {
        self.indexes.get_mut(&(label_id, property_key))
    }

    /// Index exists for `(label, property)`?
    pub fn has_index(&self, label_id: LabelId, property_key: PropertyKeyId) -> bool {
        self.indexes.contains_key(&(label_id, property_key))
    }

    /// Index with this name exists?
    pub fn has_index_by_name(&self, name: &str) -> bool {
        self.name_map.contains_key(name)
    }

    /// Descriptor by name.
    pub fn get_descriptor_by_name(&self, name: &str) -> Option<&IndexDescriptor> {
        let key = self.name_map.get(name)?;
        self.indexes.get(key).map(|idx| &idx.descriptor)
    }

    /// All index descriptors.
    pub fn descriptors(&self) -> Vec<&IndexDescriptor> {
        self.indexes.values().map(|idx| &idx.descriptor).collect()
    }

    /// All indexes.
    pub fn iter(&self) -> impl Iterator<Item = (&(LabelId, PropertyKeyId), &PropertyIndex)> {
        self.indexes.iter()
    }

    /// All indexes, mutable.
    pub fn iter_mut(
        &mut self,
    ) -> impl Iterator<Item = (&(LabelId, PropertyKeyId), &mut PropertyIndex)> {
        self.indexes.iter_mut()
    }
}

// -- Edge Indexes ------------------------------------------------------

/// Edge index metadata (property or type-only).
#[derive(Debug, Clone)]
pub struct EdgeIndexDescriptor {
    /// Edge type ID.
    pub edge_type_id: EdgeTypeId,
    /// Indexed property key; `None` for type-only.
    pub property_key: Option<PropertyKeyId>,
    /// User-visible index name.
    pub name: String,
}

/// One B-tree edge property index.
#[derive(Debug, Clone)]
pub struct EdgePropertyIndex {
    /// Descriptor for this index.
    pub descriptor: EdgeIndexDescriptor,
    tree: BTreeMap<IndexKey, SmallVec<[EdgeId; 1]>>,
}

impl EdgePropertyIndex {
    /// Empty edge property index.
    pub fn new(descriptor: EdgeIndexDescriptor) -> Self {
        Self {
            descriptor,
            tree: BTreeMap::new(),
        }
    }

    /// Adds `edge_id` under `key`.
    pub fn insert(&mut self, key: IndexKey, edge_id: EdgeId) {
        self.tree.entry(key).or_default().push(edge_id);
    }

    /// Removes `edge_id` from `key`'s entry.
    pub fn remove(&mut self, key: &IndexKey, edge_id: EdgeId) {
        if let Some(ids) = self.tree.get_mut(key) {
            ids.retain(|id| *id != edge_id);
            if ids.is_empty() {
                self.tree.remove(key);
            }
        }
    }

    /// Edge IDs with exactly `key`.
    pub fn lookup(&self, key: &IndexKey) -> Option<&SmallVec<[EdgeId; 1]>> {
        self.tree.get(key)
    }

    /// Distinct keys.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Index empty?
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Indexed edge entries across all keys.
    pub fn total_entries(&self) -> usize {
        self.tree.values().map(|v| v.len()).sum()
    }
}

/// Edge index key, for lookup by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EdgeIndexKey {
    /// Property index on (edge_type, property_key).
    Property(EdgeTypeId, PropertyKeyId),
    /// Type-only index on edge_type.
    TypeOnly(EdgeTypeId),
}

/// Edge-index metadata, the [`IndexCatalog`] counterpart.
#[derive(Debug, Clone, Default)]
pub struct EdgeIndexCatalog {
    /// By name; property and type-only indexes.
    descriptors: Vec<EdgeIndexDescriptor>,
    generation: u64,
}

impl EdgeIndexCatalog {
    /// Bumped on each edge-index change.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// All edge index descriptors, by name.
    pub fn descriptors(&self) -> &[EdgeIndexDescriptor] {
        &self.descriptors
    }

    /// Property index on `(edge type, property)`?
    pub fn has_prop_index(&self, edge_type_id: EdgeTypeId, property_key: PropertyKeyId) -> bool {
        self.descriptors
            .iter()
            .any(|d| d.edge_type_id == edge_type_id && d.property_key == Some(property_key))
    }

    /// Descriptor by name.
    pub fn get_descriptor_by_name(&self, name: &str) -> Option<&EdgeIndexDescriptor> {
        self.descriptors.iter().find(|d| d.name == name)
    }
}

/// All edge indexes of a graph.
#[derive(Debug, Clone, Default)]
pub struct EdgeIndexRegistry {
    /// Property indexes by (EdgeTypeId, PropertyKeyId).
    prop_indexes: FxHashMap<(EdgeTypeId, PropertyKeyId), EdgePropertyIndex>,
    /// Type-only indexes: names only, for DDL (data lives in Graph::edge_type_index).
    type_only: FxHashMap<EdgeTypeId, String>,
    /// Index name -> key.
    name_map: FxHashMap<String, EdgeIndexKey>,
    /// Metadata snapshot, rebuilt on every create/drop.
    catalog: Arc<EdgeIndexCatalog>,
}

impl EdgeIndexRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current generation.
    pub fn generation(&self) -> u64 {
        self.catalog.generation
    }

    /// Metadata snapshot.
    pub fn catalog(&self) -> &Arc<EdgeIndexCatalog> {
        &self.catalog
    }

    fn refresh_catalog(&mut self) {
        let mut descriptors = self.descriptors();
        descriptors.sort_by(|a, b| a.name.cmp(&b.name));
        self.catalog = Arc::new(EdgeIndexCatalog {
            descriptors,
            generation: self.catalog.generation + 1,
        });
    }

    /// Creates an edge index; errors if the name is taken.
    pub fn create_index(&mut self, descriptor: EdgeIndexDescriptor) -> Result<()> {
        if self.name_map.contains_key(&descriptor.name) {
            return Err(Error::IndexError(format!(
                "edge index '{}' already exists",
                descriptor.name
            )));
        }
        let key = match descriptor.property_key {
            Some(pkid) => {
                let composite = (descriptor.edge_type_id, pkid);
                if self.prop_indexes.contains_key(&composite) {
                    return Err(Error::IndexError(
                        "an edge index already exists for that type/property combination"
                            .to_string(),
                    ));
                }
                self.prop_indexes
                    .insert(composite, EdgePropertyIndex::new(descriptor.clone()));
                EdgeIndexKey::Property(composite.0, composite.1)
            }
            None => {
                if self.type_only.contains_key(&descriptor.edge_type_id) {
                    return Err(Error::IndexError(
                        "an edge type-only index already exists for that type".to_string(),
                    ));
                }
                self.type_only
                    .insert(descriptor.edge_type_id, descriptor.name.clone());
                EdgeIndexKey::TypeOnly(descriptor.edge_type_id)
            }
        };
        self.name_map.insert(descriptor.name.clone(), key);
        self.refresh_catalog();
        Ok(())
    }

    /// Drops an edge index by name; errors if missing.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let key = self
            .name_map
            .remove(name)
            .ok_or_else(|| Error::IndexError(format!("edge index '{name}' not found")))?;
        match key {
            EdgeIndexKey::Property(etid, pkid) => {
                self.prop_indexes.remove(&(etid, pkid));
            }
            EdgeIndexKey::TypeOnly(etid) => {
                self.type_only.remove(&etid);
            }
        }
        self.refresh_catalog();
        Ok(())
    }

    /// Property index for (edge type, property).
    pub fn get_prop_index(
        &self,
        edge_type_id: EdgeTypeId,
        property_key: PropertyKeyId,
    ) -> Option<&EdgePropertyIndex> {
        self.prop_indexes.get(&(edge_type_id, property_key))
    }

    /// Mutable property index for (edge type, property).
    pub fn get_prop_index_mut(
        &mut self,
        edge_type_id: EdgeTypeId,
        property_key: PropertyKeyId,
    ) -> Option<&mut EdgePropertyIndex> {
        self.prop_indexes.get_mut(&(edge_type_id, property_key))
    }

    /// Descriptor by name.
    pub fn get_descriptor_by_name(&self, name: &str) -> Option<EdgeIndexDescriptor> {
        let key = self.name_map.get(name)?;
        match key {
            EdgeIndexKey::Property(etid, pkid) => self
                .prop_indexes
                .get(&(*etid, *pkid))
                .map(|idx| idx.descriptor.clone()),
            EdgeIndexKey::TypeOnly(etid) => Some(EdgeIndexDescriptor {
                edge_type_id: *etid,
                property_key: None,
                name: name.to_string(),
            }),
        }
    }

    /// All edge index descriptors.
    pub fn descriptors(&self) -> Vec<EdgeIndexDescriptor> {
        let mut descs: Vec<EdgeIndexDescriptor> = self
            .prop_indexes
            .values()
            .map(|idx| idx.descriptor.clone())
            .collect();
        for (etid, name) in &self.type_only {
            descs.push(EdgeIndexDescriptor {
                edge_type_id: *etid,
                property_key: None,
                name: name.clone(),
            });
        }
        descs
    }

    /// All property indexes.
    pub fn iter_prop(
        &self,
    ) -> impl Iterator<Item = (&(EdgeTypeId, PropertyKeyId), &EdgePropertyIndex)> {
        self.prop_indexes.iter()
    }

    /// All type-only indexes.
    pub fn iter_type_only(&self) -> impl Iterator<Item = (&EdgeTypeId, &String)> {
        self.type_only.iter()
    }
}
