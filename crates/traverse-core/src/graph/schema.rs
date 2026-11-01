//! Schema: label / type / key metadata, constraints and statistics.

use std::collections::HashMap;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::error::{Error, Result};

use super::Graph;
use super::constraint::{
    ConstraintDescriptor, ConstraintEntity, ConstraintRegistry, ConstraintType,
};
use super::property::{IndexKey, InternedPropertyMap, PropertyMap, PropertyValue};
use super::stats::GraphStatistics;
use super::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};

impl Graph {
    /// All label names, from the label index: O(labels).
    pub fn schema_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .label_index
            .keys()
            .map(|lid| self.strings.resolve(lid.0).to_string())
            .collect();
        labels.sort();
        labels
    }

    /// All edge type names, from the type index: O(types).
    pub fn schema_edge_types(&self) -> Vec<String> {
        let mut types: Vec<String> = self
            .edge_type_index
            .keys()
            .map(|tid| self.strings.resolve(tid.0).to_string())
            .collect();
        types.sort();
        types
    }

    /// All property key names, from the tracked set: O(keys).
    pub fn schema_property_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .known_prop_keys
            .iter()
            .map(|kid| self.strings.resolve(kid.0).to_string())
            .collect();
        keys.sort();
        keys
    }

    /// Property keys with types, by sampling nodes and edges.
    pub fn schema_property_types(&self) -> Vec<(String, &'static str)> {
        let mut types: FxHashMap<PropertyKeyId, &'static str> = FxHashMap::default();
        let target = self.known_prop_keys.len();

        // Sample nodes
        for node in self.nodes.values() {
            for (kid, val) in node.properties.iter() {
                types.entry(kid).or_insert_with(|| val.type_name());
            }
            if types.len() >= target {
                break;
            }
        }

        // Sample edges if still missing keys
        if types.len() < target {
            for edge in self.edges.values() {
                for (kid, val) in edge.properties.iter() {
                    types.entry(kid).or_insert_with(|| val.type_name());
                }
                if types.len() >= target {
                    break;
                }
            }
        }

        let mut result: Vec<(String, &'static str)> = types
            .into_iter()
            .map(|(kid, tn)| (self.strings.resolve(kid.0).to_string(), tn))
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    /// Per-label and per-edge-type property maps, sampled from up to 200 entities
    /// each: `(label_props, edge_type_props)` sorted by owner, inner lists by property.
    pub fn schema_properties_by_owner(
        &self,
    ) -> (
        Vec<(String, Vec<(String, &'static str)>)>,
        Vec<(String, Vec<(String, &'static str)>)>,
    ) {
        const SAMPLE_LIMIT: usize = 200;

        // -- Label properties ------------------------------------------
        let mut label_props: Vec<(String, Vec<(String, &'static str)>)> = self
            .label_index
            .iter()
            .map(|(lid, node_ids)| {
                let label_name = self.strings.resolve(lid.0).to_string();
                let mut props: FxHashMap<PropertyKeyId, &'static str> = FxHashMap::default();
                for (i, nid) in node_ids.iter().enumerate() {
                    if i >= SAMPLE_LIMIT {
                        break;
                    }
                    if let Some(node) = self.nodes.get(nid) {
                        for (kid, val) in node.properties.iter() {
                            props.entry(kid).or_insert_with(|| val.type_name());
                        }
                    }
                }
                let mut prop_list: Vec<(String, &'static str)> = props
                    .into_iter()
                    .map(|(kid, tn)| (self.strings.resolve(kid.0).to_string(), tn))
                    .collect();
                prop_list.sort_by(|a, b| a.0.cmp(&b.0));
                (label_name, prop_list)
            })
            .collect();
        label_props.sort_by(|a, b| a.0.cmp(&b.0));

        // -- Edge type properties --------------------------------------
        let mut edge_type_props: Vec<(String, Vec<(String, &'static str)>)> = self
            .edge_type_index
            .iter()
            .map(|(tid, edge_ids)| {
                let type_name = self.strings.resolve(tid.0).to_string();
                let mut props: FxHashMap<PropertyKeyId, &'static str> = FxHashMap::default();
                let mut sampled = 0usize;
                for eid in edge_ids.iter() {
                    if sampled >= SAMPLE_LIMIT {
                        break;
                    }
                    // only full edges have properties
                    if let Some(edge) = self.edges.get(eid) {
                        for (kid, val) in edge.properties.iter() {
                            props.entry(kid).or_insert_with(|| val.type_name());
                        }
                        sampled += 1;
                    }
                }
                let mut prop_list: Vec<(String, &'static str)> = props
                    .into_iter()
                    .map(|(kid, tn)| (self.strings.resolve(kid.0).to_string(), tn))
                    .collect();
                prop_list.sort_by(|a, b| a.0.cmp(&b.0));
                (type_name, prop_list)
            })
            .collect();
        edge_type_props.sort_by(|a, b| a.0.cmp(&b.0));

        (label_props, edge_type_props)
    }

    /// Interns all keys in a string-keyed property map, returning an interned map.
    pub(super) fn intern_property_map(&mut self, props: PropertyMap) -> InternedPropertyMap {
        if !props.is_empty() {
            self.strings_dirty = true;
        }
        props
            .into_iter()
            .map(|(k, v)| {
                let kid = PropertyKeyId(self.strings.intern(&k));
                self.known_prop_keys.insert(kid);
                (kid, v)
            })
            .collect()
    }

    /// Resolves an interned property map back to string keys.
    pub fn resolve_property_map(&self, props: &InternedPropertyMap) -> PropertyMap {
        props
            .iter()
            .map(|(kid, pv)| (self.strings.resolve(kid.0).to_string(), pv))
            .collect()
    }

    /// Interns a property key string, returning its PropertyKeyId.
    pub fn intern_property_key(&mut self, key: &str) -> PropertyKeyId {
        self.strings_dirty = true;
        let kid = PropertyKeyId(self.strings.intern(key));
        self.known_prop_keys.insert(kid);
        kid
    }

    // -- Constraints --------------------------------------------------

    /// Creates a constraint, validating existing data.
    pub fn create_constraint(
        &mut self,
        name: &str,
        label: &str,
        property_key: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        let lid = self.strings.intern(label);
        let pkid = PropertyKeyId(self.strings.intern(property_key));

        // Validate existing data
        let node_ids: Vec<NodeId> = self
            .label_index
            .get(&LabelId(lid))
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        let is_unique = matches!(ctype, ConstraintType::Unique);

        match ctype {
            ConstraintType::Unique => {
                let mut seen: HashMap<IndexKey, NodeId> = HashMap::new();
                for nid in &node_ids {
                    let node = self.nodes.get(nid).ok_or(Error::NodeNotFound(*nid))?;
                    if let Some(pv) = node.properties.get(pkid)
                        && !matches!(pv, PropertyValue::Null)
                        && let Some(key) = IndexKey::from_property_value(&pv)
                    {
                        if let Some(existing) = seen.get(&key) {
                            return Err(Error::ConstraintError(format!(
                                "existing data violates unique constraint: node({}) and node({}) both have label `{}` with property `{}` = same value",
                                existing.0, nid.0, label, property_key
                            )));
                        }
                        seen.insert(key, *nid);
                    }
                }
            }
            ConstraintType::Exists => {
                for nid in &node_ids {
                    let node = self.nodes.get(nid).ok_or(Error::NodeNotFound(*nid))?;
                    match node.properties.get(pkid) {
                        Some(PropertyValue::Null) | None => {
                            return Err(Error::ConstraintError(format!(
                                "existing data violates existence constraint: node({}) with label `{}` is missing property `{}`",
                                nid.0, label, property_key
                            )));
                        }
                        _ => {}
                    }
                }
            }
            ConstraintType::Type(kind) => {
                for nid in &node_ids {
                    let node = self.nodes.get(nid).ok_or(Error::NodeNotFound(*nid))?;
                    if let Some(pv) = node.properties.get(pkid)
                        && !kind.matches(&pv)
                    {
                        return Err(Error::ConstraintError(format!(
                            "existing data violates type constraint: node({}) with label `{}` has property `{}` of incompatible type (expected {})",
                            nid.0,
                            label,
                            property_key,
                            kind.surface()
                        )));
                    }
                }
            }
            ConstraintType::NodeKey => {
                // NODE KEY = UNIQUE + NOT NULL on the same property
                let mut seen: HashMap<IndexKey, NodeId> = HashMap::new();
                for nid in &node_ids {
                    let node = self.nodes.get(nid).ok_or(Error::NodeNotFound(*nid))?;
                    let pv = node.properties.get(pkid);
                    match pv {
                        Some(PropertyValue::Null) | None => {
                            return Err(Error::ConstraintError(format!(
                                "existing data violates NODE KEY constraint: node({}) with label `{}` is missing property `{}`",
                                nid.0, label, property_key
                            )));
                        }
                        Some(v) => {
                            if let Some(key) = IndexKey::from_property_value(&v) {
                                if let Some(existing) = seen.get(&key) {
                                    return Err(Error::ConstraintError(format!(
                                        "existing data violates NODE KEY constraint: node({}) and node({}) share label `{}` with property `{}`",
                                        existing.0, nid.0, label, property_key
                                    )));
                                }
                                seen.insert(key, *nid);
                            }
                        }
                    }
                }
            }
        }

        let desc = ConstraintDescriptor {
            name: name.to_string(),
            label_id: LabelId(lid),
            property_key: pkid,
            extra_property_keys: Vec::new(),
            constraint_type: ctype,
            entity: ConstraintEntity::Node,
        };
        Arc::make_mut(&mut self.constraints).create_constraint(desc)?;
        self.strings_dirty = true;

        if is_unique {
            self.create_constraint_backing_index(name, LabelId(lid), pkid)?;
        }
        Ok(())
    }

    /// Unique constraints are index-backed, like Neo4j's: constrained lookups, MERGE keys and
    /// lookup joins become index seeks (tutorial datasets often create only the constraint).
    /// Named after the constraint so drop_constraint pairs with it; skipped if an equivalent index
    /// exists.
    pub(crate) fn create_constraint_backing_index(
        &mut self,
        name: &str,
        label: LabelId,
        key: PropertyKeyId,
    ) -> Result<()> {
        if self.indexes.has_index(label, key) {
            return Ok(());
        }
        let label = self.strings.resolve(label.0).to_string();
        let key = self.strings.resolve(key.0).to_string();
        self.create_index(&format!("__constraint_{name}"), &label, &key)
    }

    /// Relationship property constraint (`FOR ()-[r:T]-() REQUIRE ...`). Like
    /// [`Self::create_constraint`], but validates existing edges of `edge_type` and
    /// registers as [`ConstraintEntity::Relationship`].
    pub fn create_relationship_constraint(
        &mut self,
        name: &str,
        edge_type: &str,
        property_key: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        let tid = self.strings.intern(edge_type);
        let pkid = PropertyKeyId(self.strings.intern(property_key));

        let edge_ids: Vec<EdgeId> = self
            .edge_type_index
            .get(&EdgeTypeId(tid))
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();

        match ctype {
            ConstraintType::Unique => {
                let mut seen: HashMap<IndexKey, EdgeId> = HashMap::new();
                for eid in &edge_ids {
                    let pv = self.edges.get(eid).and_then(|e| e.properties.get(pkid));
                    if let Some(pv) = pv
                        && !matches!(pv, PropertyValue::Null)
                        && let Some(key) = IndexKey::from_property_value(&pv)
                    {
                        if let Some(existing) = seen.get(&key) {
                            return Err(Error::ConstraintError(format!(
                                "existing data violates unique constraint: edge({}) and edge({}) both have type `{}` with property `{}` = same value",
                                existing.0, eid.0, edge_type, property_key
                            )));
                        }
                        seen.insert(key, *eid);
                    }
                }
            }
            ConstraintType::Exists => {
                for eid in &edge_ids {
                    let pv = self.edges.get(eid).and_then(|e| e.properties.get(pkid));
                    if !matches!(pv, Some(v) if !matches!(v, PropertyValue::Null)) {
                        return Err(Error::ConstraintError(format!(
                            "existing data violates existence constraint: edge({}) with type `{}` is missing property `{}`",
                            eid.0, edge_type, property_key
                        )));
                    }
                }
            }
            ConstraintType::Type(kind) => {
                for eid in &edge_ids {
                    if let Some(pv) = self.edges.get(eid).and_then(|e| e.properties.get(pkid))
                        && !kind.matches(&pv)
                    {
                        return Err(Error::ConstraintError(format!(
                            "existing data violates type constraint: edge({}) with type `{}` has property `{}` of incompatible type (expected {})",
                            eid.0,
                            edge_type,
                            property_key,
                            kind.surface()
                        )));
                    }
                }
            }
            ConstraintType::NodeKey => {
                let mut seen: HashMap<IndexKey, EdgeId> = HashMap::new();
                for eid in &edge_ids {
                    let pv = self.edges.get(eid).and_then(|e| e.properties.get(pkid));
                    match pv {
                        Some(v) if !matches!(v, PropertyValue::Null) => {
                            if let Some(key) = IndexKey::from_property_value(&v) {
                                if let Some(existing) = seen.get(&key) {
                                    return Err(Error::ConstraintError(format!(
                                        "existing data violates relationship KEY constraint: edge({}) and edge({}) share type `{}` with property `{}`",
                                        existing.0, eid.0, edge_type, property_key
                                    )));
                                }
                                seen.insert(key, *eid);
                            }
                        }
                        _ => {
                            return Err(Error::ConstraintError(format!(
                                "existing data violates relationship KEY constraint: edge({}) with type `{}` is missing property `{}`",
                                eid.0, edge_type, property_key
                            )));
                        }
                    }
                }
            }
        }

        let desc = ConstraintDescriptor {
            name: name.to_string(),
            label_id: LabelId(tid),
            property_key: pkid,
            extra_property_keys: Vec::new(),
            constraint_type: ctype,
            entity: ConstraintEntity::Relationship,
        };
        Arc::make_mut(&mut self.constraints).create_constraint(desc)?;
        self.strings_dirty = true;
        Ok(())
    }

    /// COMPOSITE constraint (`REQUIRE (n.a, n.b) IS NODE KEY` / `IS UNIQUE`) over 2+
    /// properties. Uniqueness is on the value tuple; tuples with a NULL are exempt
    /// (Neo4j), and NODE KEY's existence side rejects them anyway. NODE KEY also
    /// requires every key on every entity with the label/type.
    pub fn create_composite_constraint(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        ctype: ConstraintType,
        is_relationship: bool,
    ) -> Result<()> {
        if properties.len() < 2 {
            return Err(Error::ConstraintError(
                "composite constraints need at least two properties".to_string(),
            ));
        }
        if !matches!(ctype, ConstraintType::Unique | ConstraintType::NodeKey) {
            return Err(Error::ConstraintError(
                "composite constraints support only UNIQUE and KEY".to_string(),
            ));
        }
        let lid = self.strings.intern(label);
        let pkids: Vec<PropertyKeyId> = properties
            .iter()
            .map(|p| PropertyKeyId(self.strings.intern(p)))
            .collect();

        // each entity's value tuple as index keys; `None` = missing/null/unindexable
        let tuple_of = |props: &InternedPropertyMap| -> Vec<Option<IndexKey>> {
            pkids
                .iter()
                .map(|pk| {
                    props.get(*pk).and_then(|pv| {
                        if matches!(pv, PropertyValue::Null) {
                            None
                        } else {
                            IndexKey::from_property_value(&pv)
                        }
                    })
                })
                .collect()
        };

        // Validate existing data.
        let mut seen: HashMap<Vec<IndexKey>, u64> = HashMap::new();
        let tuples: Vec<(u64, Vec<Option<IndexKey>>)> = if is_relationship {
            let edge_ids: Vec<EdgeId> = self
                .edge_type_index
                .get(&EdgeTypeId(lid))
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            edge_ids
                .iter()
                .filter_map(|eid| {
                    self.edges
                        .get(eid)
                        .map(|e| (eid.0, tuple_of(&e.properties)))
                })
                .collect()
        } else {
            let node_ids: Vec<NodeId> = self
                .label_index
                .get(&LabelId(lid))
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            node_ids
                .iter()
                .filter_map(|nid| {
                    self.nodes
                        .get(nid)
                        .map(|n| (nid.0, tuple_of(&n.properties)))
                })
                .collect()
        };
        let entity_word = if is_relationship {
            "relationship"
        } else {
            "node"
        };
        for (id, tuple) in tuples {
            if matches!(ctype, ConstraintType::NodeKey) && tuple.iter().any(Option::is_none) {
                return Err(Error::ConstraintError(format!(
                    "existing data violates composite KEY constraint: {entity_word}({id}) with \
                     `{label}` is missing one of the key properties ({})",
                    properties.join(", ")
                )));
            }
            if tuple.iter().all(Option::is_some) {
                let key: Vec<IndexKey> = tuple.into_iter().map(Option::unwrap).collect();
                if let Some(existing) = seen.get(&key) {
                    return Err(Error::ConstraintError(format!(
                        "existing data violates composite constraint: {entity_word}({existing}) \
                         and {entity_word}({id}) share `{label}` with the same ({}) tuple",
                        properties.join(", ")
                    )));
                }
                seen.insert(key, id);
            }
        }

        let desc = ConstraintDescriptor {
            name: name.to_string(),
            label_id: LabelId(lid),
            property_key: pkids[0],
            extra_property_keys: pkids[1..].to_vec(),
            constraint_type: ctype,
            entity: if is_relationship {
                ConstraintEntity::Relationship
            } else {
                ConstraintEntity::Node
            },
        };
        Arc::make_mut(&mut self.constraints).create_constraint(desc)?;
        self.strings_dirty = true;
        Ok(())
    }

    /// Drops a constraint by name.
    pub fn drop_constraint(&mut self, name: &str) -> Result<()> {
        Arc::make_mut(&mut self.constraints).drop_constraint(name)?;
        // drop the index backing a unique constraint
        let backing = format!("__constraint_{name}");
        if self.indexes.descriptors().iter().any(|d| d.name == backing) {
            self.drop_index(&backing)?;
        }
        Ok(())
    }

    /// Returns a reference to the constraint registry.
    pub fn constraint_registry(&self) -> &ConstraintRegistry {
        &self.constraints
    }

    // -- Statistics -----------------------------------------------------

    /// Returns a reference to the collected statistics, if any.
    pub fn statistics(&self) -> Option<&GraphStatistics> {
        self.stats.as_deref()
    }

    /// Returns the names of all labels in the graph.
    pub fn label_names(&self) -> Vec<String> {
        self.label_index
            .keys()
            .filter_map(|lid| self.strings.try_resolve(lid.0).map(|s| s.to_string()))
            .collect()
    }

    /// Returns edge type names with their edge ID sets.
    pub fn edge_type_entries(&self) -> Vec<(String, &FxHashSet<EdgeId>)> {
        self.edge_type_index
            .iter()
            .filter_map(|(tid, set)| {
                self.strings
                    .try_resolve(tid.0)
                    .map(|s| (s.to_string(), set))
            })
            .collect()
    }
}
