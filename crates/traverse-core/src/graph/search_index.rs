//! Vector (HNSW), full-text, point and text indexes: create, drop, query, persist
//! and maintain.

use crate::error::{Error, Result};

use super::property::PropertyValue;
use super::types::{LabelId, NodeId, PropertyKeyId};
use super::{Graph, fulltext, spatial, text, vector, versioned};

impl Graph {
    /// A node's vector property as `Vec<f32>` (numeric LIST), else `None`.
    fn node_vector(&self, node_id: NodeId, pkid: PropertyKeyId) -> Option<Vec<f32>> {
        versioned::vector_of(&self.nodes.get(&node_id)?.properties.get(pkid)?)
    }

    /// Creates a vector index (Neo4j `CREATE VECTOR INDEX`), filled from existing nodes with the
    /// label and a vector of matching dimension.
    pub fn create_vector_index(
        &mut self,
        name: &str,
        label: &str,
        property: &str,
        dim: usize,
        sim: vector::Similarity,
        params: vector::HnswParams,
    ) -> Result<()> {
        let lid = LabelId(self.strings.intern(label));
        let pkid = PropertyKeyId(self.strings.intern(property));
        self.strings_dirty = true;
        self.vector_indexes
            .create(name.to_string(), lid, pkid, dim, sim, params)
            .map_err(Error::IndexError)?;

        // Populate from existing matching nodes.
        let node_ids: Vec<NodeId> = self
            .label_index
            .get(&lid)
            .map(|set| set.to_vec())
            .unwrap_or_default();
        let vecs: Vec<(NodeId, Vec<f32>)> = node_ids
            .into_iter()
            .filter_map(|nid| self.node_vector(nid, pkid).map(|v| (nid, v)))
            .collect();
        if let Some(idx) = self.vector_indexes.get_mut(name) {
            for (nid, v) in vecs {
                idx.upsert(nid, &v);
            }
        }
        Ok(())
    }

    /// Drop a vector index by name.
    pub fn drop_vector_index(&mut self, name: &str) -> Result<()> {
        self.vector_indexes.drop(name).map_err(Error::IndexError)
    }

    /// True if a vector index of this name exists.
    pub fn has_vector_index(&self, name: &str) -> bool {
        self.vector_indexes.get(name).is_some()
    }

    /// `k` approximate nearest neighbors of `query` as `(NodeId, score)`, best first;
    /// larger score = closer, as in Neo4j. `None` if no such index.
    pub fn query_vector_index(
        &self,
        name: &str,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Option<Vec<(NodeId, f64)>> {
        self.vector_indexes
            .get(name)
            .map(|idx| idx.query(query, k, ef))
    }

    /// Indexable text of a node: string values (and string list elements) of the given keys.
    fn node_texts(&self, node_id: NodeId, pkids: &[PropertyKeyId]) -> Vec<String> {
        self.nodes
            .get(&node_id)
            .map(|n| versioned::texts_of(&n.properties, pkids))
            .unwrap_or_default()
    }

    /// Creates a full-text index (`CREATE FULLTEXT INDEX`), filled from existing nodes with any of
    /// the labels.
    pub fn create_fulltext_index(
        &mut self,
        name: &str,
        labels: &[&str],
        properties: &[&str],
        analyzer: fulltext::Analyzer,
    ) -> Result<()> {
        let label_ids: Vec<LabelId> = labels
            .iter()
            .map(|l| LabelId(self.strings.intern(l)))
            .collect();
        let prop_keys: Vec<PropertyKeyId> = properties
            .iter()
            .map(|p| PropertyKeyId(self.strings.intern(p)))
            .collect();
        self.strings_dirty = true;
        self.fulltext_indexes
            .create(name, label_ids.clone(), prop_keys.clone(), analyzer)
            .map_err(Error::IndexError)?;

        // populate from existing nodes; `index_doc` is idempotent, so multi-label nodes just
        // re-index
        let mut docs: Vec<(NodeId, Vec<String>)> = Vec::new();
        for lid in &label_ids {
            let nids: Vec<NodeId> = self
                .label_index
                .get(lid)
                .map(|s| s.to_vec())
                .unwrap_or_default();
            for nid in nids {
                let texts = self.node_texts(nid, &prop_keys);
                if !texts.is_empty() {
                    docs.push((nid, texts));
                }
            }
        }
        if let Some(idx) = self.fulltext_indexes.get_mut(name) {
            for (nid, texts) in &docs {
                let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                idx.index_doc(*nid, &refs);
            }
        }
        Ok(())
    }

    /// Drop a full-text index by name.
    pub fn drop_fulltext_index(&mut self, name: &str) -> Result<()> {
        self.fulltext_indexes.drop(name).map_err(Error::IndexError)
    }

    /// True if a full-text index of this name exists.
    pub fn has_fulltext_index(&self, name: &str) -> bool {
        self.fulltext_indexes.get(name).is_some()
    }

    /// Up to `limit` `(NodeId, BM25 score)` pairs, best first. `None` if no such index.
    pub fn query_fulltext_index(
        &self,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Option<Vec<(NodeId, f64)>> {
        self.fulltext_indexes
            .get(name)
            .map(|idx| idx.query(query, limit))
    }

    /// Descriptors for SHOW INDEXES: `(name, labels, properties, analyzer)`.
    pub fn fulltext_index_descriptors(&self) -> Vec<(String, Vec<String>, Vec<String>, String)> {
        self.fulltext_indexes
            .iter()
            .map(|idx| {
                let labels = idx
                    .label_ids()
                    .iter()
                    .map(|l| self.strings.resolve(l.0).to_string())
                    .collect();
                let props = idx
                    .property_keys()
                    .iter()
                    .map(|p| self.strings.resolve(p.0).to_string())
                    .collect();
                (
                    idx.name().to_string(),
                    labels,
                    props,
                    idx.analyzer().name().to_string(),
                )
            })
            .collect()
    }

    /// Incrementally maintain full-text indexes after a node write.
    pub(super) fn maintain_fulltext_indexes_for_node(&mut self, node_id: NodeId) {
        if self.fulltext_indexes.is_empty() {
            return;
        }
        let label_ids: Vec<LabelId> = match self.nodes.get(&node_id) {
            Some(n) => n.labels.iter().copied().collect(),
            None => return,
        };
        let mut names: Vec<String> = Vec::new();
        for lid in &label_ids {
            for n in self.fulltext_indexes.indexes_for_label(*lid) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        for name in names {
            let pkids = match self.fulltext_indexes.get(&name) {
                Some(idx) => idx.property_keys().to_vec(),
                None => continue,
            };
            let texts = self.node_texts(node_id, &pkids);
            if let Some(idx) = self.fulltext_indexes.get_mut(&name) {
                if texts.is_empty() {
                    idx.remove_doc(node_id);
                } else {
                    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
                    idx.index_doc(node_id, &refs);
                }
            }
        }
    }

    /// Remove a node from all full-text indexes (node deleted).
    pub(super) fn remove_from_fulltext_indexes(&mut self, node_id: NodeId) {
        if self.fulltext_indexes.is_empty() {
            return;
        }
        let names: Vec<String> = self
            .fulltext_indexes
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        for name in names {
            if let Some(idx) = self.fulltext_indexes.get_mut(&name) {
                idx.remove_doc(node_id);
            }
        }
    }

    /// Descriptors for SHOW INDEXES: `(name, label, property, dim, similarity)`.
    pub fn vector_index_descriptors(&self) -> Vec<(String, String, String, usize, &'static str)> {
        self.vector_indexes
            .iter()
            .map(|v| {
                (
                    v.name.clone(),
                    self.strings
                        .try_resolve(v.label_id.0)
                        .unwrap_or("")
                        .to_string(),
                    self.strings
                        .try_resolve(v.property_key.0)
                        .unwrap_or("")
                        .to_string(),
                    v.dim,
                    v.sim.as_str(),
                )
            })
            .collect()
    }

    /// Persistable descriptors of every vector index for the tvdb catalog: names resolved, HNSW
    /// params kept so the rebuild is identical.
    pub fn vector_index_definitions(&self) -> Vec<crate::storage::catalog::VectorIndexDef> {
        self.vector_indexes
            .iter()
            .map(|v| crate::storage::catalog::VectorIndexDef {
                name: v.name.clone(),
                label: self
                    .strings
                    .try_resolve(v.label_id.0)
                    .unwrap_or("")
                    .to_string(),
                property: self
                    .strings
                    .try_resolve(v.property_key.0)
                    .unwrap_or("")
                    .to_string(),
                dimensions: v.dim as u64,
                similarity: v.sim.as_str().to_string(),
                m: v.params.m as u64,
                ef_construction: v.params.ef_construction as u64,
                ef_search: v.params.ef_search as u64,
                seed: v.params.seed,
                quantize: v.params.quantize,
            })
            .collect()
    }

    /// Re-creates vector indexes from persisted descriptors during reconstruction
    /// (`from_parts` / `from_loaded`), after the label index exists.
    /// `create_vector_index` registers and fills the HNSW in one go.
    pub(crate) fn install_vector_index_definitions(
        &mut self,
        defs: &[crate::storage::catalog::VectorIndexDef],
    ) {
        for d in defs {
            let sim = match vector::Similarity::parse(&d.similarity) {
                Some(s) => s,
                // unknown metric in a persisted descriptor: skip the index, don't fail the load
                None => continue,
            };
            let params = vector::HnswParams {
                m: d.m as usize,
                ef_construction: d.ef_construction as usize,
                ef_search: d.ef_search as usize,
                seed: d.seed,
                quantize: d.quantize,
            };
            let _ = self.create_vector_index(
                &d.name,
                &d.label,
                &d.property,
                d.dimensions as usize,
                sim,
                params,
            );
        }
    }

    /// Persisted full-text-index descriptors for the tvdb catalog.
    pub fn fulltext_index_definitions(&self) -> Vec<crate::storage::catalog::FulltextIndexDef> {
        self.fulltext_indexes
            .iter()
            .map(|idx| crate::storage::catalog::FulltextIndexDef {
                name: idx.name().to_string(),
                labels: idx
                    .label_ids()
                    .iter()
                    .map(|l| self.strings.try_resolve(l.0).unwrap_or("").to_string())
                    .collect(),
                properties: idx
                    .property_keys()
                    .iter()
                    .map(|p| self.strings.try_resolve(p.0).unwrap_or("").to_string())
                    .collect(),
                analyzer: idx.analyzer().name().to_string(),
            })
            .collect()
    }

    /// Re-creates full-text indexes from persisted descriptors, rebuilding each from loaded string
    /// properties.
    pub(crate) fn install_fulltext_index_definitions(
        &mut self,
        defs: &[crate::storage::catalog::FulltextIndexDef],
    ) {
        for d in defs {
            let Some(analyzer) = fulltext::Analyzer::from_name(&d.analyzer) else {
                continue; // unknown analyzer in a persisted descriptor: skip
            };
            let labels: Vec<&str> = d.labels.iter().map(String::as_str).collect();
            let props: Vec<&str> = d.properties.iter().map(String::as_str).collect();
            let _ = self.create_fulltext_index(&d.name, &labels, &props, analyzer);
        }
    }

    // -- Point (spatial / R-tree) indexes -------------------------------

    /// Creates a point index on `(label, property)` (`CREATE POINT INDEX`), filled from existing
    /// nodes.
    pub fn create_point_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        let label_id = LabelId(self.strings.intern(label));
        let prop_key = PropertyKeyId(self.strings.intern(property));
        self.strings_dirty = true;
        self.spatial_indexes
            .create(name, label_id, prop_key)
            .map_err(Error::IndexError)?;

        // populate from existing nodes in one STR bulk load
        let node_ids: Vec<NodeId> = self
            .label_index
            .get(&label_id)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let mut points: Vec<(NodeId, [f64; 3], i64)> = Vec::new();
        for nid in node_ids {
            if let Some((coords, srid)) = self.node_point(nid, prop_key) {
                points.push((nid, coords, srid));
            }
        }
        if let Some(idx) = self.spatial_indexes.get_mut(name) {
            idx.bulk_index(points);
        }
        Ok(())
    }

    /// Drop a point index by name.
    pub fn drop_point_index(&mut self, name: &str) -> Result<()> {
        self.spatial_indexes.drop(name).map_err(Error::IndexError)
    }

    /// True if a point index of this name exists.
    pub fn has_point_index(&self, name: &str) -> bool {
        self.spatial_indexes.get(name).is_some()
    }

    /// Whether a point index covers `(label, property)`: plan-time check for accelerating spatial
    /// predicates.
    pub fn has_point_index_for(&self, label: &str, property: &str) -> bool {
        let (Some(lid), Some(pkid)) = (self.strings.get(label), self.strings.get(property)) else {
            return false;
        };
        self.spatial_indexes
            .index_for(LabelId(lid), PropertyKeyId(pkid))
            .is_some()
    }

    /// Nodes whose point lies in the SRID-matching bbox (`point.withinBBox`). `None` without a
    /// covering index.
    pub fn point_index_within_bbox(
        &self,
        label: &str,
        property: &str,
        ll: [f64; 3],
        ur: [f64; 3],
        srid: i64,
    ) -> Option<Vec<NodeId>> {
        let idx = self.point_index_for(label, property)?;
        Some(idx.within_bbox(ll, ur, srid))
    }

    /// Nodes within `radius` of `center` (`point.distance(...) <= r`) as `(node, distance)`. `None`
    /// without a covering index.
    pub fn point_index_within_distance(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        radius: f64,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        let idx = self.point_index_for(label, property)?;
        Some(idx.within_distance(center, radius, srid))
    }

    /// `k` nearest nodes to `center`, ascending (`ORDER BY point.distance(...) LIMIT k`). `None`
    /// without a covering index.
    pub fn point_index_knn(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        k: usize,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        let idx = self.point_index_for(label, property)?;
        Some(idx.knn(center, k, srid))
    }

    /// Resolve the covering point index for `(label, property)`, if any.
    fn point_index_for(&self, label: &str, property: &str) -> Option<&spatial::SpatialIndex> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(property)?);
        self.spatial_indexes.index_for(lid, pkid)
    }

    /// Descriptors for SHOW INDEXES: `(name, label, property)`.
    pub fn point_index_descriptors(&self) -> Vec<(String, String, String)> {
        self.spatial_indexes
            .iter()
            .map(|idx| {
                (
                    idx.name().to_string(),
                    self.strings
                        .try_resolve(idx.label_id().0)
                        .unwrap_or("")
                        .to_string(),
                    self.strings
                        .try_resolve(idx.property_key().0)
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect()
    }

    /// Point-index descriptors for the tvdb catalog; R-trees rebuild on load.
    pub fn point_index_definitions(&self) -> Vec<crate::storage::catalog::PointIndexDef> {
        self.spatial_indexes
            .iter()
            .map(|idx| crate::storage::catalog::PointIndexDef {
                name: idx.name().to_string(),
                label: self
                    .strings
                    .try_resolve(idx.label_id().0)
                    .unwrap_or("")
                    .to_string(),
                property: self
                    .strings
                    .try_resolve(idx.property_key().0)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect()
    }

    /// Re-creates point indexes from persisted descriptors, rebuilding each R-tree from loaded
    /// nodes.
    pub(crate) fn install_point_index_definitions(
        &mut self,
        defs: &[crate::storage::catalog::PointIndexDef],
    ) {
        for d in defs {
            let _ = self.create_point_index(&d.name, &d.label, &d.property);
        }
    }

    // -- Text (trigram) indexes -----------------------------------------

    /// Creates a trigram text index on `(label, property)` (`CREATE TEXT INDEX`), filled from
    /// existing string values.
    pub fn create_text_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        let label_id = LabelId(self.strings.intern(label));
        let prop_key = PropertyKeyId(self.strings.intern(property));
        self.strings_dirty = true;
        self.text_indexes
            .create(name, label_id, prop_key)
            .map_err(Error::IndexError)?;

        let node_ids: Vec<NodeId> = self
            .label_index
            .get(&label_id)
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let mut values: Vec<(NodeId, String)> = Vec::new();
        for nid in node_ids {
            if let Some(v) = self.node_string(nid, prop_key) {
                values.push((nid, v));
            }
        }
        if let Some(idx) = self.text_indexes.get_mut(name) {
            for (nid, v) in &values {
                idx.index_value(*nid, v);
            }
        }
        Ok(())
    }

    /// Drop a text index by name.
    pub fn drop_text_index(&mut self, name: &str) -> Result<()> {
        self.text_indexes.drop(name).map_err(Error::IndexError)
    }

    /// True if a text index of this name exists.
    pub fn has_text_index(&self, name: &str) -> bool {
        self.text_indexes.get(name).is_some()
    }

    /// Whether a text index covers `(label, property)`: plan-time check for accelerating string
    /// predicates.
    pub fn has_text_index_for(&self, label: &str, property: &str) -> bool {
        let (Some(lid), Some(pkid)) = (self.strings.get(label), self.strings.get(property)) else {
            return false;
        };
        self.text_indexes
            .index_for(LabelId(lid), PropertyKeyId(pkid))
            .is_some()
    }

    /// Candidates for a string predicate on `(label, property)`: a SUPERSET, caller re-checks the
    /// exact predicate. `None` without a covering index.
    pub fn text_index_candidates(
        &self,
        label: &str,
        property: &str,
        kind: text::TextQueryKind,
        pattern: &str,
    ) -> Option<Vec<NodeId>> {
        let lid = LabelId(self.strings.get(label)?);
        let pkid = PropertyKeyId(self.strings.get(property)?);
        let idx = self.text_indexes.index_for(lid, pkid)?;
        Some(idx.candidates(kind, pattern))
    }

    /// Descriptors for SHOW INDEXES: `(name, label, property)`.
    pub fn text_index_descriptors(&self) -> Vec<(String, String, String)> {
        self.text_indexes
            .iter()
            .map(|idx| {
                (
                    idx.name().to_string(),
                    self.strings
                        .try_resolve(idx.label_id().0)
                        .unwrap_or("")
                        .to_string(),
                    self.strings
                        .try_resolve(idx.property_key().0)
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect()
    }

    /// Text-index descriptors for the tvdb catalog; postings rebuild on load.
    pub fn text_index_definitions(&self) -> Vec<crate::storage::catalog::TextIndexDef> {
        self.text_indexes
            .iter()
            .map(|idx| crate::storage::catalog::TextIndexDef {
                name: idx.name().to_string(),
                label: self
                    .strings
                    .try_resolve(idx.label_id().0)
                    .unwrap_or("")
                    .to_string(),
                property: self
                    .strings
                    .try_resolve(idx.property_key().0)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect()
    }

    /// Re-creates text indexes from persisted descriptors, rebuilding trigrams from loaded string
    /// properties.
    pub(crate) fn install_text_index_definitions(
        &mut self,
        defs: &[crate::storage::catalog::TextIndexDef],
    ) {
        for d in defs {
            let _ = self.create_text_index(&d.name, &d.label, &d.property);
        }
    }

    /// A node's String value for `pkid`.
    fn node_string(&self, node_id: NodeId, pkid: PropertyKeyId) -> Option<String> {
        let node = self.nodes.get(&node_id)?;
        match node.properties.get(pkid) {
            Some(PropertyValue::String(s)) => Some(s),
            _ => None,
        }
    }

    /// Incrementally maintain text indexes after a node write.
    pub(super) fn maintain_text_indexes_for_node(&mut self, node_id: NodeId) {
        if self.text_indexes.is_empty() {
            return;
        }
        let label_ids: Vec<LabelId> = match self.nodes.get(&node_id) {
            Some(n) => n.labels.iter().copied().collect(),
            None => return,
        };
        let mut names: Vec<String> = Vec::new();
        for lid in &label_ids {
            for n in self.text_indexes.indexes_for_label(*lid) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        for name in names {
            let pkid = match self.text_indexes.get(&name) {
                Some(idx) => idx.property_key(),
                None => continue,
            };
            let value = self.node_string(node_id, pkid);
            if let Some(idx) = self.text_indexes.get_mut(&name) {
                match value {
                    Some(v) => idx.index_value(node_id, &v),
                    None => idx.remove(node_id),
                }
            }
        }
    }

    /// Remove a node from all text indexes (node deleted).
    pub(super) fn remove_from_text_indexes(&mut self, node_id: NodeId) {
        if self.text_indexes.is_empty() {
            return;
        }
        let names: Vec<String> = self
            .text_indexes
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        for name in names {
            if let Some(idx) = self.text_indexes.get_mut(&name) {
                idx.remove(node_id);
            }
        }
    }

    /// `([x, y, z], srid)` if the node property is a point.
    fn node_point(&self, node_id: NodeId, pkid: PropertyKeyId) -> Option<([f64; 3], i64)> {
        let node = self.nodes.get(&node_id)?;
        let pv = node.properties.get(pkid)?;
        spatial::point_coords(&pv)
    }

    /// Incrementally maintain point indexes after a node write.
    pub(super) fn maintain_spatial_indexes_for_node(&mut self, node_id: NodeId) {
        if self.spatial_indexes.is_empty() {
            return;
        }
        let label_ids: Vec<LabelId> = match self.nodes.get(&node_id) {
            Some(n) => n.labels.iter().copied().collect(),
            None => return,
        };
        let mut names: Vec<String> = Vec::new();
        for lid in &label_ids {
            for n in self.spatial_indexes.indexes_for_label(*lid) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        for name in names {
            let pkid = match self.spatial_indexes.get(&name) {
                Some(idx) => idx.property_key(),
                None => continue,
            };
            let point = self.node_point(node_id, pkid);
            if let Some(idx) = self.spatial_indexes.get_mut(&name) {
                match point {
                    Some((coords, srid)) => idx.index_point(node_id, coords, srid),
                    None => idx.remove_point(node_id),
                }
            }
        }
    }

    /// Remove a node from all point indexes (node deleted).
    pub(super) fn remove_from_spatial_indexes(&mut self, node_id: NodeId) {
        if self.spatial_indexes.is_empty() {
            return;
        }
        let names: Vec<String> = self
            .spatial_indexes
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        for name in names {
            if let Some(idx) = self.spatial_indexes.get_mut(&name) {
                idx.remove_point(node_id);
            }
        }
    }

    /// Upserts the node's vector into every vector index on one of its labels. Called from
    /// `add_node_to_indexes`.
    pub(super) fn maintain_vector_indexes_for_node(&mut self, node_id: NodeId) {
        if self.vector_indexes.is_empty() {
            return;
        }
        let label_ids: Vec<LabelId> = match self.nodes.get(&node_id) {
            Some(n) => n.labels.iter().copied().collect(),
            None => return,
        };
        for lid in label_ids {
            let names = self.vector_indexes.indexes_for_label(lid);
            for name in names {
                let pkid = match self.vector_indexes.get(&name) {
                    Some(v) => v.property_key,
                    None => continue,
                };
                match self.node_vector(node_id, pkid) {
                    Some(v) => {
                        if let Some(idx) = self.vector_indexes.get_mut(&name) {
                            idx.upsert(node_id, &v);
                        }
                    }
                    None => {
                        // no valid vector (lost or never had one): tombstone
                        if let Some(idx) = self.vector_indexes.get_mut(&name) {
                            idx.remove(node_id);
                        }
                    }
                }
            }
        }
    }

    /// Remove a node from all vector indexes (node deleted). Called from
    /// `remove_node_from_indexes`.
    pub(super) fn remove_from_vector_indexes(&mut self, node_id: NodeId) {
        if self.vector_indexes.is_empty() {
            return;
        }
        let names: Vec<String> = self.vector_indexes.iter().map(|v| v.name.clone()).collect();
        for name in names {
            if let Some(idx) = self.vector_indexes.get_mut(&name) {
                idx.remove(node_id);
            }
        }
    }
}
