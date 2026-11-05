//! Graph support for MVCC readers and writers: index keys an entity carries, the
//! ghost memberships a writer records at first touch, state restores for abort,
//! and exact per-node scoring for the vector / full-text / point indexes so a
//! snapshot reader can re-rank versions the live indexes no longer hold.

use std::sync::Arc;

use smallvec::SmallVec;

use super::Graph;
use super::constraint::{ConstraintDescriptor, ConstraintEntity, ConstraintType};
use super::property::{IndexKey, InternedPropertyMap, PropertyValue};
use super::stats::GraphStatistics;
use super::table::IdSet;
use super::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use crate::error::{Error, Result};
use crate::tx::versions::{EdgeGhost, NodeGhost, Versions};

impl Graph {
    /// Runs and ghosts (see [`Versions`]).
    #[inline]
    pub(crate) fn versions(&self) -> &Versions {
        &self.versions
    }

    #[inline]
    pub(crate) fn versions_mut(&mut self) -> &mut Versions {
        &mut self.versions
    }

    /// Nodes with `label` now.
    #[inline]
    pub(crate) fn label_members(&self, label: LabelId) -> Option<&IdSet<NodeId>> {
        self.label_index.get(&label)
    }

    /// Edges of `edge_type` now.
    #[inline]
    pub(crate) fn type_members(&self, edge_type: EdgeTypeId) -> Option<&IdSet<EdgeId>> {
        self.edge_type_index.get(&edge_type)
    }

    /// B-tree index entries a node with `labels` / `props` has: `(label, key, value)` for every
    /// single-property and composite index.
    pub(crate) fn node_index_keys(
        &self,
        labels: &[LabelId],
        props: &InternedPropertyMap,
    ) -> Vec<(LabelId, PropertyKeyId, IndexKey)> {
        let mut out = Vec::new();
        if self.indexes.catalog().descriptors().is_empty() {
            return out;
        }
        for &label in labels {
            for (key, value) in props.iter() {
                if self.indexes.has_index(label, key)
                    && let Some(v) = IndexKey::from_property_value(&value)
                {
                    out.push((label, key, v));
                }
            }
        }
        if self.has_composite_node_indexes() {
            out.extend(self.composite_node_entries(labels, props));
        }
        out
    }

    /// Edge-index entries an edge of `edge_type` with `props` has.
    pub(crate) fn edge_index_keys(
        &self,
        edge_type: EdgeTypeId,
        props: &InternedPropertyMap,
    ) -> Vec<(PropertyKeyId, IndexKey)> {
        let mut out = Vec::new();
        if self.edge_indexes.iter_prop().next().is_none() {
            return out;
        }
        for (key, value) in props.iter() {
            if self.edge_indexes.get_prop_index(edge_type, key).is_some()
                && let Some(v) = IndexKey::from_property_value(&value)
            {
                out.push((key, v));
            }
        }
        if self.has_composite_edge_indexes() {
            out.extend(self.composite_edge_entries(edge_type, props));
        }
        out
    }

    /// The B-tree index value `props` has under index key `key`: the property's value, or for a
    /// composite index (synthetic joined key) the encoded tuple. Works for node and edge indexes,
    /// whether or not the index still exists.
    pub(crate) fn index_key_of(
        &self,
        props: &InternedPropertyMap,
        key: PropertyKeyId,
    ) -> Option<IndexKey> {
        match self.strings.try_resolve(key.0) {
            Some(name) if name.contains(super::COMPOSITE_PROP_SEP) => {
                let parts: Option<Vec<PropertyKeyId>> = name
                    .split(super::COMPOSITE_PROP_SEP)
                    .map(|p| self.strings.get(p).map(PropertyKeyId))
                    .collect();
                Self::composite_key_of(props, &parts?)
            }
            _ => IndexKey::from_property_value(&props.get(key)?),
        }
    }

    /// What a node is listed under now (labels, index keys), for a writer's first touch.
    pub(crate) fn node_ghosts(&self, id: NodeId) -> Vec<NodeGhost> {
        let Some(node) = self.nodes.get(&id) else {
            return Vec::new();
        };
        let mut out: Vec<NodeGhost> = node.labels.iter().map(|&l| NodeGhost::Label(l)).collect();
        out.extend(
            self.node_index_keys(&node.labels, &node.properties)
                .into_iter()
                .map(|(l, k, v)| NodeGhost::Index(l, k, v)),
        );
        out
    }

    /// What an edge is listed under now (type, index keys), for a writer's first touch.
    pub(crate) fn edge_ghosts(&self, id: EdgeId) -> Vec<EdgeGhost> {
        let Ok(edge) = self.get_edge(id) else {
            return Vec::new();
        };
        let mut out = vec![EdgeGhost::Type(edge.edge_type)];
        out.extend(
            self.edge_index_keys(edge.edge_type, &edge.properties)
                .into_iter()
                .map(|(k, v)| EdgeGhost::Index(edge.edge_type, k, v)),
        );
        out
    }

    /// Puts a node back to `labels` / `properties` (abort), keeping the label and every property
    /// index in step.
    pub(crate) fn set_node_state(
        &mut self,
        id: NodeId,
        labels: SmallVec<[LabelId; 2]>,
        properties: InternedPropertyMap,
    ) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }
        self.remove_node_from_indexes(id);
        let node = self.nodes.get_mut(&id).expect("checked above");
        let old_labels = std::mem::replace(&mut node.labels, labels);
        node.properties = properties;
        let new_labels = node.labels.clone();
        self.unlabeled = self.unlabeled + usize::from(new_labels.is_empty())
            - usize::from(old_labels.is_empty());
        for l in &old_labels {
            if !new_labels.contains(l)
                && let Some(set) = self.label_index.get_mut(l)
            {
                set.remove(&id);
            }
        }
        for l in &new_labels {
            self.label_index.entry(*l).or_default().insert(id);
        }
        self.dirty_nodes.insert(id);
        self.add_node_to_indexes(id);
        Ok(())
    }

    /// Puts an edge's properties back (abort), keeping the edge indexes in step.
    pub(crate) fn set_edge_state(
        &mut self,
        id: EdgeId,
        properties: InternedPropertyMap,
    ) -> Result<()> {
        let edge_type = self.get_edge(id)?.edge_type;
        self.remove_edge_from_indexes(id, edge_type);
        if properties.is_empty() {
            // demote to propless when it had properties
            if let Some(e) = self.edges.remove(&id) {
                self.propless.insert(
                    id,
                    super::ProplessEdge {
                        edge_type: e.edge_type,
                        source: e.source,
                        target: e.target,
                    },
                );
            }
        } else {
            if self.propless.contains_key(&id) {
                self.promote_edge(id);
            }
            if let Some(e) = self.edges.get_mut(&id) {
                e.properties = properties;
            }
        }
        self.dirty_edges.insert(id);
        self.add_edge_to_indexes(id, edge_type);
        Ok(())
    }

    /// Re-registers a dropped constraint exactly (abort), with its backing index.
    pub(crate) fn restore_constraint(&mut self, desc: ConstraintDescriptor) -> Result<()> {
        let backing = desc.entity == ConstraintEntity::Node
            && desc.extra_property_keys.is_empty()
            && matches!(desc.constraint_type, ConstraintType::Unique);
        let (name, label, key) = (desc.name.clone(), desc.label_id, desc.property_key);
        Arc::make_mut(&mut self.constraints).create_constraint(desc)?;
        self.strings_dirty = true;
        if backing {
            self.create_constraint_backing_index(&name, label, key)?;
        }
        Ok(())
    }

    /// The statistics snapshot, for a writer to restore on abort.
    pub(crate) fn statistics_arc(&self) -> Option<Arc<GraphStatistics>> {
        self.stats.clone()
    }

    pub(crate) fn set_statistics(&mut self, stats: Option<Arc<GraphStatistics>>) {
        self.stats = stats;
    }

    /// `(label, property, dimensions, similarity, params)` of a vector index, to recreate it.
    pub(crate) fn vector_index_spec(
        &self,
        name: &str,
    ) -> Option<(
        String,
        String,
        usize,
        super::vector::Similarity,
        super::vector::HnswParams,
    )> {
        let i = self.vector_indexes.get(name)?;
        Some((
            self.strings.resolve(i.label_id.0).to_string(),
            self.strings.resolve(i.property_key.0).to_string(),
            i.dim,
            i.sim,
            i.params,
        ))
    }

    /// `(labels, properties, analyzer)` of a full-text index, to recreate it.
    pub(crate) fn fulltext_index_spec(
        &self,
        name: &str,
    ) -> Option<(Vec<String>, Vec<String>, super::fulltext::Analyzer)> {
        let i = self.fulltext_indexes.get(name)?;
        let names = |ids: &mut dyn Iterator<Item = u32>| -> Vec<String> {
            ids.map(|id| self.strings.resolve(id).to_string()).collect()
        };
        Some((
            names(&mut i.label_ids().iter().map(|l| l.0)),
            names(&mut i.property_keys().iter().map(|p| p.0)),
            i.analyzer(),
        ))
    }

    /// `(label, property)` of a point index, to recreate it.
    pub(crate) fn point_index_spec(&self, name: &str) -> Option<(String, String)> {
        let i = self.spatial_indexes.get(name)?;
        Some((
            self.strings.resolve(i.label_id().0).to_string(),
            self.strings.resolve(i.property_key().0).to_string(),
        ))
    }

    /// `(label, property)` of a text index, to recreate it.
    pub(crate) fn text_index_spec(&self, name: &str) -> Option<(String, String)> {
        let i = self.text_indexes.get(name)?;
        Some((
            self.strings.resolve(i.label_id().0).to_string(),
            self.strings.resolve(i.property_key().0).to_string(),
        ))
    }

    /// `(label, property)` of a vector index.
    pub(crate) fn vector_index_target(&self, name: &str) -> Option<(LabelId, PropertyKeyId)> {
        self.vector_indexes
            .get(name)
            .map(|i| (i.label_id, i.property_key))
    }

    /// Score the vector index `name` gives `props`' vector for `query`; `None` without a usable
    /// vector.
    pub(crate) fn vector_score(
        &self,
        name: &str,
        query: &[f32],
        props: &InternedPropertyMap,
    ) -> Option<f64> {
        let idx = self.vector_indexes.get(name)?;
        let v = vector_of(&props.get(idx.property_key)?)?;
        idx.exact_score(query, &v)
    }

    /// `(labels, properties)` of a full-text index.
    pub(crate) fn fulltext_index_target(
        &self,
        name: &str,
    ) -> Option<(Vec<LabelId>, Vec<PropertyKeyId>)> {
        self.fulltext_indexes
            .get(name)
            .map(|i| (i.label_ids().to_vec(), i.property_keys().to_vec()))
    }

    /// BM25 score the full-text index `name` gives a document with `props` for `query` (the
    /// index's corpus statistics); `None` when it doesn't match.
    pub(crate) fn fulltext_score(
        &self,
        name: &str,
        query: &str,
        props: &InternedPropertyMap,
    ) -> Option<f64> {
        let idx = self.fulltext_indexes.get(name)?;
        let texts = texts_of(props, idx.property_keys());
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        idx.score_texts(query, &refs)
    }
}

/// A vector property (`Vector`, or a numeric LIST) as f32s.
pub(crate) fn vector_of(value: &PropertyValue) -> Option<Vec<f32>> {
    match value {
        PropertyValue::Vector(v) => Some(v.clone()),
        PropertyValue::List(items) => items
            .iter()
            .map(|it| match it {
                PropertyValue::Float(f) => Some(*f as f32),
                PropertyValue::Int(i) => Some(*i as f32),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Indexable text of `keys` in `props`: strings and string list elements.
pub(crate) fn texts_of(props: &InternedPropertyMap, keys: &[PropertyKeyId]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        match props.get(*key) {
            Some(PropertyValue::String(s)) => out.push(s),
            Some(PropertyValue::List(items)) => {
                for it in items {
                    if let PropertyValue::String(s) = it {
                        out.push(s);
                    }
                }
            }
            _ => {}
        }
    }
    out
}
