//! [`GraphView`](crate::tx::view::GraphView) over the snapshot plus the local delta.

use std::borrow::Cow;
use std::ops::{Bound, RangeBounds};

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::error::{Error, Result};
use crate::graph::batch::{AdjBatch, ColumnSink};
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

use super::super::view::GraphView;
use super::{OptimisticTx, SchemaKind, SchemaOp};

impl GraphView for OptimisticTx<'_> {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        Some(self)
    }

    fn write_count(&self) -> u64 {
        OptimisticTx::write_count(self)
    }

    fn is_explicit_transaction(&self) -> bool {
        true
    }

    fn data_version(&self) -> Option<(usize, u64)> {
        if self.local.is_empty() {
            self.base.data_version()
        } else {
            None
        }
    }

    fn adjacency_snapshot(&self) -> Option<crate::graph::adjacency::AdjacencySnapshot<'_>> {
        if self.local.is_empty() {
            self.base.adjacency_snapshot()
        } else {
            None
        }
    }

    fn highest_node_id(&self) -> u64 {
        // the shared counter is past every id reserved, ours included
        self.base.highest_node_id()
    }

    fn get_node(&self, id: NodeId) -> Result<Cow<'_, Node>> {
        self.node_ref(id).ok_or(Error::NodeNotFound(id))
    }

    fn get_edge(&self, id: EdgeId) -> Result<Cow<'_, Edge>> {
        self.edge_ref(id).ok_or(Error::EdgeNotFound(id))
    }

    fn has_label(&self, node: NodeId, label: LabelId) -> Result<bool> {
        Ok(self
            .node_ref(node)
            .is_some_and(|n| n.labels.contains(&label)))
    }

    fn node_count(&self) -> usize {
        (self.base.node_count() as i64 + self.local.node_delta).max(0) as usize
    }

    fn edge_count(&self) -> usize {
        (self.base.edge_count() as i64 + self.local.edge_delta).max(0) as usize
    }

    fn nodes_vec(&self) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = self
            .base
            .nodes_vec()
            .into_iter()
            .filter(|id| !matches!(self.local.nodes.get(id), Some(None)))
            .collect();
        let mut created: Vec<NodeId> = self.local.created_nodes.iter().copied().collect();
        created.sort_unstable();
        out.extend(created);
        out
    }

    fn all_edges_vec(&self) -> Vec<EdgeId> {
        let mut out: Vec<EdgeId> = self
            .base
            .all_edges_vec()
            .into_iter()
            .filter(|id| !matches!(self.local.edges.get(id), Some(None)))
            .collect();
        let mut created: Vec<EdgeId> = self.local.created_edges.iter().copied().collect();
        created.sort_unstable();
        out.extend(created);
        out
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        let Some(lid) = self.strings.get(label).map(LabelId) else {
            return Vec::new();
        };
        let mut out: Vec<NodeId> = self
            .base
            .nodes_by_label(label)
            .into_iter()
            .filter(|id| !self.local.nodes.contains_key(id))
            .collect();
        out.extend(self.local_with_label(lid));
        out
    }

    fn edges_by_type(&self, edge_type: &str) -> Vec<EdgeId> {
        let Some(tid) = self.strings.get(edge_type).map(EdgeTypeId) else {
            return Vec::new();
        };
        let mut out: Vec<EdgeId> = self
            .base
            .edges_by_type(edge_type)
            .into_iter()
            .filter(|id| !self.local.edges.contains_key(id))
            .collect();
        out.extend(self.local.types.get(&tid).into_iter().flatten().copied());
        out
    }

    fn label_names(&self) -> Vec<String> {
        let mut names = self.base.label_names();
        for l in self.local.labels.keys() {
            let n = self.strings.resolve(l.0);
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
        names
    }

    fn relationship_type_names(&self) -> Vec<String> {
        let mut names = self.base.relationship_type_names();
        for t in self.local.types.keys() {
            let n = self.strings.resolve(t.0);
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
        names
    }

    fn property_key_names(&self) -> Vec<String> {
        let mut names = self.base.property_key_names();
        let mut seen: FxHashSet<PropertyKeyId> = FxHashSet::default();
        let local_keys = self
            .local
            .nodes
            .values()
            .flatten()
            .flat_map(|n| n.properties.keys())
            .chain(
                self.local
                    .edges
                    .values()
                    .flatten()
                    .flat_map(|e| e.properties.keys()),
            );
        for k in local_keys {
            if seen.insert(k) {
                let n = self.strings.resolve(k.0);
                if !names.iter().any(|x| x == n) {
                    names.push(n.to_string());
                }
            }
        }
        names
    }

    fn neighbors(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<NodeId>> {
        if self.adjacency_is_base(node) {
            return self.base.neighbors(node, dir, filter);
        }
        let tf = match filter {
            None => None,
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return self.edges_at(node, dir).map(|_| Vec::new()),
            },
        };
        Ok(self
            .edges_at(node, dir)?
            .into_iter()
            .filter(|e| tf.is_none_or(|f| f == e.1))
            .map(|e| e.2)
            .collect())
    }

    fn edges_of(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<EdgeId>> {
        if self.adjacency_is_base(node) {
            return self.base.edges_of(node, dir, filter);
        }
        let tf = match filter {
            None => None,
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return self.edges_at(node, dir).map(|_| Vec::new()),
            },
        };
        Ok(self
            .edges_at(node, dir)?
            .into_iter()
            .filter(|e| tf.is_none_or(|f| f == e.1))
            .map(|e| e.0)
            .collect())
    }

    fn gather_node_props(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        if self.local.nodes.is_empty() {
            return self.base.gather_node_props(ids, keys, sinks);
        }
        let mut missing = 0;
        for &id in ids {
            match self.local.nodes.get(&id) {
                Some(Some(n)) => n.properties.get_multi_into(keys, sinks),
                Some(None) => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
                None => missing += self.base.gather_node_props(&[id], keys, sinks)?,
            }
        }
        Ok(missing)
    }

    fn gather_edge_props(
        &self,
        ids: &[EdgeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        if self.local.edges.is_empty() {
            return self.base.gather_edge_props(ids, keys, sinks);
        }
        let mut missing = 0;
        for &id in ids {
            match self.local.edges.get(&id) {
                Some(Some(e)) => e.properties.get_multi_into(keys, sinks),
                Some(None) => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
                None => missing += self.base.gather_edge_props(&[id], keys, sinks)?,
            }
        }
        Ok(missing)
    }

    fn nodes_exist(&self, ids: &[NodeId], out: &mut Vec<bool>) {
        if self.local.nodes.is_empty() {
            return self.base.nodes_exist(ids, out);
        }
        out.clear();
        out.extend(ids.iter().map(|&id| self.node_ref(id).is_some()));
    }

    fn edges_exist(&self, ids: &[EdgeId], out: &mut Vec<bool>) {
        if self.local.edges.is_empty() {
            return self.base.edges_exist(ids, out);
        }
        out.clear();
        out.extend(ids.iter().map(|&id| self.edge_ref(id).is_some()));
    }

    fn nodes_have_label(&self, ids: &[NodeId], label: LabelId, out: &mut Vec<bool>) {
        if self.local.nodes.is_empty() {
            return self.base.nodes_have_label(ids, label, out);
        }
        out.clear();
        out.extend(
            ids.iter()
                .map(|&id| self.node_ref(id).is_some_and(|n| n.labels.contains(&label))),
        );
    }

    fn count_nodes_by_label(&self, label: &str) -> usize {
        if self.local.nodes.is_empty() {
            return self.base.count_nodes_by_label(label);
        }
        self.nodes_by_label(label).len()
    }

    fn count_edges_by_type(&self, edge_type: &str) -> usize {
        if self.local.edges.is_empty() {
            return self.base.count_edges_by_type(edge_type);
        }
        self.edges_by_type(edge_type).len()
    }

    fn count_index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<usize> {
        if self.local.is_empty() {
            return self.base.count_index_lookup(label, prop, value);
        }
        self.index_lookup(label, prop, value).map(|v| v.len())
    }

    fn count_index_range(
        &self,
        label: &str,
        prop: &str,
        lower: Bound<IndexKey>,
        upper: Bound<IndexKey>,
    ) -> Option<usize> {
        if self.local.is_empty() {
            return self.base.count_index_range(label, prop, lower, upper);
        }
        self.index_range_lookup(label, prop, lower, upper)
            .map(|v| v.len())
    }

    fn index_range_estimate(
        &self,
        label: &str,
        prop: &str,
        lower: &Bound<IndexKey>,
        upper: &Bound<IndexKey>,
    ) -> Option<f64> {
        if self.local.is_empty() {
            self.base.index_range_estimate(label, prop, lower, upper)
        } else {
            None
        }
    }

    fn expand_batch(
        &self,
        srcs: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut AdjBatch,
    ) -> Result<()> {
        if srcs.iter().all(|&n| self.adjacency_is_base(n)) {
            return self.base.expand_batch(srcs, dir, type_filter, out);
        }
        for (lane, &src) in srcs.iter().enumerate() {
            let lane_u = lane as u32;
            if self.adjacency_is_base(src) {
                let before = out.len();
                self.base.expand_batch(&[src], dir, type_filter, out)?;
                for l in &mut out.src_lane[before..] {
                    *l = lane_u;
                }
                continue;
            }
            for (edge_id, edge_type, neighbor, outgoing) in self.edges_at(src, dir)? {
                if type_filter.as_ref().is_none_or(|f| f.contains(&edge_type)) {
                    if dir == Direction::Both && neighbor == src && !outgoing {
                        continue;
                    }
                    out.push(lane_u, edge_id, edge_type, neighbor, outgoing);
                }
            }
        }
        Ok(())
    }

    fn edges_with_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        if self.adjacency_is_base(node) {
            return self.base.edges_with_neighbor(node, dir, filter);
        }
        let tf = match filter {
            None => None,
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return self.edges_at(node, dir).map(|_| Vec::new()),
            },
        };
        Ok(self
            .edges_at(node, dir)?
            .into_iter()
            .filter(|e| tf.is_none_or(|f| f == e.1))
            .map(|(edge_id, edge_type, neighbor, outgoing)| EdgeEndpoint {
                edge_id,
                edge_type,
                neighbor,
                source: if outgoing { node } else { neighbor },
                target: if outgoing { neighbor } else { node },
            })
            .collect())
    }

    fn neighbors_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        if self.adjacency_is_base(node) {
            return self.base.neighbors_filtered(node, dir, type_filter);
        }
        Ok(self
            .edges_at(node, dir)?
            .into_iter()
            .filter(|e| type_filter.as_ref().is_none_or(|f| f.contains(&e.1)))
            .map(|e| e.2)
            .collect())
    }

    fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        if self.adjacency_is_base(node) {
            return self.base.edge_endpoints_filtered(node, dir, type_filter);
        }
        Ok(self
            .edges_at(node, dir)?
            .into_iter()
            .filter(|e| type_filter.as_ref().is_none_or(|f| f.contains(&e.1)))
            .map(|e| (e.0, e.2))
            .collect())
    }

    fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        if self.adjacency_is_base(from) {
            return self.base.edges_between(from, to, dir, type_filter);
        }
        Ok(self
            .edges_at(from, dir)?
            .into_iter()
            .filter(|e| e.2 == to && type_filter.as_ref().is_none_or(|f| f.contains(&e.1)))
            .map(|e| e.0)
            .collect())
    }

    fn for_each_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        if self.adjacency_is_base(node) {
            return self.base.for_each_neighbor(node, dir, type_filter, f);
        }
        for e in self.edges_at(node, dir)? {
            if type_filter.as_ref().is_none_or(|tf| tf.contains(&e.1)) {
                f(e.2);
            }
        }
        Ok(())
    }

    fn expand_frontier(
        &self,
        nodes: &[NodeId],
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut FxHashSet<NodeId>,
    ) -> Result<()> {
        let (base, local): (Vec<NodeId>, Vec<NodeId>) =
            nodes.iter().partition(|&&n| self.adjacency_is_base(n));
        self.base
            .expand_frontier(&base, direction, type_filter, out)?;
        for n in local {
            let Ok(edges) = self.edges_at(n, direction) else {
                continue;
            };
            for e in edges {
                if type_filter.as_ref().is_none_or(|tf| tf.contains(&e.1)) {
                    out.insert(e.2);
                }
            }
        }
        Ok(())
    }

    fn degree(&self, node: NodeId, dir: Direction) -> Result<usize> {
        if self.adjacency_is_base(node) {
            return self.base.degree(node, dir);
        }
        Ok(self.edges_at(node, dir)?.len())
    }

    fn self_loops(&self) -> Option<usize> {
        if !OptimisticTx::has_writes(self) {
            self.base.self_loops()
        } else {
            None
        }
    }

    fn count_nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: crate::graph::batch::ColumnCmp,
        bound: &crate::graph::batch::ColumnBound,
    ) -> Option<usize> {
        if !OptimisticTx::has_writes(self) {
            self.base.count_nodes_where(label, prop, cmp, bound)
        } else {
            None
        }
    }

    fn nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: crate::graph::batch::ColumnCmp,
        bound: &crate::graph::batch::ColumnBound,
    ) -> Option<Vec<NodeId>> {
        if !OptimisticTx::has_writes(self) {
            self.base.nodes_where(label, prop, cmp, bound)
        } else {
            None
        }
    }

    fn label_bitmap(&self, label: &str) -> Option<Vec<u64>> {
        if !OptimisticTx::has_writes(self) {
            self.base.label_bitmap(label)
        } else {
            None
        }
    }

    fn degrees(&self, ids: &[NodeId], dir: Direction, out: &mut Vec<usize>) {
        out.clear();
        out.resize(ids.len(), 0);
        let mut base_ids = Vec::new();
        let mut base_at = Vec::new();
        for (i, &n) in ids.iter().enumerate() {
            if self.adjacency_is_base(n) {
                base_ids.push(n);
                base_at.push(i);
            } else {
                out[i] = self.edges_at(n, dir).map(|e| e.len()).unwrap_or(0);
            }
        }
        let mut base = Vec::new();
        self.base.degrees(&base_ids, dir, &mut base);
        for (i, d) in base_at.into_iter().zip(base) {
            out[i] = d;
        }
    }

    fn get_node_property(&self, id: NodeId, key: &str) -> Result<Option<PropertyValue>> {
        let n = self.node_ref(id).ok_or(Error::NodeNotFound(id))?;
        Ok(self
            .strings
            .get(key)
            .and_then(|k| n.properties.get(PropertyKeyId(k))))
    }

    fn get_node_property_kid(
        &self,
        id: NodeId,
        kid: PropertyKeyId,
    ) -> Result<Option<PropertyValue>> {
        let n = self.node_ref(id).ok_or(Error::NodeNotFound(id))?;
        Ok(n.properties.get(kid))
    }

    fn get_edge_property(&self, id: EdgeId, key: &str) -> Result<Option<PropertyValue>> {
        let e = self.edge_ref(id).ok_or(Error::EdgeNotFound(id))?;
        Ok(self
            .strings
            .get(key)
            .and_then(|k| e.properties.get(PropertyKeyId(k))))
    }

    fn batch_read_node_properties(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out: &mut [Option<PropertyValue>],
    ) -> Result<()> {
        self.base.batch_read_node_properties(node_ids, keys, out)?;
        if self.local.nodes.is_empty() {
            return Ok(());
        }
        let nk = keys.len();
        for (i, id) in node_ids.iter().enumerate() {
            let row = &mut out[i * nk..(i + 1) * nk];
            match self.local.nodes.get(id) {
                Some(Some(n)) => n.properties.get_multi(keys, row),
                Some(None) => row.iter_mut().for_each(|v| *v = None),
                None => {}
            }
        }
        Ok(())
    }

    fn batch_read_node_properties_i64(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out_vals: &mut [i64],
        out_valid: &mut [bool],
    ) -> Result<()> {
        self.base
            .batch_read_node_properties_i64(node_ids, keys, out_vals, out_valid)?;
        if self.local.nodes.is_empty() {
            return Ok(());
        }
        let nk = keys.len();
        for (i, id) in node_ids.iter().enumerate() {
            let (lo, hi) = (i * nk, (i + 1) * nk);
            match self.local.nodes.get(id) {
                Some(Some(n)) => {
                    n.properties
                        .get_multi_int(keys, &mut out_vals[lo..hi], &mut out_valid[lo..hi])
                }
                Some(None) => out_valid[lo..hi].iter_mut().for_each(|v| *v = false),
                None => {}
            }
        }
        Ok(())
    }

    fn strings(&self) -> &StringTable {
        &self.strings
    }

    fn statistics(&self) -> Option<&GraphStatistics> {
        self.base.statistics()
    }

    fn index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        let mut out: Vec<NodeId> = self
            .base
            .index_lookup(label, prop, value)?
            .into_iter()
            .filter(|id| !self.local.nodes.contains_key(id))
            .collect();
        let (Some(lid), Some(pk)) = (self.strings.get(label), self.strings.get(prop)) else {
            return Some(out);
        };
        let (lid, pk) = (LabelId(lid), PropertyKeyId(pk));
        let listed = self
            .local
            .node_index
            .get(&(lid.0, pk))
            .and_then(|t| t.get(value))
            .into_iter()
            .flatten()
            .copied();
        out.extend(self.local_index_hits(lid, pk, listed, &|k| k == value));
        Some(out)
    }

    fn index_range_lookup(
        &self,
        label: &str,
        prop: &str,
        lower: Bound<IndexKey>,
        upper: Bound<IndexKey>,
    ) -> Option<Vec<NodeId>> {
        let mut out: Vec<NodeId> = self
            .base
            .index_range_lookup(label, prop, lower.clone(), upper.clone())?
            .into_iter()
            .filter(|id| !self.local.nodes.contains_key(id))
            .collect();
        let (Some(lid), Some(pk)) = (self.strings.get(label), self.strings.get(prop)) else {
            return Some(out);
        };
        let (lid, pk) = (LabelId(lid), PropertyKeyId(pk));
        let range = (lower.as_ref(), upper.as_ref());
        let listed = self
            .local
            .node_index
            .get(&(lid.0, pk))
            .into_iter()
            .flat_map(|t| t.range(range))
            .flat_map(|(_, ids)| ids.iter().copied());
        let mut hits = self.local_index_hits(lid, pk, listed, &|k| range.contains(k));
        hits.sort_unstable();
        hits.dedup();
        out.extend(hits);
        Some(out)
    }

    fn index_catalog(&self) -> &IndexCatalog {
        self.base.index_catalog()
    }

    fn index_size(&self, name: &str) -> Option<usize> {
        self.base.index_size(name)
    }

    fn has_vector_index(&self, name: &str) -> bool {
        self.base.has_vector_index(name)
    }

    fn query_vector_index(
        &self,
        name: &str,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Option<Vec<(NodeId, f64)>> {
        if self.local.nodes.is_empty() {
            return self.base.query_vector_index(name, query, k, ef);
        }
        let _guard = self.store.graph_rw.read();
        // SAFETY: under the structural read lock
        let g = unsafe { self.store.graph() };
        let (label, _) = g.vector_index_target(name)?;
        let score = |n: &Node| {
            if n.labels.contains(&label) {
                g.vector_score(name, query, &n.properties)
            } else {
                None
            }
        };
        let base = self
            .base
            .query_vector_index(name, query, k + self.local.nodes.len(), ef)?;
        Some(self.merge_top_k(base, k, &score, true))
    }

    fn vector_index_descriptors(&self) -> Vec<(String, String, String, usize, &'static str)> {
        self.base.vector_index_descriptors()
    }

    fn has_fulltext_index(&self, name: &str) -> bool {
        self.base.has_fulltext_index(name)
    }

    fn query_fulltext_index(
        &self,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Option<Vec<(NodeId, f64)>> {
        if self.local.nodes.is_empty() {
            return self.base.query_fulltext_index(name, query, limit);
        }
        let _guard = self.store.graph_rw.read();
        // SAFETY: under the structural read lock
        let g = unsafe { self.store.graph() };
        let (labels, _) = g.fulltext_index_target(name)?;
        let score = |n: &Node| {
            if labels.iter().any(|l| n.labels.contains(l)) {
                g.fulltext_score(name, query, &n.properties)
            } else {
                None
            }
        };
        let base = self
            .base
            .query_fulltext_index(name, query, limit + self.local.nodes.len())?;
        Some(self.merge_top_k(base, limit, &score, true))
    }

    fn fulltext_index_descriptors(&self) -> Vec<(String, Vec<String>, Vec<String>, String)> {
        self.base.fulltext_index_descriptors()
    }

    fn has_point_index(&self, name: &str) -> bool {
        self.base.has_point_index(name)
    }

    fn has_point_index_for(&self, label: &str, property: &str) -> bool {
        self.base.has_point_index_for(label, property)
    }

    fn point_index_within_bbox(
        &self,
        label: &str,
        property: &str,
        ll: [f64; 3],
        ur: [f64; 3],
        srid: i64,
    ) -> Option<Vec<NodeId>> {
        let mut out: Vec<NodeId> = self
            .base
            .point_index_within_bbox(label, property, ll, ur, srid)?
            .into_iter()
            .filter(|id| !self.local.nodes.contains_key(id))
            .collect();
        let (Some(lid), Some(pk)) = (self.strings.get(label), self.strings.get(property)) else {
            return Some(out);
        };
        for (id, n) in self.live_local_nodes() {
            if n.labels.contains(&LabelId(lid))
                && Self::point_of(n, PropertyKeyId(pk), srid)
                    .is_some_and(|p| crate::graph::spatial::in_bbox(p, ll, ur, srid))
            {
                out.push(id);
            }
        }
        Some(out)
    }

    fn point_index_within_distance(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        radius: f64,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        let mut out: Vec<(NodeId, f64)> = self
            .base
            .point_index_within_distance(label, property, center, radius, srid)?
            .into_iter()
            .filter(|h| !self.local.nodes.contains_key(&h.0))
            .collect();
        let (Some(lid), Some(pk)) = (self.strings.get(label), self.strings.get(property)) else {
            return Some(out);
        };
        for (id, n) in self.live_local_nodes() {
            if !n.labels.contains(&LabelId(lid)) {
                continue;
            }
            if let Some(p) = Self::point_of(n, PropertyKeyId(pk), srid) {
                let d = crate::graph::spatial::point_dist(p, center, srid);
                if d <= radius {
                    out.push((id, d));
                }
            }
        }
        Some(out)
    }

    fn point_index_knn(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        k: usize,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        let base =
            self.base
                .point_index_knn(label, property, center, k + self.local.nodes.len(), srid)?;
        let (Some(lid), Some(pk)) = (self.strings.get(label), self.strings.get(property)) else {
            return Some(self.merge_top_k(base, k, &|_| None, false));
        };
        let dist = |n: &Node| {
            if !n.labels.contains(&LabelId(lid)) {
                return None;
            }
            let p = Self::point_of(n, PropertyKeyId(pk), srid)?;
            Some(crate::graph::spatial::point_dist(p, center, srid))
        };
        Some(self.merge_top_k(base, k, &dist, false))
    }

    fn point_index_descriptors(&self) -> Vec<(String, String, String)> {
        self.base.point_index_descriptors()
    }

    fn has_text_index(&self, name: &str) -> bool {
        self.base.has_text_index(name)
    }

    fn has_text_index_for(&self, label: &str, property: &str) -> bool {
        self.base.has_text_index_for(label, property)
    }

    fn text_index_candidates(
        &self,
        label: &str,
        property: &str,
        kind: crate::graph::text::TextQueryKind,
        pattern: &str,
    ) -> Option<Vec<NodeId>> {
        // a superset the caller re-checks: the snapshot's candidates we didn't touch, plus every
        // live local node with the label
        let mut out: Vec<NodeId> = self
            .base
            .text_index_candidates(label, property, kind, pattern)?
            .into_iter()
            .filter(|id| !self.local.nodes.contains_key(id))
            .collect();
        if let Some(lid) = self.strings.get(label) {
            out.extend(
                self.live_local_nodes()
                    .filter(|(_, n)| n.labels.contains(&LabelId(lid)))
                    .map(|(id, _)| id),
            );
        }
        Some(out)
    }

    fn text_index_descriptors(&self) -> Vec<(String, String, String)> {
        self.base.text_index_descriptors()
    }

    fn constraint_registry(&self) -> &ConstraintRegistry {
        self.base.constraint_registry()
    }

    fn edge_index_lookup(
        &self,
        edge_type: &str,
        prop: &str,
        value: &IndexKey,
    ) -> Option<Vec<EdgeId>> {
        let mut out: Vec<EdgeId> = self
            .base
            .edge_index_lookup(edge_type, prop, value)?
            .into_iter()
            .filter(|id| !self.local.edges.contains_key(id))
            .collect();
        let (Some(tid), Some(pk)) = (self.strings.get(edge_type), self.strings.get(prop)) else {
            return Some(out);
        };
        let pk = PropertyKeyId(pk);
        out.extend(
            self.local
                .edge_index
                .get(&(tid, pk))
                .and_then(|t| t.get(value))
                .into_iter()
                .flatten()
                .copied()
                .filter(|id| !self.local.stale_edges.contains(id)),
        );
        for &id in &self.local.stale_edges {
            if let Some(Some(e)) = self.local.edges.get(&id)
                && e.edge_type.0 == tid
                && self
                    .index_key_of(&e.properties, pk)
                    .is_some_and(|k| k == *value)
            {
                out.push(id);
            }
        }
        Some(out)
    }

    fn edge_index_catalog(&self) -> &EdgeIndexCatalog {
        self.base.edge_index_catalog()
    }

    // -- writes --

    fn create_node(&mut self, labels: &[&str], props: PropertyMap) -> Result<NodeId> {
        self.writable()?;
        self.check_budget(props.len())?;
        let id = self.store.ids.reserve_node();
        let labels: SmallVec<[LabelId; 2]> = labels
            .iter()
            .map(|l| LabelId(self.strings.intern(l)))
            .collect();
        let properties = self.intern_map(props);
        self.local.nodes.insert(
            id,
            Some(Node {
                id,
                labels,
                properties,
            }),
        );
        self.local.created_nodes.insert(id);
        self.local.node_delta += 1;
        self.relist_node(id);
        Ok(id)
    }

    fn create_edge(
        &mut self,
        src: NodeId,
        tgt: NodeId,
        edge_type: &str,
        props: PropertyMap,
    ) -> Result<EdgeId> {
        self.writable()?;
        if self.node_ref(src).is_none() {
            return Err(Error::SourceNodeNotFound(src));
        }
        if self.node_ref(tgt).is_none() {
            return Err(Error::TargetNodeNotFound(tgt));
        }
        self.check_budget(props.len())?;
        let id = self.store.ids.reserve_edge();
        let edge_type = EdgeTypeId(self.strings.intern(edge_type));
        let mut edge = Edge::new(id, edge_type, src, tgt);
        edge.properties = self.intern_map(props);
        self.local.edges.insert(id, Some(edge));
        self.local.created_edges.insert(id);
        self.local.out_added.entry(src).or_default().push(id);
        self.local.in_added.entry(tgt).or_default().push(id);
        self.local.adjacency_changed.insert(src);
        self.local.adjacency_changed.insert(tgt);
        self.local.edge_delta += 1;
        self.relist_edge(id);
        Ok(id)
    }

    fn update_node_properties(&mut self, id: NodeId, props: PropertyMap) -> Result<()> {
        self.writable()?;
        let properties = self.intern_map(props);
        self.own_node(id)?.properties = properties;
        self.relist_node(id);
        Ok(())
    }

    fn update_edge_properties(&mut self, id: EdgeId, props: PropertyMap) -> Result<()> {
        self.writable()?;
        let properties = self.intern_map(props);
        self.own_edge(id)?.properties = properties;
        self.relist_edge(id);
        Ok(())
    }

    fn delete_node(&mut self, id: NodeId) -> Result<()> {
        self.writable()?;
        if !self.edges_at(id, Direction::Both)?.is_empty() {
            return Err(Error::NodeHasEdges(id));
        }
        self.local.unlist_node(id);
        self.local.stale_nodes.remove(&id);
        if self.local.created_nodes.remove(&id) {
            self.local.nodes.remove(&id);
            self.local.out_added.remove(&id);
            self.local.in_added.remove(&id);
        } else {
            self.local.nodes.insert(id, None);
        }
        self.local.node_delta -= 1;
        Ok(())
    }

    fn delete_node_cascading(&mut self, id: NodeId) -> Result<()> {
        self.writable()?;
        let mut edges: Vec<EdgeId> = self
            .edges_at(id, Direction::Both)?
            .into_iter()
            .map(|e| e.0)
            .collect();
        edges.sort_unstable();
        edges.dedup();
        for e in edges {
            self.delete_edge(e)?;
        }
        self.delete_node(id)
    }

    fn delete_edge(&mut self, id: EdgeId) -> Result<()> {
        self.writable()?;
        let (source, target) = {
            let e = self.edge_ref(id).ok_or(Error::EdgeNotFound(id))?;
            (e.source, e.target)
        };
        self.local.unlist_edge(id);
        self.local.stale_edges.remove(&id);
        if self.local.created_edges.remove(&id) {
            self.local.edges.remove(&id);
            if let Some(v) = self.local.out_added.get_mut(&source) {
                v.retain(|e| *e != id);
            }
            if let Some(v) = self.local.in_added.get_mut(&target) {
                v.retain(|e| *e != id);
            }
        } else {
            self.local.edges.insert(id, None);
            self.local.adjacency_changed.insert(source);
            self.local.adjacency_changed.insert(target);
        }
        self.local.edge_delta -= 1;
        Ok(())
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        self.writable()?;
        let lid = LabelId(self.strings.intern(label));
        let node = self.own_node(id)?;
        if node.labels.contains(&lid) {
            return Err(Error::DuplicateLabel(id, label.to_owned()));
        }
        node.labels.push(lid);
        self.relist_node(id);
        Ok(())
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        self.writable()?;
        let Some(lid) = self.strings.get(label).map(LabelId) else {
            return Err(Error::LabelNotFound(id, label.to_owned()));
        };
        let node = self.own_node(id)?;
        if !node.labels.contains(&lid) {
            return Err(Error::LabelNotFound(id, label.to_owned()));
        }
        node.labels.retain(|l| *l != lid);
        self.relist_node(id);
        Ok(())
    }

    fn get_node_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        self.writable()?;
        self.own_node(id)?;
        self.local.stale_nodes.insert(id);
        match self.local.nodes.get_mut(&id) {
            Some(Some(n)) => Ok(n),
            _ => Err(Error::NodeNotFound(id)),
        }
    }

    fn get_edge_mut(&mut self, id: EdgeId) -> Result<&mut Edge> {
        self.writable()?;
        self.own_edge(id)?;
        self.local.stale_edges.insert(id);
        match self.local.edges.get_mut(&id) {
            Some(Some(e)) => Ok(e),
            _ => Err(Error::EdgeNotFound(id)),
        }
    }

    // the index bracket: listings refresh on the closing call; until then reads re-check the
    // object (it's stale)

    fn unindex_node(&mut self, id: NodeId) {
        if self.local.nodes.contains_key(&id) {
            self.local.stale_nodes.insert(id);
        }
    }

    fn index_node(&mut self, id: NodeId) {
        if self.local.nodes.contains_key(&id) {
            self.relist_node(id);
        }
    }

    fn unindex_node_prop(&mut self, id: NodeId, _key: &str) {
        self.unindex_node(id);
    }

    fn index_node_prop(&mut self, id: NodeId, _key: &str) {
        self.index_node(id);
    }

    fn unindex_edge(&mut self, id: EdgeId) {
        if self.local.edges.contains_key(&id) {
            self.local.stale_edges.insert(id);
        }
    }

    fn index_edge(&mut self, id: EdgeId) {
        if self.local.edges.contains_key(&id) {
            self.relist_edge(id);
        }
    }

    fn unindex_edge_prop(&mut self, id: EdgeId, _key: &str) {
        self.unindex_edge(id);
    }

    fn index_edge_prop(&mut self, id: EdgeId, _key: &str) {
        self.index_edge(id);
    }

    fn intern_property_key(&mut self, key: &str) -> PropertyKeyId {
        if self.read_only {
            // an id nothing carries; the write that wanted it fails
            return PropertyKeyId(self.strings.get(key).unwrap_or(u32::MAX));
        }
        PropertyKeyId(self.strings.intern(key))
    }

    fn analyze_statistics(&mut self, labels: &[String]) {
        if !self.read_only {
            self.schema.push(SchemaOp::Analyze(labels.to_vec()));
        }
    }

    // -- schema (applied at commit) --

    fn create_index(&mut self, name: &str, label: &str, prop: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Index,
            name,
            true,
            SchemaOp::CreateIndex {
                name: name.into(),
                label: label.into(),
                property: prop.into(),
            },
        )
    }

    fn create_composite_index(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        is_relationship: bool,
    ) -> Result<()> {
        let kind = if is_relationship {
            SchemaKind::EdgeIndex
        } else {
            SchemaKind::Index
        };
        self.schema_change(
            kind,
            name,
            true,
            SchemaOp::CreateCompositeIndex {
                name: name.into(),
                label: label.into(),
                properties: properties.to_vec(),
                is_relationship,
            },
        )
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Index,
            name,
            false,
            SchemaOp::DropIndex(name.into()),
        )
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
        self.schema_change(
            SchemaKind::Vector,
            name,
            true,
            SchemaOp::CreateVectorIndex {
                name: name.into(),
                label: label.into(),
                property: property.into(),
                dim,
                sim,
                params,
            },
        )
    }

    fn drop_vector_index(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Vector,
            name,
            false,
            SchemaOp::DropVectorIndex(name.into()),
        )
    }

    fn create_fulltext_index(
        &mut self,
        name: &str,
        labels: &[&str],
        properties: &[&str],
        analyzer: crate::graph::fulltext::Analyzer,
    ) -> Result<()> {
        self.schema_change(
            SchemaKind::Fulltext,
            name,
            true,
            SchemaOp::CreateFulltextIndex {
                name: name.into(),
                labels: labels.iter().map(|s| s.to_string()).collect(),
                properties: properties.iter().map(|s| s.to_string()).collect(),
                analyzer,
            },
        )
    }

    fn drop_fulltext_index(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Fulltext,
            name,
            false,
            SchemaOp::DropFulltextIndex(name.into()),
        )
    }

    fn create_point_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Point,
            name,
            true,
            SchemaOp::CreatePointIndex {
                name: name.into(),
                label: label.into(),
                property: property.into(),
            },
        )
    }

    fn drop_point_index(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Point,
            name,
            false,
            SchemaOp::DropPointIndex(name.into()),
        )
    }

    fn create_text_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Text,
            name,
            true,
            SchemaOp::CreateTextIndex {
                name: name.into(),
                label: label.into(),
                property: property.into(),
            },
        )
    }

    fn drop_text_index(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Text,
            name,
            false,
            SchemaOp::DropTextIndex(name.into()),
        )
    }

    fn create_constraint(
        &mut self,
        name: &str,
        label: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        self.schema_change(
            SchemaKind::Constraint,
            name,
            true,
            SchemaOp::CreateConstraint {
                name: name.into(),
                label: label.into(),
                property: prop.into(),
                ctype,
            },
        )
    }

    fn create_relationship_constraint(
        &mut self,
        name: &str,
        edge_type: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        self.schema_change(
            SchemaKind::Constraint,
            name,
            true,
            SchemaOp::CreateRelationshipConstraint {
                name: name.into(),
                edge_type: edge_type.into(),
                property: prop.into(),
                ctype,
            },
        )
    }

    fn create_composite_constraint(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        ctype: ConstraintType,
        is_relationship: bool,
    ) -> Result<()> {
        self.schema_change(
            SchemaKind::Constraint,
            name,
            true,
            SchemaOp::CreateCompositeConstraint {
                name: name.into(),
                label: label.into(),
                properties: properties.to_vec(),
                ctype,
                is_relationship,
            },
        )
    }

    fn drop_constraint(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::Constraint,
            name,
            false,
            SchemaOp::DropConstraint(name.into()),
        )
    }

    fn create_edge_index(&mut self, name: &str, edge_type: &str, prop: Option<&str>) -> Result<()> {
        self.schema_change(
            SchemaKind::EdgeIndex,
            name,
            true,
            SchemaOp::CreateEdgeIndex {
                name: name.into(),
                edge_type: edge_type.into(),
                property: prop.map(str::to_string),
            },
        )
    }

    fn drop_edge_index(&mut self, name: &str) -> Result<()> {
        self.schema_change(
            SchemaKind::EdgeIndex,
            name,
            false,
            SchemaOp::DropEdgeIndex(name.into()),
        )
    }
}

impl OptimisticTx<'_> {
    /// Live local nodes (created or changed, not deleted).
    fn live_local_nodes(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.local
            .nodes
            .iter()
            .filter_map(|(id, n)| n.as_ref().map(|n| (*id, n)))
    }

    /// The point a node holds under `key` in `srid`.
    fn point_of(n: &Node, key: PropertyKeyId, srid: i64) -> Option<[f64; 3]> {
        let (coords, s) = crate::graph::spatial::point_coords(&n.properties.get(key)?)?;
        (s == srid).then_some(coords)
    }

    /// Top-`k` of the snapshot's hits we didn't touch plus every live local node `score` rates.
    fn merge_top_k(
        &self,
        base: Vec<(NodeId, f64)>,
        k: usize,
        score: &dyn Fn(&Node) -> Option<f64>,
        best_first: bool,
    ) -> Vec<(NodeId, f64)> {
        let mut hits: Vec<(NodeId, f64)> = base
            .into_iter()
            .filter(|h| !self.local.nodes.contains_key(&h.0))
            .collect();
        hits.extend(
            self.live_local_nodes()
                .filter_map(|(id, n)| score(n).map(|s| (id, s))),
        );
        hits.sort_by(|a, b| {
            let ord = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
            let ord = if best_first { ord.reverse() } else { ord };
            ord.then_with(|| a.0.0.cmp(&b.0.0))
        });
        hits.truncate(k);
        hits
    }

    /// Refuses a create once the store's memory budget is spent (approximate: local objects are
    /// charged at commit by the writer as well).
    fn check_budget(&self, props: usize) -> Result<()> {
        use crate::graph::{NODE_BASE_COST, PROPERTY_BYTE};
        let Some(limit) = self.store.memory_budget() else {
            return Ok(());
        };
        let local = (self.local.nodes.len() + self.local.edges.len()) * NODE_BASE_COST;
        let current = self.store.estimated_memory() + local;
        if current + NODE_BASE_COST + props * 16 * PROPERTY_BYTE > limit {
            return Err(Error::MemoryLimitExceeded { current, limit });
        }
        Ok(())
    }
}
