//! Node and edge create, read, update and delete.

use std::borrow::Cow;

use smallvec::SmallVec;

use crate::error::{Error, Result};

use super::adjacency::AdjacencyStore;
use super::columns::Entity;
use super::edge::Edge;
use super::node::Node;
use super::property::{PropertyMap, PropertyValue};
use super::types::{EdgeEndpointStored, EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use super::{
    ADJ_ENTRY_COST, EDGE_BASE_COST, Graph, LABEL_INDEX_COST, NODE_BASE_COST, PROPERTY_BYTE,
    ProplessEdge, events,
};

impl Graph {
    /// Creates a node; labels and keys are interned. Returns its ID.
    pub fn create_node(&mut self, labels: &[&str], properties: PropertyMap) -> NodeId {
        let id = self.ids.reserve_node();
        self.create_node_with_id(id, labels, properties);
        events::record(events::GraphEvent::NodeCreated(id));
        id
    }

    /// Creates a node with an explicit ID: redo-log replay (later ops reference the
    /// recorded IDs) and transactions applying ids they reserved. Moves the id counter
    /// past `id`; overwrites an existing node, so replay is idempotent.
    pub fn create_node_with_id(&mut self, id: NodeId, labels: &[&str], properties: PropertyMap) {
        self.ids.bump_node(id);

        let label_ids: SmallVec<[LabelId; 2]> = labels
            .iter()
            .map(|l| {
                let lid = LabelId(self.strings.intern(l));
                self.label_index.entry(lid).or_default().insert(id);
                lid
            })
            .collect();

        let interned_props = self.intern_property_map(properties);

        let prop_bytes = interned_props.data_len();
        self.unlabeled += usize::from(label_ids.is_empty());
        let node = Node {
            id,
            labels: label_ids,
            properties: interned_props,
        };

        if let Some(replaced) = self.nodes.insert(id, node) {
            self.unlabeled -= usize::from(replaced.labels.is_empty());
        }

        if !self.bulk {
            self.dirty_nodes.insert(id);
        }
        if !labels.is_empty() {
            self.strings_dirty = true;
        }

        self.add_node_to_indexes(id);

        self.estimated_memory +=
            NODE_BASE_COST + labels.len() * LABEL_INDEX_COST + prop_bytes * PROPERTY_BYTE;
    }

    /// Node by ID, or an error if missing.
    pub fn get_node(&self, id: NodeId) -> Result<&Node> {
        self.nodes.get(&id).ok_or(Error::NodeNotFound(id))
    }

    /// One node property, without cloning the node.
    pub fn get_node_property(&self, id: NodeId, key: &str) -> Result<Option<PropertyValue>> {
        let node = self.nodes.get(&id).ok_or(Error::NodeNotFound(id))?;
        let kid = match self.strings.get(key) {
            Some(raw) => PropertyKeyId(raw),
            None => return Ok(None),
        };
        Ok(node.properties.get(kid))
    }

    /// Like `get_node_property` with a pre-resolved key (no string hash).
    pub fn get_node_property_kid(
        &self,
        id: NodeId,
        kid: PropertyKeyId,
    ) -> Result<Option<PropertyValue>> {
        let node = self.nodes.get(&id).ok_or(Error::NodeNotFound(id))?;
        Ok(node.properties.get(kid))
    }

    /// Batch property read into a flat buffer: `out[i * keys.len() + j]` = `keys[j]`
    /// of `node_ids[i]`. No locking; caller holds the guard.
    pub fn batch_read_node_properties(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out: &mut [Option<PropertyValue>],
    ) {
        let nk = keys.len();
        for (i, &nid) in node_ids.iter().enumerate() {
            let base = i * nk;
            if let Some(node) = self.nodes.get(&nid) {
                node.properties.get_multi(keys, &mut out[base..base + nk]);
            } else {
                for j in 0..nk {
                    out[base + j] = None;
                }
            }
        }
    }

    /// Typed i64 batch read, same layout as `batch_read_node_properties`.
    /// `out_valid[idx] = false` for missing nodes or keys and non-integers.
    ///
    /// Runs of the same NodeId (scan-expand fans one source out to many edges) copy
    /// the previous row instead of looking it up again.
    pub fn batch_read_node_properties_i64(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out_vals: &mut [i64],
        out_valid: &mut [bool],
    ) {
        let nk = keys.len();
        for (i, &nid) in node_ids.iter().enumerate() {
            let base = i * nk;
            // same NodeId as the previous row: copy its result
            if i > 0 && nid == node_ids[i - 1] {
                let prev_base = (i - 1) * nk;
                out_vals.copy_within(prev_base..prev_base + nk, base);
                out_valid.copy_within(prev_base..prev_base + nk, base);
                continue;
            }
            if let Some(node) = self.nodes.get(&nid) {
                node.properties.get_multi_int(
                    keys,
                    &mut out_vals[base..base + nk],
                    &mut out_valid[base..base + nk],
                );
            } else {
                for j in 0..nk {
                    out_valid[base + j] = false;
                }
            }
        }
    }

    /// One edge property, without cloning the edge.
    pub fn get_edge_property(&self, id: EdgeId, key: &str) -> Result<Option<PropertyValue>> {
        if self.propless.contains_key(&id) {
            return Ok(None);
        }
        let edge = self.edges.get(&id).ok_or(Error::EdgeNotFound(id))?;
        let kid = match self.strings.get(key) {
            Some(raw) => PropertyKeyId(raw),
            None => return Ok(None),
        };
        Ok(edge.properties.get(kid))
    }

    /// Mutable node by ID. Marks it dirty, since the caller may mutate it.
    pub fn get_node_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        let node = self.nodes.get_mut(&id).ok_or(Error::NodeNotFound(id))?;
        self.dirty_nodes.insert(id);
        Ok(node)
    }

    /// Replaces all properties on a node.
    pub fn update_node_properties(&mut self, id: NodeId, properties: PropertyMap) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }
        self.remove_node_from_indexes(id);
        let old_bytes = self.nodes[&id].properties.data_len();
        let interned = self.intern_property_map(properties);
        let new_bytes = interned.data_len();
        let node = self.nodes.get_mut(&id).unwrap();
        node.properties = interned;
        self.dirty_nodes.insert(id);
        self.add_node_to_indexes(id);
        // Adjust memory delta
        self.estimated_memory = self
            .estimated_memory
            .saturating_sub(old_bytes * PROPERTY_BYTE)
            + new_bytes * PROPERTY_BYTE;
        Ok(())
    }

    /// Adds a label to a node. Returns an error if the label is already present.
    pub fn add_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        let lid = LabelId(self.strings.intern(label));
        let node = self.nodes.get_mut(&id).ok_or(Error::NodeNotFound(id))?;

        if node.labels.contains(&lid) {
            return Err(Error::DuplicateLabel(id, label.to_owned()));
        }

        self.unlabeled -= usize::from(node.labels.is_empty());
        node.labels.push(lid);
        self.label_index.entry(lid).or_default().insert(id);
        self.dirty_nodes.insert(id);
        self.strings_dirty = true;

        // Add index entries for the newly added label
        self.add_node_to_indexes_for_label(id, lid);
        self.estimated_memory += LABEL_INDEX_COST;
        events::record(events::GraphEvent::LabelAdded {
            node: id,
            label: label.to_string(),
        });
        Ok(())
    }

    /// Removes a label from a node. Returns an error if the label is not present.
    pub fn remove_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        let lid = match self.strings.get(label) {
            Some(raw) => LabelId(raw),
            None => return Err(Error::LabelNotFound(id, label.to_owned())),
        };

        let node = self.nodes.get(&id).ok_or(Error::NodeNotFound(id))?;

        if !node.labels.contains(&lid) {
            return Err(Error::LabelNotFound(id, label.to_owned()));
        }

        // Remove index entries for this label before removing it
        self.remove_node_from_indexes_for_label(id, lid);

        let node = self.nodes.get_mut(&id).unwrap();
        let pos = node.labels.iter().position(|l| *l == lid).unwrap();

        node.labels.remove(pos);
        self.unlabeled += usize::from(node.labels.is_empty());
        if let Some(set) = self.label_index.get_mut(&lid) {
            set.remove(&id);
            if set.is_empty() {
                self.label_index.remove(&lid);
            }
        }
        self.dirty_nodes.insert(id);
        self.estimated_memory = self.estimated_memory.saturating_sub(LABEL_INDEX_COST);
        events::record(events::GraphEvent::LabelRemoved {
            node: id,
            label: label.to_string(),
        });
        Ok(())
    }

    /// Deletes a node. Fails if the node still has edges attached.
    pub fn delete_node(&mut self, id: NodeId) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }

        let has_out = !self.adj_out.neighbors(id).is_empty();
        let has_in = !self.adj_in.neighbors(id).is_empty();

        if has_out || has_in {
            return Err(Error::NodeHasEdges(id));
        }

        if events::capturing()
            && let Some(n) = self.nodes.get(&id)
        {
            events::record(events::GraphEvent::NodeDeleted {
                id,
                labels: n
                    .labels
                    .iter()
                    .map(|l| self.strings.resolve(l.0).to_string())
                    .collect(),
                properties: n
                    .properties
                    .iter()
                    .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v.clone()))
                    .collect(),
            });
        }
        self.remove_node_from_indexes(id);
        self.columns.removed(Entity::Node, id.0 as usize);

        let node = self.nodes.remove(&id).unwrap();
        let label_count = node.labels.len();
        self.unlabeled -= usize::from(label_count == 0);
        let prop_bytes = node.properties.data_len();
        for lid in &node.labels {
            if let Some(set) = self.label_index.get_mut(lid) {
                set.remove(&id);
                if set.is_empty() {
                    self.label_index.remove(lid);
                }
            }
        }
        self.adj_out.clear_node(id);
        self.adj_in.clear_node(id);

        self.deleted_nodes.insert(id);
        self.dirty_nodes.remove(&id);

        self.estimated_memory = self.estimated_memory.saturating_sub(
            NODE_BASE_COST + label_count * LABEL_INDEX_COST + prop_bytes * PROPERTY_BYTE,
        );

        Ok(())
    }

    /// Re-inserts a deleted node (undo log): labels, properties, adjacency, label and property
    /// index entries.
    pub fn restore_node(&mut self, node: Node) {
        let id = node.id;
        self.unlabeled += usize::from(node.labels.is_empty());
        for &lid in &node.labels {
            self.label_index.entry(lid).or_default().insert(id);
        }
        for kid in node.properties.keys() {
            self.known_prop_keys.insert(kid);
        }
        if let Some(replaced) = self.nodes.insert(id, node) {
            self.unlabeled -= usize::from(replaced.labels.is_empty());
        }
        self.dirty_nodes.insert(id);
        self.deleted_nodes.remove(&id);
        self.add_node_to_indexes(id);
    }

    /// Re-inserts a deleted edge with its adjacency entries (undo log).
    pub fn restore_edge(&mut self, edge: Edge) {
        let id = edge.id;
        let source = edge.source;
        let target = edge.target;
        let edge_type = edge.edge_type;
        for kid in edge.properties.keys() {
            self.known_prop_keys.insert(kid);
        }
        if edge.properties.is_empty() {
            self.propless.insert(
                id,
                ProplessEdge {
                    edge_type,
                    source,
                    target,
                },
            );
        } else {
            self.edges.insert(id, edge);
        }
        let out_ep = EdgeEndpointStored {
            edge_id: id,
            edge_type,
            neighbor: target,
        };
        let in_ep = EdgeEndpointStored {
            edge_id: id,
            edge_type,
            neighbor: source,
        };
        self.adj_out.push(source, out_ep);
        self.adj_in.push(target, in_ep);
        if source == target {
            self.loops += 1;
        }
        self.edge_type_index
            .entry(edge_type)
            .or_default()
            .insert(id);
        self.add_edge_to_indexes(id, edge_type);
        self.dirty_edges.insert(id);
        self.deleted_edges.remove(&id);
    }

    /// Deletes a node and all edges connected to it.
    pub fn delete_node_cascading(&mut self, id: NodeId) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            return Err(Error::NodeNotFound(id));
        }

        // collect connected edges (out + in)
        let mut edge_ids: Vec<EdgeId> = Vec::new();
        edge_ids.extend(self.adj_out.neighbors(id).iter().map(|e| e.edge_id()));
        edge_ids.extend(self.adj_in.neighbors(id).iter().map(|e| e.edge_id()));
        edge_ids.sort_unstable();
        edge_ids.dedup();

        for eid in edge_ids {
            // delete_edge handles all index cleanup
            let _ = self.delete_edge(eid);
        }

        // safe now, no edges left
        self.delete_node(id)
    }

    // -- Edge CRUD ------------------------------------------------------

    /// Creates a directed edge; the type is interned. Returns its ID.
    pub fn create_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        edge_type: &str,
        properties: PropertyMap,
    ) -> Result<EdgeId> {
        let id = self.ids.reserve_edge();
        let r = self.create_edge_with_id(id, source, target, edge_type, properties);
        if r.is_ok() {
            events::record(events::GraphEvent::EdgeCreated(id));
        }
        r
    }

    /// Creates an edge with an explicit ID (replay, reserved ids). Moves the id counter past `id`.
    pub fn create_edge_with_id(
        &mut self,
        id: EdgeId,
        source: NodeId,
        target: NodeId,
        edge_type: &str,
        properties: PropertyMap,
    ) -> Result<EdgeId> {
        if !self.nodes.contains_key(&source) {
            return Err(Error::SourceNodeNotFound(source));
        }
        if !self.nodes.contains_key(&target) {
            return Err(Error::TargetNodeNotFound(target));
        }

        self.ids.bump_edge(id);

        let type_id = EdgeTypeId(self.strings.intern(edge_type));
        let interned = self.intern_property_map(properties);

        let prop_bytes = interned.data_len();
        if interned.is_empty() {
            self.propless.insert(
                id,
                ProplessEdge {
                    edge_type: type_id,
                    source,
                    target,
                },
            );
        } else {
            let mut edge = Edge::new(id, type_id, source, target);
            edge.properties = interned;
            self.edges.insert(id, edge);
        }
        let out_ep = EdgeEndpointStored {
            edge_id: id,
            edge_type: type_id,
            neighbor: target,
        };
        let in_ep = EdgeEndpointStored {
            edge_id: id,
            edge_type: type_id,
            neighbor: source,
        };
        self.adj_out.push(source, out_ep);
        self.adj_in.push(target, in_ep);
        if source == target {
            self.loops += 1;
        }
        self.edge_type_index.entry(type_id).or_default().insert(id);

        self.add_edge_to_indexes(id, type_id);

        if !self.bulk {
            self.dirty_edges.insert(id);
        }
        self.strings_dirty = true;

        self.estimated_memory += EDGE_BASE_COST + ADJ_ENTRY_COST + prop_bytes * PROPERTY_BYTE;

        Ok(id)
    }

    /// Creates one node per property map, every one with `labels`, with consecutive ids from
    /// the returned one.
    pub fn bulk_nodes(&mut self, labels: &[&str], props: Vec<PropertyMap>) -> NodeId {
        let first = self.ids.reserve_nodes(props.len() as u64);
        for (i, p) in props.into_iter().enumerate() {
            let id = NodeId(first.0 + i as u64);
            self.create_node_with_id(id, labels, p);
            events::record(events::GraphEvent::NodeCreated(id));
        }
        first
    }

    /// Creates an edge of `edge_type` without properties per `(source, target)` pair, with
    /// consecutive ids from the returned one; what a single creation books per edge is booked
    /// once per call. Every endpoint must exist.
    pub fn bulk_edges(&mut self, edge_type: &str, pairs: &[(NodeId, NodeId)]) -> Result<EdgeId> {
        for &(s, t) in pairs {
            if !self.nodes.contains_key(&s) {
                return Err(Error::SourceNodeNotFound(s));
            }
            if !self.nodes.contains_key(&t) {
                return Err(Error::TargetNodeNotFound(t));
            }
        }
        let n = pairs.len();
        let first = self.ids.reserve_edges(n as u64);
        let type_id = EdgeTypeId(self.strings.intern(edge_type));
        // property indexes and columns are only there to maintain once something made them
        let maintain = self.edge_indexes.iter_prop().next().is_some()
            || self.columns.gather(Entity::Edge).active();
        let edge_at = |i: usize| EdgeId(first.0 + i as u64);
        if self.adj_out.is_empty() && self.adj_in.is_empty() {
            // the first edges of the graph: packed lists straight from the pairs
            let bound = self.ids.node_bound();
            let (out, inc) = rayon::join(
                || {
                    AdjacencyStore::try_from_edges(
                        || {
                            pairs
                                .iter()
                                .enumerate()
                                .map(|(i, &(s, t))| (s, edge_at(i), type_id, t))
                        },
                        bound,
                    )
                },
                || {
                    AdjacencyStore::try_from_edges(
                        || {
                            pairs
                                .iter()
                                .enumerate()
                                .map(|(i, &(s, t))| (t, edge_at(i), type_id, s))
                        },
                        bound,
                    )
                },
            );
            self.adj_out = out?;
            self.adj_in = inc?;
        } else {
            let out: Vec<(NodeId, EdgeEndpointStored)> = pairs
                .iter()
                .enumerate()
                .map(|(i, &(s, t))| {
                    (
                        s,
                        EdgeEndpointStored {
                            edge_id: edge_at(i),
                            edge_type: type_id,
                            neighbor: t,
                        },
                    )
                })
                .collect();
            self.adj_out.push_many(&out);
            drop(out);
            let inc: Vec<(NodeId, EdgeEndpointStored)> = pairs
                .iter()
                .enumerate()
                .map(|(i, &(s, t))| {
                    (
                        t,
                        EdgeEndpointStored {
                            edge_id: edge_at(i),
                            edge_type: type_id,
                            neighbor: s,
                        },
                    )
                })
                .collect();
            self.adj_in.push_many(&inc);
        }
        for (i, &(s, t)) in pairs.iter().enumerate() {
            let id = edge_at(i);
            self.propless.insert(
                id,
                ProplessEdge {
                    edge_type: type_id,
                    source: s,
                    target: t,
                },
            );
            if s == t {
                self.loops += 1;
            }
            if maintain {
                self.add_edge_to_indexes(id, type_id);
            }
            if !self.bulk {
                self.dirty_edges.insert(id);
            }
            events::record(events::GraphEvent::EdgeCreated(id));
        }
        let set = self.edge_type_index.entry(type_id).or_default();
        for i in 0..n as u64 {
            set.insert(EdgeId(first.0 + i));
        }
        self.strings_dirty = true;
        self.estimated_memory += n * (EDGE_BASE_COST + ADJ_ENTRY_COST);
        Ok(first)
    }

    /// Edge by ID: `Cow::Borrowed` if it has properties, `Cow::Owned` synthesized for property-less
    /// ones.
    pub fn get_edge(&self, id: EdgeId) -> Result<Cow<'_, Edge>> {
        if let Some(edge) = self.edges.get(&id) {
            Ok(Cow::Borrowed(edge))
        } else if let Some(pt) = self.propless.get(&id) {
            Ok(Cow::Owned(Edge::new(
                id,
                pt.edge_type,
                pt.source,
                pt.target,
            )))
        } else {
            Err(Error::EdgeNotFound(id))
        }
    }

    /// Returns `true` if the edge exists (in either map).
    pub fn has_edge(&self, id: EdgeId) -> bool {
        self.edges.contains_key(&id) || self.propless.contains_key(&id)
    }

    /// Mutable edge by ID. Promotes property-less edges into `edges` and marks the edge dirty.
    pub fn get_edge_mut(&mut self, id: EdgeId) -> Result<&mut Edge> {
        if self.propless.contains_key(&id) {
            self.promote_edge(id);
        }
        let edge = self.edges.get_mut(&id).ok_or(Error::EdgeNotFound(id))?;
        self.dirty_edges.insert(id);
        Ok(edge)
    }

    /// Moves a property-less edge into `edges` (e.g. before mutation).
    pub(super) fn promote_edge(&mut self, id: EdgeId) {
        if let Some(pt) = self.propless.remove(&id) {
            self.edges
                .insert(id, Edge::new(id, pt.edge_type, pt.source, pt.target));
        }
    }

    /// Replaces all properties on an edge.
    pub fn update_edge_properties(&mut self, id: EdgeId, properties: PropertyMap) -> Result<()> {
        if !self.edges.contains_key(&id) && !self.propless.contains_key(&id) {
            return Err(Error::EdgeNotFound(id));
        }
        let old_bytes = if let Some(e) = self.edges.get(&id) {
            e.properties.data_len()
        } else {
            0
        };
        // edge type, for index maintenance
        let type_id = if let Some(e) = self.edges.get(&id) {
            e.edge_type
        } else {
            self.propless[&id].edge_type
        };
        self.remove_edge_from_indexes(id, type_id);

        let interned = self.intern_property_map(properties);
        if interned.is_empty() && self.propless.contains_key(&id) {
            // was propless and still has no properties: re-add to indexes (no-op) and return
            self.add_edge_to_indexes(id, type_id);
            return Ok(());
        }
        let new_bytes = interned.data_len();
        if self.propless.contains_key(&id) {
            self.promote_edge(id);
        }
        let edge = self.edges.get_mut(&id).unwrap();
        edge.properties = interned;
        self.add_edge_to_indexes(id, type_id);
        self.dirty_edges.insert(id);
        self.estimated_memory = self
            .estimated_memory
            .saturating_sub(old_bytes * PROPERTY_BYTE)
            + new_bytes * PROPERTY_BYTE;
        Ok(())
    }

    /// Deletes an edge and removes it from all indexes.
    pub fn delete_edge(&mut self, id: EdgeId) -> Result<()> {
        if events::capturing() {
            if let Some(e) = self.edges.get(&id) {
                events::record(events::GraphEvent::EdgeDeleted {
                    id,
                    edge_type: self.strings.resolve(e.edge_type.0).to_string(),
                    source: e.source,
                    target: e.target,
                    properties: e
                        .properties
                        .iter()
                        .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v.clone()))
                        .collect(),
                });
            } else if let Some(pt) = self.propless.get(&id) {
                events::record(events::GraphEvent::EdgeDeleted {
                    id,
                    edge_type: self.strings.resolve(pt.edge_type.0).to_string(),
                    source: pt.source,
                    target: pt.target,
                    properties: Vec::new(),
                });
            }
        }
        // drop from edge property indexes before removing storage
        let edge_type_for_idx = if let Some(e) = self.edges.get(&id) {
            Some(e.edge_type)
        } else {
            self.propless.get(&id).map(|pt| pt.edge_type)
        };
        if let Some(type_id) = edge_type_for_idx {
            self.remove_edge_from_indexes(id, type_id);
        }
        self.columns.removed(Entity::Edge, id.0 as usize);

        let (source, target, edge_type, prop_bytes) = if let Some(edge) = self.edges.remove(&id) {
            let pb = edge.properties.data_len();
            (edge.source, edge.target, edge.edge_type, pb)
        } else if let Some(pt) = self.propless.remove(&id) {
            (pt.source, pt.target, pt.edge_type, 0)
        } else {
            return Err(Error::EdgeNotFound(id));
        };

        self.adj_out.remove(source, id);
        self.adj_in.remove(target, id);
        if source == target {
            self.loops = self.loops.saturating_sub(1);
        }
        if let Some(set) = self.edge_type_index.get_mut(&edge_type) {
            set.remove(&id);
            if set.is_empty() {
                self.edge_type_index.remove(&edge_type);
            }
        }

        self.deleted_edges.insert(id);
        self.dirty_edges.remove(&id);

        self.estimated_memory = self
            .estimated_memory
            .saturating_sub(EDGE_BASE_COST + ADJ_ENTRY_COST + prop_bytes * PROPERTY_BYTE);

        Ok(())
    }
}
