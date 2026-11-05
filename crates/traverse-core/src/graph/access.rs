//! Read access: traversal, counts, scans, label lookups and adjacency snapshots.

use std::borrow::Cow;
use std::sync::Arc;

use rustc_hash::FxHashSet;

use crate::error::{Error, Result};
use crate::interner::StringTable;

use super::adjacency::AdjEntry;
use super::batch::{AdjBatch, ColumnBound, ColumnCmp, ColumnSink};
use super::columns::{ColumnBuilder, Entity, KeyPlan, KindTally};

/// Gathers below this many lanes decode rows: a point read wants one decode for all its keys.
const COLUMN_MIN_LANES: usize = 64;
/// Consecutive ids from this many on stream the column as one slice.
const RUN_MIN_LANES: usize = 8;
use super::edge::Edge;
use super::node::Node;
use super::types::PropertyKeyId;
use super::types::{Direction, EdgeEndpoint, EdgeId, EdgeTypeId, LabelId, NodeId};
use super::{Graph, IdCounters, MetaSnapshot, adjacency};

/// An edge-type filter resolved for the inner loop: one type compares, several hash.
enum TypeWanted<'a> {
    Any,
    One(EdgeTypeId),
    Set(&'a FxHashSet<EdgeTypeId>),
}

impl<'a> TypeWanted<'a> {
    fn of(filter: &'a Option<FxHashSet<EdgeTypeId>>) -> Self {
        match filter {
            None => TypeWanted::Any,
            Some(f) if f.len() == 1 => TypeWanted::One(*f.iter().next().unwrap()),
            Some(f) => TypeWanted::Set(f),
        }
    }

    #[inline]
    fn has(&self, t: EdgeTypeId) -> bool {
        match self {
            TypeWanted::Any => true,
            TypeWanted::One(w) => *w == t,
            TypeWanted::Set(f) => f.contains(&t),
        }
    }
}

impl Graph {
    /// Neighbor IDs of `node` in `direction`, optionally filtered by edge type.
    pub fn neighbors(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<NodeId>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        // type filter given but never interned: nothing matches
        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.neighbor());
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.neighbor());
                }
            }
        }

        Ok(result)
    }

    /// Edges of `node` in `direction`, optionally filtered by edge type.
    pub fn edges_of(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<Cow<'_, Edge>>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        // type filter given but never interned: nothing matches
        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f)
                    && let Ok(edge) = self.get_edge(ep.edge_id())
                {
                    result.push(edge);
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f)
                    && let Ok(edge) = self.get_edge(ep.edge_id())
                {
                    result.push(edge);
                }
            }
        }

        Ok(result)
    }

    /// Outgoing adjacency entries: packed array slot for unmodified nodes, per-node overlay for
    /// mutated ones.
    #[inline]
    pub(crate) fn outgoing_entries(&self, node: NodeId) -> &[AdjEntry] {
        self.adj_out.neighbors(node)
    }

    /// Incoming adjacency entries: packed array slot for unmodified nodes, per-node overlay for
    /// mutated ones.
    #[inline]
    pub(crate) fn incoming_entries(&self, node: NodeId) -> &[AdjEntry] {
        self.adj_in.neighbors(node)
    }

    /// Neighbor IDs with a pre-resolved type filter.
    pub fn neighbors_filtered(
        &self,
        node: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }
        let mut result = Vec::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push(ep.neighbor());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push(ep.neighbor());
                }
            }
        }
        Ok(result)
    }

    /// `(EdgeId, NodeId)` pairs with a pre-resolved type filter.
    pub fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }
        let mut result = Vec::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push((ep.edge_id(), ep.neighbor()));
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push((ep.edge_id(), ep.neighbor()));
                }
            }
        }
        Ok(result)
    }

    /// Edges from `from` to `to` in `direction`, filtered by type.
    pub fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        if !self.nodes.contains_key(&from) {
            return Err(Error::NodeNotFound(from));
        }
        let wanted = TypeWanted::of(type_filter);
        let mut result = Vec::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(from) {
                if ep.neighbor() == to && wanted.has(ep.edge_type()) {
                    result.push(ep.edge_id());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(from) {
                if ep.neighbor() == to && wanted.has(ep.edge_type()) {
                    result.push(ep.edge_id());
                }
            }
        }
        Ok(result)
    }

    /// Calls `f` per neighbor with a pre-resolved type filter. No allocation.
    pub fn for_each_neighbor(
        &self,
        node: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|tf| tf.contains(&ep.edge_type()))
                {
                    f(ep.neighbor());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|tf| tf.contains(&ep.edge_type()))
                {
                    f(ep.neighbor());
                }
            }
        }
        Ok(())
    }

    /// Inserts every neighbor of the frontier into `out`. No dyn dispatch.
    pub fn expand_frontier(
        &self,
        nodes: &[NodeId],
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut FxHashSet<NodeId>,
    ) -> Result<()> {
        for &node in nodes {
            if !self.nodes.contains_key(&node) {
                continue;
            }
            if matches!(direction, Direction::Outgoing | Direction::Both) {
                for ep in self.outgoing_entries(node) {
                    if type_filter
                        .as_ref()
                        .is_none_or(|tf| tf.contains(&ep.edge_type()))
                    {
                        out.insert(ep.neighbor());
                    }
                }
            }
            if matches!(direction, Direction::Incoming | Direction::Both) {
                for ep in self.incoming_entries(node) {
                    if type_filter
                        .as_ref()
                        .is_none_or(|tf| tf.contains(&ep.edge_type()))
                    {
                        out.insert(ep.neighbor());
                    }
                }
            }
        }
        Ok(())
    }

    /// Number of edges of `node` in `direction`.
    pub fn degree(&self, node: NodeId, direction: Direction) -> Result<usize> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        let out = match direction {
            Direction::Outgoing | Direction::Both => self.adj_out.degree(node),
            Direction::Incoming => 0,
        };

        let inc = match direction {
            Direction::Incoming | Direction::Both => self.adj_in.degree(node),
            Direction::Outgoing => 0,
        };

        Ok(out + inc)
    }

    // -- Queries --------------------------------------------------------

    /// Returns the number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len() + self.propless.len()
    }

    /// One past the highest `NodeId.0` ever assigned. Sizes dense per-node arrays; adjacency is
    /// indexable by `0..highest_node_id()`.
    pub fn highest_node_id(&self) -> u64 {
        self.ids.next_node()
    }

    /// The id counters, shared with the store that owns this graph.
    pub(crate) fn id_counters(&self) -> Arc<IdCounters> {
        Arc::clone(&self.ids)
    }

    /// Returns an iterator over all nodes.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// All edges: borrowed if they have properties, synthesized if property-less.
    pub fn all_edges(&self) -> impl Iterator<Item = Cow<'_, Edge>> + '_ {
        self.edges.values().map(Cow::Borrowed).chain(
            self.propless
                .iter()
                .map(|(id, pt)| Cow::Owned(Edge::new(id, pt.edge_type, pt.source, pt.target))),
        )
    }

    /// Returns all node IDs as a `Vec`.
    pub fn nodes_vec(&self) -> Vec<NodeId> {
        self.nodes.keys().collect()
    }

    /// Returns all edge IDs as a `Vec`.
    pub fn all_edges_vec(&self) -> Vec<EdgeId> {
        self.edges.keys().chain(self.propless.keys()).collect()
    }

    /// Returns edge IDs connected to `node` in the given direction.
    pub fn edges_of_ids(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<EdgeId>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.edge_id());
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.edge_id());
                }
            }
        }

        Ok(result)
    }

    /// Property columns of a batch of nodes: see `GraphView::gather_node_props`. Keys with a
    /// typed column stream it; the rest decode each node's row.
    pub fn gather_node_props(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> usize {
        if ids.len() < COLUMN_MIN_LANES {
            // a point read decodes its row once for every key
            return self.gather_node_rows(ids, keys, sinks);
        }
        let due = self.columns.note_demand(Entity::Node, keys, ids.len());
        if !due.is_empty() {
            self.build_node_columns(&due);
        }
        let g = self.columns.gather(Entity::Node);
        if !g.active() {
            drop(g);
            return self.gather_node_rows(ids, keys, sinks);
        }
        // missing entities: a run of consecutive ids is a popcount of the presence bits
        let mut missing = 0;
        let mut i = 0;
        while i < ids.len() {
            let mut j = i + 1;
            while j < ids.len() && ids[j].0 == ids[j - 1].0 + 1 {
                j += 1;
            }
            match g.present_count(ids[i].0 as usize, j - i) {
                Some(live) => missing += (j - i) - live,
                None => {
                    for id in &ids[i..j] {
                        let live = match g.present(id.0 as usize) {
                            Some(p) => p,
                            None => self.nodes.contains_key(id),
                        };
                        if !live {
                            missing += 1;
                        }
                    }
                }
            }
            i = j;
        }
        for (j, &k) in keys.iter().enumerate() {
            let sink: &mut dyn ColumnSink = &mut *sinks[j];
            match g.plan(k) {
                KeyPlan::Column(col) => {
                    let row = |i: usize, s: &mut dyn ColumnSink| match self
                        .nodes
                        .get(&NodeId(i as u64))
                    {
                        Some(n) => n.properties.get_into(k, s),
                        None => s.push_null(),
                    };
                    let mut i = 0;
                    while i < ids.len() {
                        // consecutive ids stream the typed array
                        let mut j = i + 1;
                        while j < ids.len() && ids[j].0 == ids[j - 1].0 + 1 {
                            j += 1;
                        }
                        if j - i >= RUN_MIN_LANES {
                            g.push_run(col, ids[i].0 as usize, j - i, sink, row);
                        } else {
                            for id in &ids[i..j] {
                                let at = id.0 as usize;
                                g.push_lane(col, at, sink, |s| row(at, s));
                            }
                        }
                        i = j;
                    }
                }
                KeyPlan::Rows => {
                    for id in ids {
                        match self.nodes.get(id) {
                            Some(n) => n.properties.get_into(k, sink),
                            None => sink.push_null(),
                        }
                    }
                }
            }
        }
        missing
    }

    /// The row-store gather: every node decoded once for all keys.
    fn gather_node_rows(
        &self,
        ids: &[NodeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> usize {
        let mut missing = 0;
        for id in ids {
            match self.nodes.get(id) {
                Some(node) => node.properties.get_multi_into(keys, sinks),
                None => {
                    missing += 1;
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
            }
        }
        missing
    }

    /// Builds the node columns of `keys` from one scan of the table (a tally pass picks each
    /// column's type, a fill pass loads it).
    fn build_node_columns(&self, keys: &[PropertyKeyId]) {
        let prefix = self.nodes.prefix_len();
        let mut tallies = vec![KindTally::default(); keys.len()];
        for (id, node) in self.nodes.iter() {
            if id.0 as usize >= prefix {
                break;
            }
            for (j, k) in keys.iter().enumerate() {
                tallies[j].add(node.properties.get_scalar(*k));
            }
        }
        let mut builders: Vec<Option<ColumnBuilder>> = tallies
            .iter()
            .map(|t| ColumnBuilder::new(t, prefix))
            .collect();
        let mut present = if self.columns.gather(Entity::Node).active() {
            None
        } else {
            Some(vec![0u64; prefix.div_ceil(64)])
        };
        for (id, node) in self.nodes.iter() {
            let i = id.0 as usize;
            if i >= prefix {
                break;
            }
            if let Some(p) = &mut present {
                p[i / 64] |= 1u64 << (i % 64);
            }
            for (j, k) in keys.iter().enumerate() {
                if let Some(b) = &mut builders[j] {
                    b.set(i, node.properties.get_scalar(*k));
                }
            }
        }
        self.columns.install(
            Entity::Node,
            keys.iter().copied().zip(builders).collect(),
            present,
        );
    }

    /// The nodes with `label` whose `prop` satisfies `cmp` against `bound`, counted over the
    /// typed column (built if the label is big enough to warrant it); `None` when the column
    /// can't answer exactly.
    pub fn count_nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: ColumnCmp,
        bound: &ColumnBound,
    ) -> Option<usize> {
        let lid = LabelId(self.strings.get(label)?);
        let kid = PropertyKeyId(self.strings.get(prop)?);
        let set = self.label_index.get(&lid)?;
        let words = set.as_bitmap()?;
        let due = self.columns.note_demand(Entity::Node, &[kid], set.len());
        if !due.is_empty() {
            self.build_node_columns(&due);
        }
        let g = self.columns.gather(Entity::Node);
        match g.plan(kid) {
            KeyPlan::Column(col) => g.count_where(col, words, cmp, bound),
            KeyPlan::Rows => None,
        }
    }

    /// The nodes with `label` whose `prop` satisfies `cmp` against `bound`, in id order, from
    /// the typed column; `None` when the column can't answer exactly.
    pub fn nodes_where(
        &self,
        label: &str,
        prop: &str,
        cmp: ColumnCmp,
        bound: &ColumnBound,
    ) -> Option<Vec<NodeId>> {
        let lid = LabelId(self.strings.get(label)?);
        let kid = PropertyKeyId(self.strings.get(prop)?);
        let set = self.label_index.get(&lid)?;
        let words = set.as_bitmap()?;
        let due = self.columns.note_demand(Entity::Node, &[kid], set.len());
        if !due.is_empty() {
            self.build_node_columns(&due);
        }
        let g = self.columns.gather(Entity::Node);
        match g.plan(kid) {
            KeyPlan::Column(col) => g.ids_where(col, words, cmp, bound),
            KeyPlan::Rows => None,
        }
    }

    /// One past the highest edge id the edge columns cover.
    pub(super) fn edge_prefix(&self) -> usize {
        self.edges.prefix_len().max(self.propless.prefix_len())
    }

    /// Property columns of a batch of edges: see `GraphView::gather_edge_props`. Property-less
    /// edges yield nulls without a lookup in the full table.
    pub fn gather_edge_props(
        &self,
        ids: &[EdgeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> usize {
        if ids.len() < COLUMN_MIN_LANES {
            return self.gather_edge_rows(ids, keys, sinks);
        }
        let due = self.columns.note_demand(Entity::Edge, keys, ids.len());
        if !due.is_empty() {
            self.build_edge_columns(&due);
        }
        let g = self.columns.gather(Entity::Edge);
        if !g.active() {
            drop(g);
            return self.gather_edge_rows(ids, keys, sinks);
        }
        let mut missing = 0;
        for id in ids {
            let live = match g.present(id.0 as usize) {
                Some(p) => p,
                None => self.edges.contains_key(id) || self.propless.contains_key(id),
            };
            if !live {
                missing += 1;
            }
        }
        for (j, &k) in keys.iter().enumerate() {
            let sink: &mut dyn ColumnSink = &mut *sinks[j];
            match g.plan(k) {
                KeyPlan::Column(col) => {
                    let row = |i: usize, s: &mut dyn ColumnSink| match self
                        .edges
                        .get(&EdgeId(i as u64))
                    {
                        Some(e) => e.properties.get_into(k, s),
                        None => s.push_null(),
                    };
                    let mut i = 0;
                    while i < ids.len() {
                        let mut j = i + 1;
                        while j < ids.len() && ids[j].0 == ids[j - 1].0 + 1 {
                            j += 1;
                        }
                        if j - i >= RUN_MIN_LANES {
                            g.push_run(col, ids[i].0 as usize, j - i, sink, row);
                        } else {
                            for id in &ids[i..j] {
                                let at = id.0 as usize;
                                g.push_lane(col, at, sink, |s| row(at, s));
                            }
                        }
                        i = j;
                    }
                }
                KeyPlan::Rows => {
                    for id in ids {
                        match self.edges.get(id) {
                            Some(e) => e.properties.get_into(k, sink),
                            None => sink.push_null(),
                        }
                    }
                }
            }
        }
        missing
    }

    /// The row-store gather for edges.
    fn gather_edge_rows(
        &self,
        ids: &[EdgeId],
        keys: &[PropertyKeyId],
        sinks: &mut [&mut dyn ColumnSink],
    ) -> usize {
        let mut missing = 0;
        for id in ids {
            match self.edges.get(id) {
                Some(edge) => edge.properties.get_multi_into(keys, sinks),
                None => {
                    if !self.propless.contains_key(id) {
                        missing += 1;
                    }
                    for s in sinks.iter_mut() {
                        s.push_null();
                    }
                }
            }
        }
        missing
    }

    /// Edge counterpart of [`Self::build_node_columns`]; property-less edges only count as
    /// present.
    fn build_edge_columns(&self, keys: &[PropertyKeyId]) {
        let prefix = self.edge_prefix();
        let mut tallies = vec![KindTally::default(); keys.len()];
        for (id, edge) in self.edges.iter() {
            if id.0 as usize >= prefix {
                break;
            }
            for (j, k) in keys.iter().enumerate() {
                tallies[j].add(edge.properties.get_scalar(*k));
            }
        }
        let mut builders: Vec<Option<ColumnBuilder>> = tallies
            .iter()
            .map(|t| ColumnBuilder::new(t, prefix))
            .collect();
        let mut present = if self.columns.gather(Entity::Edge).active() {
            None
        } else {
            let mut p = vec![0u64; prefix.div_ceil(64)];
            for id in self.propless.keys() {
                let i = id.0 as usize;
                if i < prefix {
                    p[i / 64] |= 1u64 << (i % 64);
                }
            }
            Some(p)
        };
        for (id, edge) in self.edges.iter() {
            let i = id.0 as usize;
            if i >= prefix {
                break;
            }
            if let Some(p) = &mut present {
                p[i / 64] |= 1u64 << (i % 64);
            }
            for (j, k) in keys.iter().enumerate() {
                if let Some(b) = &mut builders[j] {
                    b.set(i, edge.properties.get_scalar(*k));
                }
            }
        }
        self.columns.install(
            Entity::Edge,
            keys.iter().copied().zip(builders).collect(),
            present,
        );
    }

    /// Adjacency of a batch of sources: see `GraphView::expand_batch`.
    pub fn expand_batch(
        &self,
        srcs: &[NodeId],
        dir: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut AdjBatch,
    ) -> Result<()> {
        let wanted = TypeWanted::of(type_filter);
        for (lane, &src) in srcs.iter().enumerate() {
            if !self.nodes.contains_key(&src) {
                return Err(Error::NodeNotFound(src));
            }
            let lane = lane as u32;
            if matches!(dir, Direction::Outgoing | Direction::Both) {
                for ep in self.outgoing_entries(src) {
                    if wanted.has(ep.edge_type()) {
                        out.push(lane, ep.edge_id(), ep.edge_type(), ep.neighbor(), true);
                    }
                }
            }
            if matches!(dir, Direction::Incoming | Direction::Both) {
                for ep in self.incoming_entries(src) {
                    // an undirected self-loop is already listed among the outgoing entries
                    if dir == Direction::Both && ep.neighbor() == src {
                        continue;
                    }
                    if wanted.has(ep.edge_type()) {
                        out.push(lane, ep.edge_id(), ep.edge_type(), ep.neighbor(), false);
                    }
                }
            }
        }
        Ok(())
    }

    /// Topology (no properties) of the edges of `node`; reads fixed-size fields only.
    pub fn edges_with_neighbor(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(EdgeEndpoint {
                        edge_id: ep.edge_id(),
                        edge_type: ep.edge_type(),
                        neighbor: ep.neighbor(),
                        source: node,
                        target: ep.neighbor(),
                    });
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(EdgeEndpoint {
                        edge_id: ep.edge_id(),
                        edge_type: ep.edge_type(),
                        neighbor: ep.neighbor(),
                        source: ep.neighbor(),
                        target: node,
                    });
                }
            }
        }

        Ok(result)
    }

    /// `true` if the node has `label`. O(1).
    pub fn has_label(&self, node: NodeId, label: LabelId) -> bool {
        self.label_index
            .get(&label)
            .is_some_and(|s| s.contains(&node))
    }

    /// Edges whose two ends are the same node.
    pub fn self_loop_count(&self) -> usize {
        self.loops
    }

    /// The label's members as a bitmap over node ids, when every member is in the id prefix.
    pub fn label_bitmap(&self, label: &str) -> Option<Vec<u64>> {
        let lid = LabelId(self.strings.get(label)?);
        self.label_index
            .get(&lid)
            .and_then(|set| set.as_bitmap())
            .map(<[u64]>::to_vec)
    }

    /// Returns node IDs that have the given label.
    pub fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        let lid = match self.strings.get(label) {
            Some(raw) => LabelId(raw),
            None => return Vec::new(),
        };
        self.label_index
            .get(&lid)
            .map(|set| set.to_vec())
            .unwrap_or_default()
    }

    /// Adjacency borrowed for an algorithm run (the borrow keeps the graph unchanged).
    pub fn adjacency_snapshot(&self) -> adjacency::AdjacencySnapshot<'_> {
        adjacency::AdjacencySnapshot::new(
            std::borrow::Cow::Borrowed(&self.adj_out),
            std::borrow::Cow::Borrowed(&self.adj_in),
            self.highest_node_id(),
        )
    }

    /// Adjacency copied for a reader that outlives any borrow: shares the packed arrays, copies
    /// the overlay (kept small by [`Self::compact_adjacency`]).
    pub(crate) fn owned_adjacency_snapshot(&self) -> adjacency::AdjacencySnapshot<'static> {
        adjacency::AdjacencySnapshot::new(
            std::borrow::Cow::Owned(self.adj_out.clone()),
            std::borrow::Cow::Owned(self.adj_in.clone()),
            self.highest_node_id(),
        )
    }

    /// Folds every adjacency overlay into fresh packed arrays, whatever its size: after a bulk
    /// load, which wrote most lists into them. O(nodes + relationships).
    pub fn pack_adjacency(&mut self) {
        self.adj_out.compact();
        self.adj_in.compact();
    }

    /// Folds adjacency overlays into fresh packed arrays once they cover more than an eighth of the
    /// nodes (and at least 1024): keeps reads on the packed fast path and snapshot copies small.
    /// Amortized O(1) per mutation.
    pub fn compact_adjacency(&mut self) {
        let threshold = (self.node_count() / 8).max(1024);
        if self.adj_out.overlay_len() > threshold {
            self.adj_out.compact();
        }
        if self.adj_in.overlay_len() > threshold {
            self.adj_in.compact();
        }
    }

    /// Returns a reference to the string table.
    pub fn strings(&self) -> &StringTable {
        &self.strings
    }

    /// Copy-on-write metadata a snapshot reader captures at its start (see [`MetaSnapshot`]).
    pub(crate) fn meta_snapshot(&self) -> MetaSnapshot {
        MetaSnapshot {
            strings: self.strings.snapshot(),
            index_catalog: Arc::clone(self.indexes.catalog()),
            edge_index_catalog: Arc::clone(self.edge_indexes.catalog()),
            constraints: Arc::clone(&self.constraints),
            stats: self.stats.clone(),
        }
    }
}
