//! Snapshot reads: [`ReadTx`] and the metadata-only [`UnregisteredReadView`].
//!
//! A reader takes the structural read lock per call and resolves every entity
//! through three things: the run that created it (hidden if created after the
//! snapshot), its delta chain (the oldest image newer than the snapshot), and
//! ghosts (memberships the live labels / types / indexes / adjacency dropped after
//! the snapshot). With no version state at all it reads the live graph directly.

use std::borrow::Cow;
use std::ops::{Bound, RangeBounds};
use std::sync::atomic::Ordering;

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use super::MvccStore;
use super::mvcc::{CommitInfo, DeltaAction, TXN_ID_SENTINEL, image_at};
use super::timestamp::HybridTimestamp;
use super::view::GraphView;
use crate::error::{Error, Result};
use crate::graph::batch::{AdjBatch, ColumnSink};
use crate::graph::constraint::ConstraintRegistry;
use crate::graph::edge::Edge;
use crate::graph::index::{EdgeIndexCatalog, IndexCatalog};
use crate::graph::node::Node;
use crate::graph::property::{IndexKey, InternedPropertyMap, PropertyValue};
use crate::graph::stats::GraphStatistics;
use crate::graph::types::{
    Direction, EdgeEndpoint, EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId,
};
use crate::graph::{Graph, MetaSnapshot};
use crate::interner::StringTable;

// ===========================================================================
// ReadTx
// ===========================================================================

/// Read-only transaction on a consistent snapshot.
///
/// Sees the graph as of `snapshot_ts`: every transaction committed by then, none
/// after, never an uncommitted one. Registers in the reader registry so GC keeps
/// what it needs; writes return `Err(Error::ReadOnlyTransaction)`.
pub struct ReadTx<'a> {
    store: &'a MvccStore,
    snapshot_ts: HybridTimestamp,
    /// Encoded `snapshot_ts`, for delta comparison.
    snap: u64,
    /// Slot in the reader registry.
    slot: usize,
    /// Strings, catalogs, constraints and statistics as of the start: methods returning
    /// references hand these out, never references into the live graph.
    meta: MetaSnapshot,
}

/// A node as a snapshot sees it.
enum NodeAt<'g> {
    Live(&'g Node),
    Image {
        id: NodeId,
        labels: &'g SmallVec<[LabelId; 2]>,
        properties: &'g InternedPropertyMap,
    },
}

impl NodeAt<'_> {
    fn labels(&self) -> &[LabelId] {
        match self {
            NodeAt::Live(n) => &n.labels,
            NodeAt::Image { labels, .. } => labels,
        }
    }

    fn properties(&self) -> &InternedPropertyMap {
        match self {
            NodeAt::Live(n) => &n.properties,
            NodeAt::Image { properties, .. } => properties,
        }
    }

    fn to_node(&self) -> Node {
        match self {
            NodeAt::Live(n) => (*n).clone(),
            NodeAt::Image {
                id,
                labels,
                properties,
            } => Node {
                id: *id,
                labels: (*labels).clone(),
                properties: (*properties).clone(),
            },
        }
    }
}

/// An edge as a snapshot sees it.
enum EdgeAt<'g> {
    Live(Cow<'g, Edge>),
    Image {
        id: EdgeId,
        source: NodeId,
        target: NodeId,
        edge_type: EdgeTypeId,
        properties: &'g InternedPropertyMap,
    },
}

impl EdgeAt<'_> {
    fn source(&self) -> NodeId {
        match self {
            EdgeAt::Live(e) => e.source,
            EdgeAt::Image { source, .. } => *source,
        }
    }

    fn target(&self) -> NodeId {
        match self {
            EdgeAt::Live(e) => e.target,
            EdgeAt::Image { target, .. } => *target,
        }
    }

    fn edge_type(&self) -> EdgeTypeId {
        match self {
            EdgeAt::Live(e) => e.edge_type,
            EdgeAt::Image { edge_type, .. } => *edge_type,
        }
    }

    fn properties(&self) -> &InternedPropertyMap {
        match self {
            EdgeAt::Live(e) => &e.properties,
            EdgeAt::Image { properties, .. } => properties,
        }
    }

    fn into_edge(self) -> Edge {
        match self {
            EdgeAt::Live(e) => e.into_owned(),
            EdgeAt::Image {
                id,
                source,
                target,
                edge_type,
                properties,
            } => {
                let mut e = Edge::new(id, edge_type, source, target);
                e.properties = properties.clone();
                e
            }
        }
    }
}

/// Changed by a recent transaction (chain or run): its live state may not be the snapshot's.
#[inline]
fn node_touched(g: &Graph, id: NodeId) -> bool {
    !g.node_delta_head(id).is_null() || g.versions().node_runs.creator(id.0).is_some()
}

#[inline]
fn edge_touched(g: &Graph, id: EdgeId) -> bool {
    !g.edge_delta_head(id).is_null() || g.versions().edge_runs.creator(id.0).is_some()
}

impl<'a> ReadTx<'a> {
    /// Snapshot timestamp: commits after it are invisible here.
    pub fn snapshot_ts(&self) -> HybridTimestamp {
        self.snapshot_ts
    }

    pub(super) fn new(
        store: &'a MvccStore,
        snapshot_ts: HybridTimestamp,
        snap: u64,
        slot: usize,
    ) -> Self {
        let meta = {
            let _guard = store.graph_rw.read();
            // SAFETY: the read guard excludes structural writes while we clone the Arcs
            unsafe { store.graph() }.meta_snapshot()
        };
        Self {
            store,
            snapshot_ts,
            snap,
            slot,
            meta,
        }
    }

    /// Encoded snapshot timestamp.
    #[inline]
    pub(crate) fn snap(&self) -> u64 {
        self.snap
    }

    /// Metadata as of the snapshot.
    pub(crate) fn meta(&self) -> &MetaSnapshot {
        &self.meta
    }

    /// Appends the nodes among `ids` the snapshot sees, in order, under one structural read lock:
    /// streaming the snapshot out in batches.
    pub(crate) fn nodes_at(&self, ids: &[NodeId], out: &mut Vec<Node>) {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            out.extend(ids.iter().filter_map(|&id| g.get_node(id).ok().cloned()));
        } else {
            out.extend(
                ids.iter()
                    .filter_map(|&id| self.node_at(g, id).map(|n| n.to_node())),
            );
        }
    }

    /// [`nodes_at`](Self::nodes_at) for relationships.
    pub(crate) fn edges_at(&self, ids: &[EdgeId], out: &mut Vec<Edge>) {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            out.extend(
                ids.iter()
                    .filter_map(|&id| g.get_edge(id).ok().map(Cow::into_owned)),
            );
        } else {
            out.extend(
                ids.iter()
                    .filter_map(|&id| self.edge_at(g, id).map(EdgeAt::into_edge)),
            );
        }
    }

    /// The live graph; callers hold the structural read lock while they use it.
    #[inline]
    fn graph(&self) -> &Graph {
        // SAFETY: reads only, under the structural read lock (see the module docs)
        unsafe { self.store.graph() }
    }

    /// A transaction this snapshot can't see: uncommitted, or committed after it.
    #[inline]
    fn hidden(&self, ci: *const CommitInfo) -> bool {
        // SAFETY: a CommitInfo outlives every run and delta pointing at it
        let ts = unsafe { (*ci).timestamp.load(Ordering::Acquire) };
        ts == TXN_ID_SENTINEL || ts > self.snap
    }

    /// The node as of the snapshot, `None` if it didn't exist then.
    fn node_at<'g>(&self, g: &'g Graph, id: NodeId) -> Option<NodeAt<'g>> {
        if let Some(ci) = g.versions().node_runs.creator(id.0)
            && self.hidden(ci)
        {
            return None;
        }
        let head = g.node_delta_head(id);
        // SAFETY: registered + structural read lock: no delta in the chain can be freed
        let image = unsafe { image_at(self.snap, head) };
        if let Some(DeltaAction::NodeImage { labels, properties }) = image {
            return Some(NodeAt::Image {
                id,
                labels,
                properties,
            });
        }
        g.get_node(id).ok().map(NodeAt::Live)
    }

    /// The edge as of the snapshot, `None` if it didn't exist then.
    fn edge_at<'g>(&self, g: &'g Graph, id: EdgeId) -> Option<EdgeAt<'g>> {
        if let Some(ci) = g.versions().edge_runs.creator(id.0)
            && self.hidden(ci)
        {
            return None;
        }
        let head = g.edge_delta_head(id);
        // SAFETY: as in `node_at`
        let image = unsafe { image_at(self.snap, head) };
        if let Some(DeltaAction::EdgeImage {
            source,
            target,
            edge_type,
            properties,
        }) = image
        {
            return Some(EdgeAt::Image {
                id,
                source: *source,
                target: *target,
                edge_type: *edge_type,
                properties,
            });
        }
        g.get_edge(id).ok().map(EdgeAt::Live)
    }

    #[inline]
    fn node_visible(&self, g: &Graph, id: NodeId) -> bool {
        if node_touched(g, id) {
            self.node_at(g, id).is_some()
        } else {
            g.get_node(id).is_ok()
        }
    }

    /// Calls `f(edge, type, neighbor, outgoing)` for each edge of `node` at the snapshot: live
    /// entries it can see plus edges deleted since. `node` is visible.
    fn for_each_edge_at(
        &self,
        g: &Graph,
        node: NodeId,
        dir: Direction,
        f: &mut dyn FnMut(EdgeId, EdgeTypeId, NodeId, bool),
    ) {
        let v = g.versions();
        if matches!(dir, Direction::Outgoing | Direction::Both) {
            for ep in g.outgoing_entries(node) {
                let e = ep.edge_id();
                if !edge_touched(g, e) || self.edge_at(g, e).is_some() {
                    f(e, ep.edge_type(), ep.neighbor(), true);
                }
            }
            for e in v.deleted_out(node) {
                if let Some(at) = self.edge_at(g, e) {
                    f(e, at.edge_type(), at.target(), true);
                }
            }
        }
        if matches!(dir, Direction::Incoming | Direction::Both) {
            for ep in g.incoming_entries(node) {
                let e = ep.edge_id();
                if !edge_touched(g, e) || self.edge_at(g, e).is_some() {
                    f(e, ep.edge_type(), ep.neighbor(), false);
                }
            }
            for e in v.deleted_in(node) {
                if let Some(at) = self.edge_at(g, e) {
                    f(e, at.edge_type(), at.source(), false);
                }
            }
        }
    }

    /// `NodeNotFound` unless `node` is visible.
    fn require_node(&self, g: &Graph, node: NodeId) -> Result<()> {
        if self.node_visible(g, node) {
            Ok(())
        } else {
            Err(Error::NodeNotFound(node))
        }
    }

    /// Edge type id of a filter name as of the snapshot; `Err(())` = unknown name (no match).
    fn type_filter(&self, filter: Option<&str>) -> std::result::Result<Option<EdgeTypeId>, ()> {
        match filter {
            None => Ok(None),
            Some(t) => self
                .meta
                .strings
                .get(t)
                .map(|id| Some(EdgeTypeId(id)))
                .ok_or(()),
        }
    }

    /// Nodes `(label, key)` index lookup would return at the snapshot. `live` = the live index's
    /// candidates (`None`: the index is gone since, scan the label); `matches` checks a value.
    fn node_index_at(
        &self,
        g: &Graph,
        label: LabelId,
        key: PropertyKeyId,
        live: Option<Vec<NodeId>>,
        ghosts: Vec<NodeId>,
        matches: &dyn Fn(&IndexKey) -> bool,
    ) -> Vec<NodeId> {
        let check = |id: NodeId| {
            self.node_at(g, id).is_some_and(|n| {
                n.labels().contains(&label)
                    && g.index_key_of(n.properties(), key)
                        .is_some_and(|k| matches(&k))
            })
        };
        let Some(live) = live else {
            return self
                .nodes_with_label_at(g, label)
                .into_iter()
                .filter(|&id| check(id))
                .collect();
        };
        let mut out: Vec<NodeId> = live
            .iter()
            .copied()
            .filter(|&id| !node_touched(g, id) || check(id))
            .collect();
        if !ghosts.is_empty() {
            // a node can be a ghost under several values of a range
            let mut seen: FxHashSet<NodeId> = live.into_iter().collect();
            out.extend(
                ghosts
                    .into_iter()
                    .filter(|&id| seen.insert(id) && check(id)),
            );
        }
        out
    }

    /// Nodes with `label` at the snapshot.
    fn nodes_with_label_at(&self, g: &Graph, label: LabelId) -> Vec<NodeId> {
        let live = g.label_members(label);
        let mut out = Vec::new();
        if let Some(set) = live {
            out.reserve(set.len());
            for id in set {
                if !node_touched(g, id)
                    || self
                        .node_at(g, id)
                        .is_some_and(|n| n.labels().contains(&label))
                {
                    out.push(id);
                }
            }
        }
        for id in g.versions().label_ghosts(label) {
            if live.is_some_and(|s| s.contains(&id)) {
                continue;
            }
            if self
                .node_at(g, id)
                .is_some_and(|n| n.labels().contains(&label))
            {
                out.push(id);
            }
        }
        out
    }

    /// Edges of `edge_type` at the snapshot.
    fn edges_with_type_at(&self, g: &Graph, edge_type: EdgeTypeId) -> Vec<EdgeId> {
        let live = g.type_members(edge_type);
        let mut out = Vec::new();
        if let Some(set) = live {
            out.reserve(set.len());
            for id in set {
                if !edge_touched(g, id) || self.edge_at(g, id).is_some() {
                    out.push(id);
                }
            }
        }
        for id in g.versions().type_ghosts(edge_type) {
            if live.is_some_and(|s| s.contains(&id)) {
                continue;
            }
            if self
                .edge_at(g, id)
                .is_some_and(|e| e.edge_type() == edge_type)
            {
                out.push(id);
            }
        }
        out
    }

    /// Top-`k` by a score from a live index whose entries may be newer than the snapshot:
    /// untouched live hits keep their score, touched hits and ghosts (`ghosts`) are rescored at the
    /// snapshot by `rescore` (`None` = not a hit then). `query(n)` asks the live index for `n`.
    /// `best_first` sorts descending (scores) or ascending (distances).
    fn top_k_at(
        &self,
        g: &Graph,
        k: usize,
        ghosts: Vec<NodeId>,
        query: &dyn Fn(usize) -> Vec<(NodeId, f64)>,
        rescore: &dyn Fn(&NodeAt<'_>) -> Option<f64>,
        best_first: bool,
    ) -> Vec<(NodeId, f64)> {
        let rescore_id = |id: NodeId| self.node_at(g, id).and_then(|n| rescore(&n));
        let mut want = k.saturating_add(ghosts.len());
        let mut hits: Vec<(NodeId, f64)>;
        loop {
            let live = query(want);
            let exhausted = live.len() < want;
            hits = Vec::with_capacity(live.len());
            for (id, score) in live {
                if !node_touched(g, id) {
                    hits.push((id, score));
                } else if let Some(s) = rescore_id(id) {
                    hits.push((id, s));
                }
            }
            if hits.len() >= k || exhausted {
                break;
            }
            want = want.saturating_mul(2);
        }
        let seen: FxHashSet<NodeId> = hits.iter().map(|h| h.0).collect();
        for id in ghosts {
            if !seen.contains(&id)
                && let Some(s) = rescore_id(id)
            {
                hits.push((id, s));
            }
        }
        hits.sort_by(|a, b| {
            let ord = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
            let ord = if best_first { ord.reverse() } else { ord };
            ord.then_with(|| a.0.0.cmp(&b.0.0))
        });
        hits.truncate(k);
        hits
    }

    /// The point `props` holds under `key` in `srid`, if any.
    fn point_in(props: &InternedPropertyMap, key: PropertyKeyId, srid: i64) -> Option<[f64; 3]> {
        let (coords, s) = crate::graph::spatial::point_coords(&props.get(key)?)?;
        (s == srid).then_some(coords)
    }
}

// SAFETY: ReadTx is `Sync`: methods only read the graph, under the structural read lock; `store`
// is `Sync`; the rest is immutable after construction.
unsafe impl Sync for ReadTx<'_> {}

impl Drop for ReadTx<'_> {
    fn drop(&mut self) {
        self.store.reader_registry.unregister(self.slot);
    }
}

impl GraphView for ReadTx<'_> {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        Some(self)
    }

    fn data_version(&self) -> Option<(usize, u64)> {
        // a snapshot never changes: store identity + snapshot timestamp
        Some((self.store as *const MvccStore as usize, self.snap))
    }

    fn adjacency_snapshot(&self) -> Option<crate::graph::adjacency::AdjacencySnapshot<'_>> {
        let _guard = self.store.graph_rw.read();
        let graph = self.graph();
        // no version state while we're registered: nothing changed since our snapshot, so the
        // current stores ARE the snapshot; copies share the packed arrays and take the overlay
        if graph.has_active_deltas() {
            return None;
        }
        Some(graph.owned_adjacency_snapshot())
    }

    fn highest_node_id(&self) -> u64 {
        let _guard = self.store.graph_rw.read();
        self.graph().highest_node_id()
    }

    fn get_node(&self, id: NodeId) -> Result<Cow<'_, Node>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.get_node(id).map(|n| Cow::Owned(n.clone()));
        }
        self.node_at(g, id)
            .map(|n| Cow::Owned(n.to_node()))
            .ok_or(Error::NodeNotFound(id))
    }

    fn get_edge(&self, id: EdgeId) -> Result<Cow<'_, Edge>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.get_edge(id).map(|e| Cow::Owned(e.into_owned()));
        }
        self.edge_at(g, id)
            .map(|e| Cow::Owned(e.into_edge()))
            .ok_or(Error::EdgeNotFound(id))
    }

    fn has_label(&self, node: NodeId, label: LabelId) -> Result<bool> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return Ok(g.has_label(node, label));
        }
        Ok(self
            .node_at(g, node)
            .is_some_and(|n| n.labels().contains(&label)))
    }

    fn get_node_property(&self, id: NodeId, key: &str) -> Result<Option<PropertyValue>> {
        let kid = self.meta.strings.get(key).map(PropertyKeyId);
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let n = if g.has_active_deltas() {
            self.node_at(g, id)
        } else {
            g.get_node(id).ok().map(NodeAt::Live)
        }
        .ok_or(Error::NodeNotFound(id))?;
        Ok(kid.and_then(|k| n.properties().get(k)))
    }

    fn get_node_property_kid(
        &self,
        id: NodeId,
        kid: PropertyKeyId,
    ) -> Result<Option<PropertyValue>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let n = if g.has_active_deltas() {
            self.node_at(g, id)
        } else {
            g.get_node(id).ok().map(NodeAt::Live)
        }
        .ok_or(Error::NodeNotFound(id))?;
        Ok(n.properties().get(kid))
    }

    fn get_edge_property(&self, id: EdgeId, key: &str) -> Result<Option<PropertyValue>> {
        let kid = self.meta.strings.get(key).map(PropertyKeyId);
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let e = if g.has_active_deltas() {
            self.edge_at(g, id)
        } else {
            g.get_edge(id).ok().map(EdgeAt::Live)
        }
        .ok_or(Error::EdgeNotFound(id))?;
        Ok(kid.and_then(|k| e.properties().get(k)))
    }

    fn batch_read_node_properties(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out: &mut [Option<PropertyValue>],
    ) -> Result<()> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            g.batch_read_node_properties(node_ids, keys, out);
            return Ok(());
        }
        let nk = keys.len();
        for (i, &nid) in node_ids.iter().enumerate() {
            let row = &mut out[i * nk..(i + 1) * nk];
            match self.node_at(g, nid) {
                Some(n) => n.properties().get_multi(keys, row),
                None => row.iter_mut().for_each(|v| *v = None),
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
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            g.batch_read_node_properties_i64(node_ids, keys, out_vals, out_valid);
            return Ok(());
        }
        let nk = keys.len();
        for (i, &nid) in node_ids.iter().enumerate() {
            let (lo, hi) = (i * nk, (i + 1) * nk);
            match self.node_at(g, nid) {
                Some(n) => n.properties().get_multi_int(
                    keys,
                    &mut out_vals[lo..hi],
                    &mut out_valid[lo..hi],
                ),
                None => out_valid[lo..hi].iter_mut().for_each(|v| *v = false),
            }
        }
        Ok(())
    }

    fn gather_node_props(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return Ok(g.gather_node_props(ids, keys, sinks));
        }
        let mut missing = 0;
        for &id in ids {
            match self.node_at(g, id) {
                Some(n) => n.properties().get_multi_into(keys, sinks),
                None => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
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
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return Ok(g.gather_edge_props(ids, keys, sinks));
        }
        let mut missing = 0;
        for &id in ids {
            match self.edge_at(g, id) {
                Some(e) => e.properties().get_multi_into(keys, sinks),
                None => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
            }
        }
        Ok(missing)
    }

    fn nodes_exist(&self, ids: &[NodeId], out: &mut Vec<bool>) {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        out.clear();
        out.extend(ids.iter().map(|&id| self.node_visible(g, id)));
    }

    fn edges_exist(&self, ids: &[EdgeId], out: &mut Vec<bool>) {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        out.clear();
        if !g.has_active_deltas() {
            out.extend(ids.iter().map(|&id| g.has_edge(id)));
        } else {
            out.extend(ids.iter().map(|&id| self.edge_at(g, id).is_some()));
        }
    }

    fn nodes_have_label(&self, ids: &[NodeId], label: LabelId, out: &mut Vec<bool>) {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        out.clear();
        if !g.has_active_deltas() {
            match g.label_members(label) {
                Some(set) => out.extend(ids.iter().map(|id| set.contains(id))),
                None => out.resize(ids.len(), false),
            }
            return;
        }
        out.extend(ids.iter().map(|&id| {
            self.node_at(g, id)
                .is_some_and(|n| n.labels().contains(&label))
        }));
    }

    fn count_nodes_by_label(&self, label: &str) -> usize {
        let Some(lid) = self.meta.strings.get(label).map(LabelId) else {
            return 0;
        };
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.label_members(lid).map_or(0, |s| s.len());
        }
        self.nodes_with_label_at(g, lid).len()
    }

    fn count_edges_by_type(&self, edge_type: &str) -> usize {
        let Some(tid) = self.meta.strings.get(edge_type).map(EdgeTypeId) else {
            return 0;
        };
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.type_members(tid).map_or(0, |s| s.len());
        }
        self.edges_with_type_at(g, tid).len()
    }

    fn count_index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<usize> {
        {
            let _guard = self.store.graph_rw.read();
            let g = self.graph();
            if !g.has_active_deltas() {
                let lid = LabelId(self.meta.strings.get(label)?);
                let pk = PropertyKeyId(self.meta.strings.get(prop)?);
                if !self.meta.index_catalog.has_index(lid, pk) {
                    return None;
                }
                if let Some(n) = GraphView::count_index_lookup(g, label, prop, value) {
                    return Some(n);
                }
            }
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
        {
            let _guard = self.store.graph_rw.read();
            let g = self.graph();
            if !g.has_active_deltas() {
                let lid = LabelId(self.meta.strings.get(label)?);
                let pk = PropertyKeyId(self.meta.strings.get(prop)?);
                if !self.meta.index_catalog.has_index(lid, pk) {
                    return None;
                }
                if let Some(n) =
                    GraphView::count_index_range(g, label, prop, lower.clone(), upper.clone())
                {
                    return Some(n);
                }
            }
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
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if g.has_active_deltas() {
            return None;
        }
        let lid = LabelId(self.meta.strings.get(label)?);
        let pk = PropertyKeyId(self.meta.strings.get(prop)?);
        if !self.meta.index_catalog.has_index(lid, pk) {
            return None;
        }
        GraphView::index_range_estimate(g, label, prop, lower, upper)
    }

    fn expand_batch(
        &self,
        srcs: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut AdjBatch,
    ) -> Result<()> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.expand_batch(srcs, dir, type_filter, out);
        }
        for (lane, &src) in srcs.iter().enumerate() {
            self.require_node(g, src)?;
            let lane = lane as u32;
            self.for_each_edge_at(g, src, dir, &mut |e, t, nb, outgoing| {
                if type_filter.as_ref().is_none_or(|f| f.contains(&t)) {
                    // an undirected self-loop is already listed among the outgoing entries
                    if dir == Direction::Both && nb == src && !outgoing {
                        return;
                    }
                    out.push(lane, e, t, nb, outgoing);
                }
            });
        }
        Ok(())
    }

    fn edges_with_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.edges_with_neighbor(node, dir, filter);
        }
        self.require_node(g, node)?;
        let Ok(tf) = self.type_filter(filter) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        self.for_each_edge_at(g, node, dir, &mut |e, t, nb, outgoing| {
            if tf.is_none_or(|f| f == t) {
                out.push(EdgeEndpoint {
                    edge_id: e,
                    edge_type: t,
                    neighbor: nb,
                    source: if outgoing { node } else { nb },
                    target: if outgoing { nb } else { node },
                });
            }
        });
        Ok(out)
    }

    fn neighbors_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.neighbors_filtered(node, dir, type_filter);
        }
        self.require_node(g, node)?;
        let mut out = Vec::new();
        self.for_each_edge_at(g, node, dir, &mut |_, t, nb, _| {
            if type_filter.as_ref().is_none_or(|f| f.contains(&t)) {
                out.push(nb);
            }
        });
        Ok(out)
    }

    fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.edge_endpoints_filtered(node, dir, type_filter);
        }
        self.require_node(g, node)?;
        let mut out = Vec::new();
        self.for_each_edge_at(g, node, dir, &mut |e, t, nb, _| {
            if type_filter.as_ref().is_none_or(|f| f.contains(&t)) {
                out.push((e, nb));
            }
        });
        Ok(out)
    }

    fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.edges_between(from, to, dir, type_filter);
        }
        self.require_node(g, from)?;
        let mut out = Vec::new();
        self.for_each_edge_at(g, from, dir, &mut |e, t, nb, _| {
            if nb == to && type_filter.as_ref().is_none_or(|f| f.contains(&t)) {
                out.push(e);
            }
        });
        Ok(out)
    }

    fn for_each_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.for_each_neighbor(node, dir, type_filter, f);
        }
        self.require_node(g, node)?;
        self.for_each_edge_at(g, node, dir, &mut |_, t, nb, _| {
            if type_filter.as_ref().is_none_or(|tf| tf.contains(&t)) {
                f(nb);
            }
        });
        Ok(())
    }

    fn expand_frontier(
        &self,
        nodes: &[NodeId],
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut FxHashSet<NodeId>,
    ) -> Result<()> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.expand_frontier(nodes, direction, type_filter, out);
        }
        for &node in nodes {
            if !self.node_visible(g, node) {
                continue;
            }
            self.for_each_edge_at(g, node, direction, &mut |_, t, nb, _| {
                if type_filter.as_ref().is_none_or(|tf| tf.contains(&t)) {
                    out.insert(nb);
                }
            });
        }
        Ok(())
    }

    fn node_count(&self) -> usize {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.node_count();
        }
        let v = g.versions();
        // live nodes this snapshot can't see were created after it; nodes it sees that are gone
        // were deleted after it
        let hidden = v
            .node_runs
            .hidden_ids(|ci| self.hidden(ci))
            .filter(|&id| g.get_node(NodeId(id)).is_ok())
            .count();
        let deleted = v
            .deleted_nodes()
            .filter(|&id| self.node_at(g, id).is_some())
            .count();
        g.node_count() - hidden + deleted
    }

    fn edge_count(&self) -> usize {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.edge_count();
        }
        let v = g.versions();
        let hidden = v
            .edge_runs
            .hidden_ids(|ci| self.hidden(ci))
            .filter(|&id| g.has_edge(EdgeId(id)))
            .count();
        let deleted = v
            .deleted_edges()
            .filter(|&id| self.edge_at(g, id).is_some())
            .count();
        g.edge_count() - hidden + deleted
    }

    fn nodes_vec(&self) -> Vec<NodeId> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let mut out = g.nodes_vec();
        if !g.has_active_deltas() {
            return out;
        }
        out.retain(|&id| !node_touched(g, id) || self.node_at(g, id).is_some());
        out.extend(
            g.versions()
                .deleted_nodes()
                .filter(|&id| self.node_at(g, id).is_some()),
        );
        out
    }

    fn all_edges_vec(&self) -> Vec<EdgeId> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let mut out = g.all_edges_vec();
        if !g.has_active_deltas() {
            return out;
        }
        out.retain(|&id| !edge_touched(g, id) || self.edge_at(g, id).is_some());
        out.extend(
            g.versions()
                .deleted_edges()
                .filter(|&id| self.edge_at(g, id).is_some()),
        );
        out
    }

    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        let Some(lid) = self.meta.strings.get(label).map(LabelId) else {
            return Vec::new();
        };
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.label_members(lid).map(|s| s.to_vec()).unwrap_or_default();
        }
        self.nodes_with_label_at(g, lid)
    }

    fn edges_by_type(&self, edge_type: &str) -> Vec<EdgeId> {
        let Some(tid) = self.meta.strings.get(edge_type).map(EdgeTypeId) else {
            return Vec::new();
        };
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.type_members(tid).map(|s| s.to_vec()).unwrap_or_default();
        }
        self.edges_with_type_at(g, tid)
    }

    // schema token sets (db.labels / relationshipTypes / propertyKeys) come from the live graph,
    // like Neo4j's token-store introspection
    fn label_names(&self) -> Vec<String> {
        let _guard = self.store.graph_rw.read();
        self.graph().label_names()
    }

    fn relationship_type_names(&self) -> Vec<String> {
        let _guard = self.store.graph_rw.read();
        self.graph().relationship_type_names()
    }

    fn property_key_names(&self) -> Vec<String> {
        let _guard = self.store.graph_rw.read();
        self.graph().property_key_names()
    }

    fn neighbors(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<NodeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.neighbors(node, dir, filter);
        }
        self.require_node(g, node)?;
        let Ok(tf) = self.type_filter(filter) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        self.for_each_edge_at(g, node, dir, &mut |_, t, nb, _| {
            if tf.is_none_or(|f| f == t) {
                out.push(nb);
            }
        });
        Ok(out)
    }

    fn edges_of(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<EdgeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.edges_of_ids(node, dir, filter);
        }
        self.require_node(g, node)?;
        let Ok(tf) = self.type_filter(filter) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        self.for_each_edge_at(g, node, dir, &mut |e, t, _, _| {
            if tf.is_none_or(|f| f == t) {
                out.push(e);
            }
        });
        Ok(out)
    }

    fn degree(&self, node: NodeId, dir: Direction) -> Result<usize> {
        {
            let _guard = self.store.graph_rw.read();
            let g = self.graph();
            if !g.has_active_deltas() {
                return g.degree(node, dir);
            }
        }
        Ok(self.edges_of(node, dir, None)?.len())
    }

    fn degrees(&self, ids: &[NodeId], dir: Direction, out: &mut Vec<usize>) {
        {
            let _guard = self.store.graph_rw.read();
            let g = self.graph();
            if !g.has_active_deltas() {
                out.clear();
                out.extend(ids.iter().map(|&n| g.degree(n, dir).unwrap_or(0)));
                return;
            }
        }
        out.clear();
        out.extend(ids.iter().map(|&n| self.degree(n, dir).unwrap_or(0)));
    }

    fn label_bitmap(&self, label: &str) -> Option<Vec<u64>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if g.has_active_deltas() {
            return None;
        }
        g.label_bitmap(label)
    }

    fn self_loops(&self) -> Option<usize> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if g.has_active_deltas() {
            return None;
        }
        Some(g.self_loop_count())
    }

    fn count_nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: crate::graph::batch::ColumnCmp,
        bound: &crate::graph::batch::ColumnBound,
    ) -> Option<usize> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if g.has_active_deltas() {
            return None;
        }
        g.count_nodes_where(label, prop, cmp, bound)
    }

    fn nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: crate::graph::batch::ColumnCmp,
        bound: &crate::graph::batch::ColumnBound,
    ) -> Option<Vec<NodeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if g.has_active_deltas() {
            return None;
        }
        g.nodes_where(label, prop, cmp, bound)
    }

    fn strings(&self) -> &StringTable {
        &self.meta.strings
    }

    fn statistics(&self) -> Option<&GraphStatistics> {
        self.meta.stats.as_deref()
    }

    fn index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        let lid = LabelId(self.meta.strings.get(label)?);
        let pk = PropertyKeyId(self.meta.strings.get(prop)?);
        if !self.meta.index_catalog.has_index(lid, pk) {
            return None;
        }
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let live = g.index_lookup(label, prop, value);
        if !g.has_active_deltas()
            && let Some(ids) = live
        {
            return Some(ids);
        }
        let ghosts = g.versions().node_index_ghosts(lid, pk, value).collect();
        Some(self.node_index_at(g, lid, pk, live, ghosts, &|k| k == value))
    }

    // only on the live graph: with versions about, the live label counts and index entries
    // aren't this snapshot's
    fn property_seek(&self, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if g.has_active_deltas() {
            return None;
        }
        g.property_seek(prop, value)
    }

    fn property_seek_covers(&self, prop: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().property_seek_covers(prop)
    }

    fn index_range_lookup(
        &self,
        label: &str,
        prop: &str,
        lower: Bound<IndexKey>,
        upper: Bound<IndexKey>,
    ) -> Option<Vec<NodeId>> {
        let lid = LabelId(self.meta.strings.get(label)?);
        let pk = PropertyKeyId(self.meta.strings.get(prop)?);
        if !self.meta.index_catalog.has_index(lid, pk) {
            return None;
        }
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let live = g.index_range_lookup(label, prop, lower.as_ref(), upper.as_ref());
        if !g.has_active_deltas()
            && let Some(ids) = live
        {
            return Some(ids);
        }
        let ghosts = g
            .versions()
            .node_index_range_ghosts(lid, pk, lower.as_ref(), upper.as_ref())
            .collect();
        let range = (lower.as_ref(), upper.as_ref());
        Some(self.node_index_at(g, lid, pk, live, ghosts, &|k| range.contains(k)))
    }

    fn index_catalog(&self) -> &IndexCatalog {
        &self.meta.index_catalog
    }

    fn index_size(&self, name: &str) -> Option<usize> {
        let _guard = self.store.graph_rw.read();
        self.graph().index_size(name)
    }

    fn has_vector_index(&self, name: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().has_vector_index(name)
    }

    fn query_vector_index(
        &self,
        name: &str,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Option<Vec<(NodeId, f64)>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.query_vector_index(name, query, k, ef);
        }
        let (label, _) = g.vector_index_target(name)?;
        let ghosts = g.versions().label_ghosts(label).collect();
        Some(self.top_k_at(
            g,
            k,
            ghosts,
            &|n| g.query_vector_index(name, query, n, ef).unwrap_or_default(),
            &|at| {
                if at.labels().contains(&label) {
                    g.vector_score(name, query, at.properties())
                } else {
                    None
                }
            },
            true,
        ))
    }

    fn vector_index_descriptors(&self) -> Vec<(String, String, String, usize, &'static str)> {
        let _guard = self.store.graph_rw.read();
        self.graph().vector_index_descriptors()
    }

    fn has_fulltext_index(&self, name: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().has_fulltext_index(name)
    }

    fn query_fulltext_index(
        &self,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Option<Vec<(NodeId, f64)>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        if !g.has_active_deltas() {
            return g.query_fulltext_index(name, query, limit);
        }
        let (labels, _) = g.fulltext_index_target(name)?;
        let mut ghosts: Vec<NodeId> = labels
            .iter()
            .flat_map(|&l| g.versions().label_ghosts(l))
            .collect();
        ghosts.sort_unstable();
        ghosts.dedup();
        Some(self.top_k_at(
            g,
            limit,
            ghosts,
            &|n| g.query_fulltext_index(name, query, n).unwrap_or_default(),
            &|at| {
                if labels.iter().any(|l| at.labels().contains(l)) {
                    g.fulltext_score(name, query, at.properties())
                } else {
                    None
                }
            },
            true,
        ))
    }

    fn fulltext_index_descriptors(&self) -> Vec<(String, Vec<String>, Vec<String>, String)> {
        let _guard = self.store.graph_rw.read();
        self.graph().fulltext_index_descriptors()
    }

    fn has_point_index(&self, name: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().has_point_index(name)
    }

    fn has_point_index_for(&self, label: &str, property: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().has_point_index_for(label, property)
    }

    fn point_index_within_bbox(
        &self,
        label: &str,
        property: &str,
        ll: [f64; 3],
        ur: [f64; 3],
        srid: i64,
    ) -> Option<Vec<NodeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let live = g.point_index_within_bbox(label, property, ll, ur, srid)?;
        if !g.has_active_deltas() {
            return Some(live);
        }
        let lid = LabelId(self.meta.strings.get(label)?);
        let pk = PropertyKeyId(self.meta.strings.get(property)?);
        let hit = |id: NodeId| {
            self.node_at(g, id).is_some_and(|n| {
                n.labels().contains(&lid)
                    && Self::point_in(n.properties(), pk, srid)
                        .is_some_and(|p| crate::graph::spatial::in_bbox(p, ll, ur, srid))
            })
        };
        let seen: FxHashSet<NodeId> = live.iter().copied().collect();
        let mut out: Vec<NodeId> = live
            .into_iter()
            .filter(|&id| !node_touched(g, id) || hit(id))
            .collect();
        out.extend(
            g.versions()
                .label_ghosts(lid)
                .filter(|id| !seen.contains(id) && hit(*id)),
        );
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
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let live = g.point_index_within_distance(label, property, center, radius, srid)?;
        if !g.has_active_deltas() {
            return Some(live);
        }
        let lid = LabelId(self.meta.strings.get(label)?);
        let pk = PropertyKeyId(self.meta.strings.get(property)?);
        let distance = |id: NodeId| {
            let n = self.node_at(g, id)?;
            if !n.labels().contains(&lid) {
                return None;
            }
            let p = Self::point_in(n.properties(), pk, srid)?;
            let d = crate::graph::spatial::point_dist(p, center, srid);
            (d <= radius).then_some(d)
        };
        let seen: FxHashSet<NodeId> = live.iter().map(|h| h.0).collect();
        let mut out: Vec<(NodeId, f64)> = live
            .into_iter()
            .filter_map(|(id, d)| {
                if node_touched(g, id) {
                    distance(id).map(|d| (id, d))
                } else {
                    Some((id, d))
                }
            })
            .collect();
        out.extend(
            g.versions()
                .label_ghosts(lid)
                .filter(|id| !seen.contains(id))
                .filter_map(|id| distance(id).map(|d| (id, d))),
        );
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
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let first = g.point_index_knn(label, property, center, k, srid)?;
        if !g.has_active_deltas() {
            return Some(first);
        }
        let lid = LabelId(self.meta.strings.get(label)?);
        let pk = PropertyKeyId(self.meta.strings.get(property)?);
        let ghosts = g.versions().label_ghosts(lid).collect();
        Some(self.top_k_at(
            g,
            k,
            ghosts,
            &|n| {
                g.point_index_knn(label, property, center, n, srid)
                    .unwrap_or_default()
            },
            &|at| {
                if !at.labels().contains(&lid) {
                    return None;
                }
                let p = Self::point_in(at.properties(), pk, srid)?;
                Some(crate::graph::spatial::point_dist(p, center, srid))
            },
            false,
        ))
    }

    fn point_index_descriptors(&self) -> Vec<(String, String, String)> {
        let _guard = self.store.graph_rw.read();
        self.graph().point_index_descriptors()
    }

    fn has_text_index(&self, name: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().has_text_index(name)
    }

    fn has_text_index_for(&self, label: &str, property: &str) -> bool {
        let _guard = self.store.graph_rw.read();
        self.graph().has_text_index_for(label, property)
    }

    fn text_index_candidates(
        &self,
        label: &str,
        property: &str,
        kind: crate::graph::text::TextQueryKind,
        pattern: &str,
    ) -> Option<Vec<NodeId>> {
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let mut live = g.text_index_candidates(label, property, kind, pattern)?;
        if !g.has_active_deltas() {
            return Some(live);
        }
        // a superset the caller re-checks: add every node that had the label recently, drop the
        // ones this snapshot can't see
        let lid = LabelId(self.meta.strings.get(label)?);
        live.retain(|&id| !node_touched(g, id) || self.node_at(g, id).is_some());
        let seen: FxHashSet<NodeId> = live.iter().copied().collect();
        live.extend(
            g.versions()
                .label_ghosts(lid)
                .filter(|id| !seen.contains(id) && self.node_at(g, *id).is_some()),
        );
        Some(live)
    }

    fn text_index_descriptors(&self) -> Vec<(String, String, String)> {
        let _guard = self.store.graph_rw.read();
        self.graph().text_index_descriptors()
    }

    fn constraint_registry(&self) -> &ConstraintRegistry {
        &self.meta.constraints
    }

    fn edge_index_lookup(
        &self,
        edge_type: &str,
        prop: &str,
        value: &IndexKey,
    ) -> Option<Vec<EdgeId>> {
        let tid = EdgeTypeId(self.meta.strings.get(edge_type)?);
        let pk = PropertyKeyId(self.meta.strings.get(prop)?);
        if !self.meta.edge_index_catalog.has_prop_index(tid, pk) {
            return None;
        }
        let _guard = self.store.graph_rw.read();
        let g = self.graph();
        let live = g.edge_index_lookup(edge_type, prop, value);
        if !g.has_active_deltas()
            && let Some(ids) = live
        {
            return Some(ids);
        }
        let check = |id: EdgeId| {
            self.edge_at(g, id).is_some_and(|e| {
                e.edge_type() == tid
                    && g.index_key_of(e.properties(), pk)
                        .is_some_and(|k| k == *value)
            })
        };
        let Some(live) = live else {
            // the index is gone since the snapshot: scan the type
            return Some(
                self.edges_with_type_at(g, tid)
                    .into_iter()
                    .filter(|&id| check(id))
                    .collect(),
            );
        };
        let seen: FxHashSet<EdgeId> = live.iter().copied().collect();
        let mut out: Vec<EdgeId> = live
            .into_iter()
            .filter(|&id| !edge_touched(g, id) || check(id))
            .collect();
        out.extend(
            g.versions()
                .edge_index_ghosts(tid, pk, value)
                .filter(|id| !seen.contains(id) && check(*id)),
        );
        Some(out)
    }

    fn edge_index_catalog(&self) -> &EdgeIndexCatalog {
        &self.meta.edge_index_catalog
    }

    crate::reject_graph_view_writes!();
}

// ===========================================================================
// UnregisteredReadView: metadata-only view, no GC registration
// ===========================================================================

/// Read view that does NOT register with the reader registry. For metadata
/// (index hints, string table, statistics) where snapshot isolation isn't needed;
/// doesn't hold back GC of old deltas.
pub struct UnregisteredReadView<'a> {
    store: &'a MvccStore,
    meta: MetaSnapshot,
}

impl<'a> UnregisteredReadView<'a> {
    pub(super) fn new(store: &'a MvccStore) -> Self {
        let meta = {
            let _guard = store.graph_rw.read();
            // SAFETY: the read guard excludes structural writes while we clone the Arcs
            unsafe { store.graph() }.meta_snapshot()
        };
        Self { store, meta }
    }

    /// The live graph under a structural read lock, held for the guard's (temporary) lifetime:
    /// results are copied out before it drops.
    #[inline]
    fn graph(&self) -> GuardedGraph<'_> {
        GuardedGraph {
            _guard: self.store.graph_rw.read(),
            // SAFETY: the read guard excludes structural writes while the reference lives
            graph: unsafe { self.store.graph() },
        }
    }
}

/// `&Graph` paired with the structural read lock that keeps it valid.
struct GuardedGraph<'a> {
    _guard: crate::sync::SlottedReadGuard<'a>,
    graph: &'a Graph,
}

impl std::ops::Deref for GuardedGraph<'_> {
    type Target = Graph;

    fn deref(&self) -> &Graph {
        self.graph
    }
}

impl GraphView for UnregisteredReadView<'_> {
    fn highest_node_id(&self) -> u64 {
        self.graph().highest_node_id()
    }

    fn get_node(&self, id: NodeId) -> Result<Cow<'_, Node>> {
        self.graph().get_node(id).map(|n| Cow::Owned(n.clone()))
    }

    fn get_edge(&self, id: EdgeId) -> Result<Cow<'_, Edge>> {
        self.graph()
            .get_edge(id)
            .map(|cow| Cow::Owned(cow.into_owned()))
    }

    fn has_label(&self, node: NodeId, label: LabelId) -> Result<bool> {
        Ok(self.graph().has_label(node, label))
    }

    fn get_node_property(&self, id: NodeId, key: &str) -> Result<Option<PropertyValue>> {
        self.graph().get_node_property(id, key)
    }

    fn get_node_property_kid(
        &self,
        id: NodeId,
        kid: PropertyKeyId,
    ) -> Result<Option<PropertyValue>> {
        self.graph().get_node_property_kid(id, kid)
    }

    fn get_edge_property(&self, id: EdgeId, key: &str) -> Result<Option<PropertyValue>> {
        self.graph().get_edge_property(id, key)
    }

    fn batch_read_node_properties(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out: &mut [Option<PropertyValue>],
    ) -> Result<()> {
        self.graph().batch_read_node_properties(node_ids, keys, out);
        Ok(())
    }

    fn batch_read_node_properties_i64(
        &self,
        node_ids: &[NodeId],
        keys: &[PropertyKeyId],
        out_vals: &mut [i64],
        out_valid: &mut [bool],
    ) -> Result<()> {
        self.graph()
            .batch_read_node_properties_i64(node_ids, keys, out_vals, out_valid);
        Ok(())
    }

    fn gather_node_props(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        Ok(self.graph().gather_node_props(ids, keys, sinks))
    }

    fn gather_edge_props(
        &self,
        ids: &[EdgeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> Result<usize> {
        Ok(self.graph().gather_edge_props(ids, keys, sinks))
    }

    fn nodes_exist(&self, ids: &[NodeId], out: &mut Vec<bool>) {
        GraphView::nodes_exist(&*self.graph(), ids, out)
    }

    fn edges_exist(&self, ids: &[EdgeId], out: &mut Vec<bool>) {
        GraphView::edges_exist(&*self.graph(), ids, out)
    }

    fn nodes_have_label(&self, ids: &[NodeId], label: LabelId, out: &mut Vec<bool>) {
        GraphView::nodes_have_label(&*self.graph(), ids, label, out)
    }

    fn count_nodes_by_label(&self, label: &str) -> usize {
        GraphView::count_nodes_by_label(&*self.graph(), label)
    }

    fn count_edges_by_type(&self, edge_type: &str) -> usize {
        GraphView::count_edges_by_type(&*self.graph(), edge_type)
    }

    fn count_index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<usize> {
        GraphView::count_index_lookup(&*self.graph(), label, prop, value)
    }

    fn count_index_range(
        &self,
        label: &str,
        prop: &str,
        lower: Bound<IndexKey>,
        upper: Bound<IndexKey>,
    ) -> Option<usize> {
        GraphView::count_index_range(&*self.graph(), label, prop, lower, upper)
    }

    fn expand_batch(
        &self,
        srcs: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut AdjBatch,
    ) -> Result<()> {
        self.graph().expand_batch(srcs, dir, type_filter, out)
    }

    fn edges_with_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        self.graph().edges_with_neighbor(node, dir, filter)
    }

    fn neighbors_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        self.graph().neighbors_filtered(node, dir, type_filter)
    }

    fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        self.graph().edge_endpoints_filtered(node, dir, type_filter)
    }

    fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        self.graph().edges_between(from, to, dir, type_filter)
    }

    fn for_each_neighbor(
        &self,
        node: NodeId,
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        self.graph().for_each_neighbor(node, dir, type_filter, f)
    }

    fn node_count(&self) -> usize {
        self.graph().node_count()
    }
    fn edge_count(&self) -> usize {
        self.graph().edge_count()
    }
    fn nodes_vec(&self) -> Vec<NodeId> {
        self.graph().nodes_vec()
    }
    fn all_edges_vec(&self) -> Vec<EdgeId> {
        self.graph().all_edges_vec()
    }
    fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.graph().nodes_by_label(label)
    }
    fn edges_by_type(&self, edge_type: &str) -> Vec<EdgeId> {
        self.graph().edges_by_type(edge_type)
    }
    fn label_names(&self) -> Vec<String> {
        self.graph().label_names()
    }
    fn relationship_type_names(&self) -> Vec<String> {
        self.graph().relationship_type_names()
    }
    fn property_key_names(&self) -> Vec<String> {
        self.graph().property_key_names()
    }

    fn neighbors(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<NodeId>> {
        self.graph().neighbors(node, dir, filter)
    }

    fn edges_of(&self, node: NodeId, dir: Direction, filter: Option<&str>) -> Result<Vec<EdgeId>> {
        self.graph().edges_of_ids(node, dir, filter)
    }

    fn degree(&self, node: NodeId, dir: Direction) -> Result<usize> {
        self.graph().degree(node, dir)
    }

    fn strings(&self) -> &StringTable {
        &self.meta.strings
    }
    fn statistics(&self) -> Option<&GraphStatistics> {
        self.meta.stats.as_deref()
    }

    fn index_lookup(&self, label: &str, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        self.graph().index_lookup(label, prop, value)
    }
    fn property_seek(&self, prop: &str, value: &IndexKey) -> Option<Vec<NodeId>> {
        self.graph().property_seek(prop, value)
    }
    fn property_seek_covers(&self, prop: &str) -> bool {
        self.graph().property_seek_covers(prop)
    }
    fn index_range_lookup(
        &self,
        label: &str,
        prop: &str,
        lower: std::ops::Bound<IndexKey>,
        upper: std::ops::Bound<IndexKey>,
    ) -> Option<Vec<NodeId>> {
        self.graph()
            .index_range_lookup(label, prop, lower.as_ref(), upper.as_ref())
    }
    fn index_catalog(&self) -> &IndexCatalog {
        &self.meta.index_catalog
    }
    fn index_size(&self, name: &str) -> Option<usize> {
        self.graph().index_size(name)
    }
    fn has_vector_index(&self, name: &str) -> bool {
        self.graph().has_vector_index(name)
    }
    fn query_vector_index(
        &self,
        name: &str,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Option<Vec<(NodeId, f64)>> {
        self.graph().query_vector_index(name, query, k, ef)
    }
    fn vector_index_descriptors(&self) -> Vec<(String, String, String, usize, &'static str)> {
        self.graph().vector_index_descriptors()
    }
    fn has_fulltext_index(&self, name: &str) -> bool {
        self.graph().has_fulltext_index(name)
    }
    fn query_fulltext_index(
        &self,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Option<Vec<(NodeId, f64)>> {
        self.graph().query_fulltext_index(name, query, limit)
    }
    fn fulltext_index_descriptors(&self) -> Vec<(String, Vec<String>, Vec<String>, String)> {
        self.graph().fulltext_index_descriptors()
    }
    fn has_point_index(&self, name: &str) -> bool {
        self.graph().has_point_index(name)
    }
    fn has_point_index_for(&self, label: &str, property: &str) -> bool {
        self.graph().has_point_index_for(label, property)
    }
    fn point_index_within_bbox(
        &self,
        label: &str,
        property: &str,
        ll: [f64; 3],
        ur: [f64; 3],
        srid: i64,
    ) -> Option<Vec<NodeId>> {
        self.graph()
            .point_index_within_bbox(label, property, ll, ur, srid)
    }
    fn point_index_within_distance(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        radius: f64,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        self.graph()
            .point_index_within_distance(label, property, center, radius, srid)
    }
    fn point_index_knn(
        &self,
        label: &str,
        property: &str,
        center: [f64; 3],
        k: usize,
        srid: i64,
    ) -> Option<Vec<(NodeId, f64)>> {
        self.graph()
            .point_index_knn(label, property, center, k, srid)
    }
    fn point_index_descriptors(&self) -> Vec<(String, String, String)> {
        self.graph().point_index_descriptors()
    }
    fn has_text_index(&self, name: &str) -> bool {
        self.graph().has_text_index(name)
    }
    fn has_text_index_for(&self, label: &str, property: &str) -> bool {
        self.graph().has_text_index_for(label, property)
    }
    fn text_index_candidates(
        &self,
        label: &str,
        property: &str,
        kind: crate::graph::text::TextQueryKind,
        pattern: &str,
    ) -> Option<Vec<NodeId>> {
        self.graph()
            .text_index_candidates(label, property, kind, pattern)
    }
    fn text_index_descriptors(&self) -> Vec<(String, String, String)> {
        self.graph().text_index_descriptors()
    }
    fn constraint_registry(&self) -> &ConstraintRegistry {
        &self.meta.constraints
    }
    fn edge_index_lookup(
        &self,
        edge_type: &str,
        prop: &str,
        value: &IndexKey,
    ) -> Option<Vec<EdgeId>> {
        self.graph().edge_index_lookup(edge_type, prop, value)
    }
    fn edge_index_catalog(&self) -> &EdgeIndexCatalog {
        &self.meta.edge_index_catalog
    }

    crate::reject_graph_view_writes!();
}
