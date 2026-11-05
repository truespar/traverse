//! Expansion: single hop, variable length, distinct multi-hop, index lookup joins and vector
//! search. Each runs per flat source lane and fills a new group with what it reaches.

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::batch::AdjBatch;
use traverse_core::graph::types::{Direction, EdgeEndpoint, EdgeId, EdgeTypeId, LabelId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::eval::EvalCtx;
use super::super::expr::{CompiledExpr, EvalScratch, kernels};
use super::super::plan::SeedRow;
use super::super::values::index_key;
use super::super::vector::{VECTOR_CAPACITY, Vector, VectorBuilder};
use super::{PhysOperator, edge_ends_of, edge_ids_of, install};

/// Edge-type names -> interned ids; `None` for no filter. Unknown names match nothing.
pub(crate) fn type_filter(
    graph: &dyn GraphView,
    edge_types: &[String],
) -> Option<FxHashSet<EdgeTypeId>> {
    if edge_types.is_empty() {
        return None;
    }
    let st = graph.strings();
    Some(
        edge_types
            .iter()
            .filter_map(|t| st.get(t).map(EdgeTypeId))
            .collect(),
    )
}

/// Edges bound at `positions` of the current (flat) row, which this hop must not reuse.
fn used_edges(rs: &ResultSet, positions: &[DataPos], out: &mut Vec<EdgeId>) {
    out.clear();
    for &p in positions {
        edge_ids_of(rs.scalar_at(p), out);
    }
}

/// Single hop. From a flat source it fills a new group per source lane; in place
/// (`in_place`) the source group is unflat and its lanes are replicated per edge with the
/// relationship and neighbor appended, so a chunk of sources expands in a few chunks sized
/// by fan-out instead of one per lane.
pub struct ExpandOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    from: DataPos,
    rel: DataPos,
    to: DataPos,
    out_group: GroupId,
    dir: Direction,
    edge_types: &'p [String],
    prior_edges: &'p [DataPos],
    bound_to: Option<DataPos>,
    batch: AdjBatch,
    srcs: Vec<NodeId>,
    pos: usize,
    used: Vec<EdgeId>,
    in_place: bool,
    /// In place: the source group's columns and lane count as the chunk arrived; given back
    /// before the chain below runs again (it may return the same chunk at another flat
    /// position of some other group).
    orig_cols: Vec<Vector>,
    orig_len: usize,
    borrowed: bool,
    /// In place: the group lane of every source, in selection order (null sources dropped).
    src_lanes: Vec<u32>,
    /// In place: the other groups' selections as the chunk arrived, restored per emitted chunk.
    sels: Vec<Option<Vec<u32>>>,
    /// In place: the next source to expand and the first source of the current batch.
    src_pos: usize,
    slice_base: usize,
    /// In place: the edges each source's row already uses (empty when none are bound).
    used_per_src: Vec<Vec<EdgeId>>,
    /// In place: the bound target per source.
    targets: Vec<Option<NodeId>>,
    /// In place with one bound target for every source: its edges in the hop's reverse
    /// direction by neighbor, so a source's edges to it come from one lookup (`outgoing` as
    /// seen from the source).
    probe: Option<(NodeId, FxHashMap<NodeId, Vec<(EdgeId, EdgeTypeId, bool)>>)>,
    tf: Option<FxHashSet<EdgeTypeId>>,
    deg: Vec<usize>,
    /// In place under a row budget: lanes per chunk, from the budget doubling to a full vector
    /// (0 until the first chunk). Kept across `reset`, so re-runs of a seeded body pay the small
    /// first chunks once per query rather than once per run.
    chunk: usize,
}

impl<'p> ExpandOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        from: DataPos,
        rel: DataPos,
        to: DataPos,
        out_group: GroupId,
        dir: Direction,
        edge_types: &'p [String],
        prior_edges: &'p [DataPos],
        bound_to: Option<DataPos>,
        in_place: bool,
    ) -> Self {
        Self {
            input,
            from,
            rel,
            to,
            out_group,
            dir,
            edge_types,
            prior_edges,
            bound_to,
            batch: AdjBatch::new(),
            srcs: Vec::new(),
            pos: 0,
            used: Vec::new(),
            in_place,
            orig_cols: Vec::new(),
            orig_len: 0,
            borrowed: false,
            src_lanes: Vec::new(),
            sels: Vec::new(),
            src_pos: 0,
            slice_base: 0,
            used_per_src: Vec::new(),
            targets: Vec::new(),
            probe: None,
            tf: None,
            deg: Vec::new(),
            chunk: 0,
        }
    }

    /// Lanes per chunk in place: a full vector, or under a row budget
    /// ([`ExecCtx::row_budget`]) the budget at first, doubling with every chunk emitted. A
    /// `LIMIT 1` over a four-hop chain then expands one row per hop instead of filling a full
    /// chunk at every hop before the limit sees its first row.
    fn chunk_lanes(&mut self, cx: &ExecCtx<'_>) -> usize {
        match cx.row_budget() {
            None => VECTOR_CAPACITY,
            Some(b) => {
                if self.chunk == 0 {
                    self.chunk = b.clamp(1, VECTOR_CAPACITY);
                }
                self.chunk
            }
        }
    }

    fn next_in_place(
        &mut self,
        rs: &mut ResultSet,
        cx: &mut ExecCtx<'_>,
    ) -> Result<bool, CypherError> {
        loop {
            if self.pos < self.batch.len() {
                return self.emit_in_place(rs, cx);
            }
            if self.src_pos < self.srcs.len() {
                self.fill_slice(cx)?;
                if self.batch.is_empty() {
                    continue;
                }
                return self.emit_in_place(rs, cx);
            }
            if self.borrowed {
                let own = self.out_group;
                let g = rs.group_mut(own);
                g.cols = std::mem::take(&mut self.orig_cols);
                g.len = self.orig_len;
                g.sel = self.sels.get(own as usize).cloned().flatten();
                g.flat = None;
                g.active = true;
                self.borrowed = false;
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            self.take_sources(rs, cx);
        }
    }

    /// Takes the source group's columns and lists its sources (with their used edges and bound
    /// targets) for the chunk that just arrived.
    fn take_sources(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) {
        self.srcs.clear();
        self.src_lanes.clear();
        self.used_per_src.clear();
        self.targets.clear();
        self.src_pos = 0;
        self.slice_base = 0;
        self.batch.clear();
        self.pos = 0;
        let own = self.out_group;
        let bound_scalar: Option<Option<NodeId>> = match self.bound_to {
            Some(bt) if bt.group != own => Some(rs.scalar_at(bt).node_id()),
            _ => None,
        };
        // prior edges in flat groups apply to every source
        let mut flat_used: Vec<EdgeId> = Vec::new();
        for &p in self.prior_edges {
            if p.group != own {
                edge_ids_of(rs.scalar_at(p), &mut flat_used);
            }
        }
        let g = rs.group_mut(own);
        if !g.active {
            return;
        }
        let lanes: Vec<u32> = (0..g.selected_len()).map(|p| g.lane(p) as u32).collect();
        let width = g.cols.len();
        self.orig_len = g.len;
        self.orig_cols =
            std::mem::replace(&mut g.cols, (0..width).map(|_| Vector::empty()).collect());
        self.borrowed = true;
        self.sels = rs.groups.iter().map(|g| g.sel.clone()).collect();
        let from = &self.orig_cols[self.from.col as usize];
        for &lane in &lanes {
            let Some(src) = from.get(lane as usize).node_id() else {
                continue;
            };
            self.srcs.push(src);
            self.src_lanes.push(lane);
            if !self.prior_edges.is_empty() {
                let mut used = flat_used.clone();
                for &p in self.prior_edges {
                    if p.group == own {
                        edge_ids_of(self.orig_cols[p.col as usize].get(lane as usize), &mut used);
                    }
                }
                self.used_per_src.push(used);
            }
            if let Some(bt) = self.bound_to {
                self.targets.push(match bound_scalar {
                    Some(t) => t,
                    None => self.orig_cols[bt.col as usize].get(lane as usize).node_id(),
                });
            }
        }
        self.tf = type_filter(cx.view(), self.edge_types);
        // one target for every source (a flat one, or one replicated along the row): its
        // neighbors as a set
        let same_target = match bound_scalar {
            Some(t) => t,
            None => self
                .targets
                .first()
                .copied()
                .flatten()
                .filter(|t| self.targets.iter().all(|x| *x == Some(*t))),
        };
        // worth one pass over the target's edges when there are enough sources to look up
        if let Some(target) = same_target
            && self.srcs.len() >= 32
            && self.probe.as_ref().is_none_or(|(t, _)| *t != target)
        {
            let rev = match self.dir {
                Direction::Outgoing => Direction::Incoming,
                Direction::Incoming => Direction::Outgoing,
                Direction::Both => Direction::Both,
            };
            // straight off the adjacency, no edge records read
            self.batch.clear();
            let mut by_src: FxHashMap<NodeId, Vec<(EdgeId, EdgeTypeId, bool)>> =
                FxHashMap::default();
            if cx
                .view()
                .expand_batch(&[target], rev, &self.tf, &mut self.batch)
                .is_ok()
            {
                for i in 0..self.batch.len() {
                    by_src.entry(self.batch.neighbor[i]).or_default().push((
                        self.batch.edge[i],
                        self.batch.edge_type[i],
                        !self.batch.outgoing[i],
                    ));
                }
            }
            self.batch.clear();
            self.probe = Some((target, by_src));
        }
    }

    /// Expands the next sources into the batch: up to a chunk's worth of edges at a time (a
    /// single source may exceed it), or a chunk of sources when the target is bound.
    fn fill_slice(&mut self, cx: &mut ExecCtx<'_>) -> Result<(), CypherError> {
        self.batch.clear();
        self.pos = 0;
        let start = self.src_pos;
        let lanes = self.chunk_lanes(cx);
        let cap = (start + lanes).min(self.srcs.len());
        let view = cx.view();
        if self.bound_to.is_some() {
            for s in start..cap {
                let src = self.srcs[s];
                let Some(target) = self.targets[s] else {
                    continue;
                };
                if let Some((t, by_src)) = &self.probe
                    && *t == target
                {
                    if let Some(edges) = by_src.get(&src) {
                        for &(eid, et, outgoing) in edges {
                            if self.used_per_src.get(s).is_some_and(|u| u.contains(&eid)) {
                                continue;
                            }
                            self.batch
                                .push((s - start) as u32, eid, et, target, outgoing);
                        }
                    }
                    continue;
                }
                let dir = if self.dir == Direction::Both && src == target {
                    Direction::Outgoing
                } else {
                    self.dir
                };
                for eid in view.edges_between(src, target, dir, &self.tf)? {
                    if self.used_per_src.get(s).is_some_and(|u| u.contains(&eid)) {
                        continue;
                    }
                    let edge = view.get_edge(eid)?;
                    let outgoing = edge.source == src;
                    self.batch
                        .push((s - start) as u32, eid, edge.edge_type, target, outgoing);
                }
            }
            self.slice_base = start;
            self.src_pos = cap;
            return Ok(());
        }
        view.degrees(&self.srcs[start..cap], self.dir, &mut self.deg);
        let mut end = start;
        let mut work = 0usize;
        while end < cap {
            let d = self.deg[end - start];
            if end > start && work + d > lanes {
                break;
            }
            work += d;
            end += 1;
        }
        view.expand_batch(&self.srcs[start..end], self.dir, &self.tf, &mut self.batch)?;
        if !self.used_per_src.is_empty() {
            let used = &self.used_per_src;
            let keep: Vec<usize> = (0..self.batch.len())
                .filter(|&i| {
                    !used[start + self.batch.src_lane[i] as usize].contains(&self.batch.edge[i])
                })
                .collect();
            if keep.len() != self.batch.len() {
                let mut nb = AdjBatch::new();
                for i in keep {
                    nb.push(
                        self.batch.src_lane[i],
                        self.batch.edge[i],
                        self.batch.edge_type[i],
                        self.batch.neighbor[i],
                        self.batch.outgoing[i],
                    );
                }
                self.batch = nb;
            }
        }
        self.slice_base = start;
        self.src_pos = end;
        Ok(())
    }

    /// Emits the next chunk of the batch into the source group: its columns replicated by
    /// source lane, the relationship and neighbor appended.
    fn emit_in_place(
        &mut self,
        rs: &mut ResultSet,
        cx: &mut ExecCtx<'_>,
    ) -> Result<bool, CypherError> {
        let lanes = self.chunk_lanes(cx);
        if self.chunk != 0 {
            self.chunk = (self.chunk * 2).min(VECTOR_CAPACITY);
        }
        let end = (self.pos + lanes).min(self.batch.len());
        let n = end - self.pos;
        for (g, sel) in self.sels.iter().enumerate() {
            if g != self.out_group as usize {
                rs.groups[g].sel = sel.clone();
            }
        }
        let lanes: Vec<u32> = (self.pos..end)
            .map(|i| self.src_lanes[self.slice_base + self.batch.src_lane[i] as usize])
            .collect();
        let slice_srcs = &self.srcs[self.slice_base..];
        let mut rel_b = VectorBuilder::with_capacity(n);
        let mut to_b = VectorBuilder::with_capacity(n);
        for i in self.pos..end {
            rel_b.push_edge(
                self.batch.edge[i],
                self.batch.edge_type[i],
                self.batch.source(i, slice_srcs),
                self.batch.target(i, slice_srcs),
            );
            to_b.push_node(self.batch.neighbor[i]);
        }
        self.pos = end;
        let g = rs.group_mut(self.out_group);
        for (c, col) in g.cols.iter_mut().enumerate() {
            if c == self.rel.col as usize || c == self.to.col as usize {
                continue;
            }
            // a column not yet bound at this point of the plan holds nothing for these lanes
            let orig = &self.orig_cols[c];
            *col = if orig.len() != self.orig_len {
                Vector::nulls(n)
            } else {
                orig.take(&lanes)
            };
        }
        g.cols[self.rel.col as usize] = rel_b.finish();
        g.cols[self.to.col as usize] = to_b.finish();
        g.reset(n);
        cx.tick()?;
        Ok(true)
    }

    fn emit(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        let end = (self.pos + VECTOR_CAPACITY).min(self.batch.len());
        let n = end - self.pos;
        let mut rel_b = VectorBuilder::new();
        let mut to_b = VectorBuilder::new();
        for i in self.pos..end {
            rel_b.push_edge(
                self.batch.edge[i],
                self.batch.edge_type[i],
                self.batch.source(i, &self.srcs),
                self.batch.target(i, &self.srcs),
            );
            to_b.push_node(self.batch.neighbor[i]);
        }
        self.pos = end;
        install(
            rs,
            self.out_group,
            n,
            vec![(self.rel.col, rel_b.finish()), (self.to.col, to_b.finish())],
        );
        cx.tick()?;
        Ok(true)
    }
}

impl PhysOperator for ExpandOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.in_place {
            return self.next_in_place(rs, cx);
        }
        loop {
            if self.pos < self.batch.len() {
                return self.emit(rs, cx);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let Some(src) = rs.scalar_at(self.from).node_id() else {
                continue;
            };
            used_edges(rs, self.prior_edges, &mut self.used);
            self.batch.clear();
            self.pos = 0;
            self.srcs.clear();
            self.srcs.push(src);
            let tf = type_filter(cx.view(), self.edge_types);
            match self.bound_to {
                Some(bt) => {
                    let Some(target) = rs.scalar_at(bt).node_id() else {
                        continue;
                    };
                    // both endpoints known: only the edges between them, from the lower-degree
                    // side; a self-loop in an undirected pattern is listed once
                    let dir = if self.dir == Direction::Both && src == target {
                        Direction::Outgoing
                    } else {
                        self.dir
                    };
                    let ids = cx.view().edges_between(src, target, dir, &tf)?;
                    for eid in ids {
                        if self.used.contains(&eid) {
                            continue;
                        }
                        let edge = cx.view().get_edge(eid)?;
                        let outgoing = edge.source == src;
                        self.batch.push(0, eid, edge.edge_type, target, outgoing);
                    }
                }
                None => {
                    cx.view()
                        .expand_batch(&self.srcs, self.dir, &tf, &mut self.batch)?;
                    if !self.used.is_empty() {
                        let used = &self.used;
                        let keep: Vec<usize> = (0..self.batch.len())
                            .filter(|&i| !used.contains(&self.batch.edge[i]))
                            .collect();
                        if keep.len() != self.batch.len() {
                            let mut nb = AdjBatch::new();
                            for i in keep {
                                nb.push(
                                    self.batch.src_lane[i],
                                    self.batch.edge[i],
                                    self.batch.edge_type[i],
                                    self.batch.neighbor[i],
                                    self.batch.outgoing[i],
                                );
                            }
                            self.batch = nb;
                        }
                    }
                }
            }
            if self.batch.is_empty() {
                continue;
            }
            return self.emit(rs, cx);
        }
    }

    fn reset(&mut self) {
        self.batch.clear();
        self.pos = 0;
        self.srcs.clear();
        self.src_pos = 0;
        self.orig_cols.clear();
        self.borrowed = false;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Paths of `min..=max` hops from `start`, as `(end node, edges)`, under the statement's path
/// mode: TRAIL keeps edges unique within a path, ACYCLIC also nodes, WALK neither.
pub(crate) fn variable_length_paths(
    graph: &dyn GraphView,
    start: NodeId,
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
    min: usize,
    max: usize,
) -> Result<Vec<(NodeId, Vec<EdgeEndpoint>)>, CypherError> {
    let path_mode = crate::session::current_path_mode();
    let mut results = Vec::new();
    let mut charge = crate::memory::Charge::new();
    let mut stack: Vec<(
        NodeId,
        Vec<EdgeEndpoint>,
        FxHashSet<EdgeId>,
        FxHashSet<NodeId>,
    )> = Vec::new();
    let mut start_nodes = FxHashSet::default();
    if matches!(path_mode, crate::ast::PathMode::Acyclic) {
        start_nodes.insert(start);
    }
    stack.push((start, Vec::new(), FxHashSet::default(), start_nodes));
    let mut batch = AdjBatch::new();
    while let Some((current, path, used_edges, visited_nodes)) = stack.pop() {
        let depth = path.len();
        if depth >= min {
            charge.add(40 + depth * std::mem::size_of::<EdgeEndpoint>())?;
            results.push((current, path.clone()));
        }
        if depth >= max {
            continue;
        }
        batch.clear();
        graph.expand_batch(&[current], direction, type_filter, &mut batch)?;
        let srcs = [current];
        for i in 0..batch.len() {
            let ep = EdgeEndpoint {
                edge_id: batch.edge[i],
                edge_type: batch.edge_type[i],
                neighbor: batch.neighbor[i],
                source: batch.source(i, &srcs),
                target: batch.target(i, &srcs),
            };
            match path_mode {
                crate::ast::PathMode::Trail => {
                    if used_edges.contains(&ep.edge_id) {
                        continue;
                    }
                }
                crate::ast::PathMode::Acyclic => {
                    if used_edges.contains(&ep.edge_id) || visited_nodes.contains(&ep.neighbor) {
                        continue;
                    }
                }
                crate::ast::PathMode::Walk => {}
            }
            let mut new_path = path.clone();
            new_path.push(ep);
            let new_used = if matches!(path_mode, crate::ast::PathMode::Walk) {
                used_edges.clone()
            } else {
                let mut s = used_edges.clone();
                s.insert(ep.edge_id);
                s
            };
            let new_visited = if matches!(path_mode, crate::ast::PathMode::Acyclic) {
                let mut v = visited_nodes.clone();
                v.insert(ep.neighbor);
                v
            } else {
                visited_nodes.clone()
            };
            charge.add(96 + (depth + 1) * (std::mem::size_of::<EdgeEndpoint>() + 24))?;
            stack.push((ep.neighbor, new_path, new_used, new_visited));
        }
    }
    Ok(results)
}

/// Variable-length expansion from a flat source.
pub struct VarExpandOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    from: DataPos,
    rel: DataPos,
    to: DataPos,
    out_group: GroupId,
    dir: Direction,
    edge_types: &'p [String],
    min: usize,
    max: usize,
    prior_edges: &'p [DataPos],
    bound_to: Option<DataPos>,
    pending: Vec<(NodeId, Vec<EdgeEndpoint>)>,
    pos: usize,
}

impl<'p> VarExpandOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        from: DataPos,
        rel: DataPos,
        to: DataPos,
        out_group: GroupId,
        dir: Direction,
        edge_types: &'p [String],
        min: usize,
        max: usize,
        prior_edges: &'p [DataPos],
        bound_to: Option<DataPos>,
    ) -> Self {
        Self {
            input,
            from,
            rel,
            to,
            out_group,
            dir,
            edge_types,
            min,
            max,
            prior_edges,
            bound_to,
            pending: Vec::new(),
            pos: 0,
        }
    }
}

/// An edge list as a result value.
pub(crate) fn edge_list_value(path: &[EdgeEndpoint]) -> CypherValue {
    CypherValue::List(
        path.iter()
            .map(|ep| CypherValue::EdgeRef {
                id: ep.edge_id,
                edge_type_id: ep.edge_type,
                source: ep.source,
                target: ep.target,
            })
            .collect(),
    )
}

impl PhysOperator for VarExpandOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.pos < self.pending.len() {
                let end = (self.pos + VECTOR_CAPACITY).min(self.pending.len());
                let n = end - self.pos;
                let mut rel_b = VectorBuilder::new();
                let mut to_b = VectorBuilder::new();
                for (end_node, path) in &self.pending[self.pos..end] {
                    rel_b.push_any(edge_list_value(path));
                    to_b.push_node(*end_node);
                }
                self.pos = end;
                if self.pos >= self.pending.len() {
                    self.pending.clear();
                    self.pos = 0;
                }
                install(
                    rs,
                    self.out_group,
                    n,
                    vec![(self.rel.col, rel_b.finish()), (self.to.col, to_b.finish())],
                );
                cx.tick()?;
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let Some(src) = rs.scalar_at(self.from).node_id() else {
                continue;
            };
            let mut used = Vec::new();
            used_edges(rs, self.prior_edges, &mut used);
            let bound = self.bound_to.and_then(|p| rs.scalar_at(p).node_id());
            let tf = type_filter(cx.view(), self.edge_types);
            let paths = variable_length_paths(cx.view(), src, self.dir, &tf, self.min, self.max)?;
            self.pending = paths
                .into_iter()
                .filter(|(end, path)| {
                    !path.iter().any(|ep| used.contains(&ep.edge_id))
                        && bound.is_none_or(|b| b == *end)
                })
                .collect();
            self.pos = 0;
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.pos = 0;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Per node, its `(predecessor, relationship)` pairs one walk layer down.
type Preds = FxHashMap<NodeId, Vec<(NodeId, EdgeId)>>;

/// Predecessor pairs kept per run at most; past it a layer's trail steps scan the adjacency.
const PRED_CAP: usize = 1 << 20;

/// Is there a trail of `depth` relationships from a source (`walk[0]`) to `v`, none of them in
/// `used`? Searched backwards through the walk layers (`walk[d]`: reached by a walk of `d`
/// hops), along `preds[d]` where kept, else over the incoming relationships in `rev` (the
/// direction opposite the traversal). `steps` counts the edges looked at, for interrupt checks.
#[allow(clippy::too_many_arguments)]
fn trail_back(
    cx: &ExecCtx<'_>,
    walk: &[FxHashSet<NodeId>],
    preds: &[Option<Preds>],
    rev: Direction,
    tf: &Option<FxHashSet<EdgeTypeId>>,
    v: NodeId,
    depth: usize,
    used: &mut Vec<EdgeId>,
    steps: &mut usize,
) -> Result<bool, CypherError> {
    if depth == 0 {
        return Ok(walk[0].contains(&v));
    }
    let tick = |steps: &mut usize| -> Result<(), CypherError> {
        *steps += 1;
        if *steps & 0xFFF == 0 {
            cx.check()?;
        }
        Ok(())
    };
    if let Some(Some(level)) = preds.get(depth) {
        let Some(list) = level.get(&v) else {
            return Ok(false);
        };
        for &(u, e) in list {
            tick(steps)?;
            if used.contains(&e) {
                continue;
            }
            used.push(e);
            if trail_back(cx, walk, preds, rev, tf, u, depth - 1, used, steps)? {
                return Ok(true);
            }
            used.pop();
        }
        return Ok(false);
    }
    let mut batch = AdjBatch::new();
    cx.view().expand_batch(&[v], rev, tf, &mut batch)?;
    for i in 0..batch.len() {
        tick(steps)?;
        let (u, e) = (batch.neighbor[i], batch.edge[i]);
        if used.contains(&e) || !walk[depth - 1].contains(&u) {
            continue;
        }
        used.push(e);
        if trail_back(cx, walk, preds, rev, tf, u, depth - 1, used, steps)? {
            return Ok(true);
        }
        used.pop();
    }
    Ok(false)
}

/// Distinct targets at `min..=max` hops from a flat source, or from the sources of every input
/// row at once (`multi`, see `PhysNode::DistinctExpand::multi_source`).
pub struct DistinctExpandOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    from: DataPos,
    to: DataPos,
    out_group: GroupId,
    min: usize,
    max: usize,
    dir: Direction,
    edge_types: &'p [String],
    target_label: Option<&'p str>,
    global_dedup: bool,
    target_filter: Option<&'p CompiledExpr>,
    global_seen: FxHashSet<NodeId>,
    pending: Vec<NodeId>,
    pos: usize,
    scratch: EvalScratch<'p>,
    /// Multi-source: the input groups.
    multi: Option<&'p [GroupId]>,
    /// Multi-source: sources taken from the input, each once, in input order, and the next to
    /// expand. A repeated source reaches nothing new, so the buffer is bounded by the node count
    /// however many rows the input has.
    queue: Vec<NodeId>,
    queued: FxHashSet<NodeId>,
    qpos: usize,
    input_done: bool,
    /// Multi-source under a row budget: sources per walk, from the budget doubling (0 until the
    /// first walk; kept across `reset` like `ExpandOp::chunk`). Without a budget every source
    /// is taken before one walk.
    batch: usize,
    /// Multi-source: the input groups' `(group, len, sel, flat, active)` as the last input chunk
    /// left them, taken out of the row product while targets are emitted and handed back before
    /// the input runs again (a `Flatten` below may step through that same chunk).
    hidden: Vec<(GroupId, usize, Option<Vec<u32>>, Option<usize>, bool)>,
    /// The view's self-loop count, read once per run (`None` inside: unknown).
    self_loops: Option<Option<usize>>,
}

impl<'p> DistinctExpandOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        from: DataPos,
        to: DataPos,
        out_group: GroupId,
        min: usize,
        max: usize,
        dir: Direction,
        edge_types: &'p [String],
        target_label: Option<&'p str>,
        global_dedup: bool,
        multi: Option<&'p [GroupId]>,
        target_filter: Option<&'p CompiledExpr>,
    ) -> Self {
        Self {
            input,
            from,
            to,
            out_group,
            min,
            max,
            dir,
            edge_types,
            target_label,
            global_dedup,
            target_filter,
            global_seen: FxHashSet::default(),
            pending: Vec::new(),
            pos: 0,
            scratch: EvalScratch::new(),
            multi,
            queue: Vec::new(),
            queued: FxHashSet::default(),
            qpos: 0,
            input_done: false,
            batch: 0,
            hidden: Vec::new(),
            self_loops: None,
        }
    }

    /// The distinct targets at `min..=max` hops from `sources`, sorted, minus those seen before
    /// under a global dedup, with the target label: the ends of the trails (no relationship
    /// used twice, as in the pattern this replaces) of those lengths from any of them.
    ///
    /// One frontier walk per depth from all the sources finds every node some walk of that
    /// length reaches, a superset. Where the two can differ, each depth's walk is split by
    /// shortest distance: a node first reached at `d - 1` lies at the end of a shortest path,
    /// whose relationships all leave earlier nodes, so any relationship onward from it extends
    /// that path to a trail (undirected, except back to the node's own predecessor). Targets
    /// no such step reaches are checked by a search for a trail back to a source.
    fn targets_of(
        &mut self,
        sources: &[NodeId],
        cx: &mut ExecCtx<'_>,
    ) -> Result<Vec<NodeId>, CypherError> {
        let tf = type_filter(cx.view(), self.edge_types);
        let directed = self.dir != Direction::Both;
        let self_loops = *self
            .self_loops
            .get_or_insert_with(|| cx.view().self_loops());
        // walks reach exactly the trails' ends: directed with a one-hop minimum (a walk to
        // another node shortens to a path, a closed walk holds a cycle), or two directed hops
        // with no self-loop (only a self-loop takes one relationship twice in a row)
        let walks_exact = directed && (self.min <= 1 || (self.max <= 2 && self_loops == Some(0)));
        let src_set: FxHashSet<NodeId> = sources.iter().copied().collect();
        let mut collected: FxHashSet<NodeId> = FxHashSet::default();
        if self.min == 0 {
            // the zero-length path
            collected.extend(&src_set);
        }
        if walks_exact {
            let mut frontier = src_set;
            let mut frontier_vec: Vec<NodeId> = Vec::new();
            for depth in 1..=self.max {
                frontier_vec.clear();
                frontier_vec.extend(frontier.iter().copied());
                let mut next: FxHashSet<NodeId> = FxHashSet::default();
                cx.view()
                    .expand_frontier(&frontier_vec, self.dir, &tf, &mut next)?;
                frontier = next;
                if depth >= self.min {
                    collected.extend(&frontier);
                }
                cx.check()?;
                if frontier.is_empty() {
                    break;
                }
            }
        } else {
            // walk[d]: reached by a walk of d hops; shell[d]: first reached at d; preds[d]: for
            // the inner depths, each node's (predecessor in walk[d - 1], relationship), which
            // the trail search follows instead of scanning every incoming relationship
            let mut walk: Vec<FxHashSet<NodeId>> = vec![src_set.clone()];
            let mut shell: Vec<FxHashSet<NodeId>> = vec![src_set.clone()];
            let mut preds: Vec<Option<Preds>> = vec![None];
            let mut pred_entries = 0usize;
            let mut seen = src_set;
            let mut certified: FxHashSet<NodeId> = FxHashSet::default();
            let mut batch = AdjBatch::new();
            let rev = match self.dir {
                Direction::Outgoing => Direction::Incoming,
                Direction::Incoming => Direction::Outgoing,
                Direction::Both => Direction::Both,
            };
            let mut used: Vec<EdgeId> = Vec::new();
            let mut steps = 0usize;
            // a fixed length settles its targets at the last depth itself
            let mut settled = false;
            for depth in 1..=self.max {
                let (near, rest): (Vec<NodeId>, Vec<NodeId>) = walk[depth - 1]
                    .iter()
                    .copied()
                    .partition(|v| shell[depth - 1].contains(v));
                let mut onward: FxHashSet<NodeId> = FxHashSet::default();
                let mut next: FxHashSet<NodeId>;
                if depth < self.max {
                    let mut level: Option<Preds> = Some(FxHashMap::default());
                    // a fixed directed chain searches from depth max - 1 only out of nodes some
                    // shorter walk reached (the rest there): only theirs are kept
                    let only_seen = directed && self.min == self.max && depth + 1 == self.max;
                    next = FxHashSet::default();
                    for (srcs, is_near) in [(&near, true), (&rest, false)] {
                        for chunk in srcs.chunks(VECTOR_CAPACITY) {
                            batch.clear();
                            cx.view().expand_batch(chunk, self.dir, &tf, &mut batch)?;
                            for i in 0..batch.len() {
                                let (u, v) = (chunk[batch.src_lane[i] as usize], batch.neighbor[i]);
                                next.insert(v);
                                if is_near {
                                    onward.insert(v);
                                }
                                if only_seen && !seen.contains(&v) {
                                    continue;
                                }
                                if let Some(m) = level.as_mut() {
                                    if pred_entries < PRED_CAP {
                                        m.entry(v).or_default().push((u, batch.edge[i]));
                                        pred_entries += 1;
                                    } else {
                                        level = None;
                                    }
                                }
                            }
                            cx.check()?;
                        }
                    }
                    preds.push(level);
                } else if directed && self.min == self.max {
                    // a fixed directed chain's last depth: what the shell reaches ends trails;
                    // a target only the rest reaches has every predecessor there, and its
                    // trail is searched along them
                    let mut ends: FxHashSet<NodeId> = FxHashSet::default();
                    cx.view().expand_frontier(&near, self.dir, &tf, &mut ends)?;
                    let mut doubt: FxHashSet<NodeId> = FxHashSet::default();
                    let mut level: Option<Preds> = Some(FxHashMap::default());
                    for chunk in rest.chunks(VECTOR_CAPACITY) {
                        batch.clear();
                        cx.view().expand_batch(chunk, self.dir, &tf, &mut batch)?;
                        for i in 0..batch.len() {
                            let (u, v) = (chunk[batch.src_lane[i] as usize], batch.neighbor[i]);
                            if ends.contains(&v) {
                                continue;
                            }
                            doubt.insert(v);
                            if let Some(m) = level.as_mut() {
                                if pred_entries < PRED_CAP {
                                    m.entry(v).or_default().push((u, batch.edge[i]));
                                    pred_entries += 1;
                                } else {
                                    level = None;
                                }
                            }
                        }
                        cx.check()?;
                    }
                    preds.push(level);
                    for t in doubt {
                        used.clear();
                        if trail_back(cx, &walk, &preds, rev, &tf, t, depth, &mut used, &mut steps)?
                        {
                            ends.insert(t);
                        }
                    }
                    collected = ends;
                    settled = true;
                    break;
                } else if self.min == self.max {
                    // a fixed length's last depth (undirected here): what the shell reaches ends
                    // trails, but for a target back in the shell before it (perhaps reached only
                    // back along its predecessor's own relationship) and one only the rest
                    // reaches, each searched; one set, not a walk layer and a certified copy
                    let mut ends: FxHashSet<NodeId> = FxHashSet::default();
                    cx.view().expand_frontier(&near, self.dir, &tf, &mut ends)?;
                    let mut doubt: FxHashSet<NodeId> = FxHashSet::default();
                    if depth > 1 {
                        doubt.extend(shell[depth - 2].iter().filter(|v| ends.contains(v)));
                    }
                    for chunk in rest.chunks(VECTOR_CAPACITY) {
                        batch.clear();
                        cx.view().expand_batch(chunk, self.dir, &tf, &mut batch)?;
                        for &v in &batch.neighbor[..batch.len()] {
                            if !ends.contains(&v) {
                                doubt.insert(v);
                            }
                        }
                        cx.check()?;
                    }
                    for t in doubt {
                        used.clear();
                        if trail_back(cx, &walk, &preds, rev, &tf, t, depth, &mut used, &mut steps)?
                        {
                            ends.insert(t);
                        } else {
                            ends.remove(&t);
                        }
                    }
                    collected = ends;
                    settled = true;
                    break;
                } else {
                    // the last depth: what the shell reaches is the bulk, as a set; a target
                    // left in doubt has every predecessor in the rest, so only those are kept
                    cx.view()
                        .expand_frontier(&near, self.dir, &tf, &mut onward)?;
                    next = onward.clone();
                    // undirected, a target the shell reaches may still be in doubt (reached only
                    // back along its predecessor's own relationship): its search scans
                    let mut level: Option<Preds> = directed.then(FxHashMap::default);
                    for chunk in rest.chunks(VECTOR_CAPACITY) {
                        batch.clear();
                        cx.view().expand_batch(chunk, self.dir, &tf, &mut batch)?;
                        for i in 0..batch.len() {
                            let (u, v) = (chunk[batch.src_lane[i] as usize], batch.neighbor[i]);
                            next.insert(v);
                            if onward.contains(&v) {
                                continue;
                            }
                            if let Some(m) = level.as_mut() {
                                if pred_entries < PRED_CAP {
                                    m.entry(v).or_default().push((u, batch.edge[i]));
                                    pred_entries += 1;
                                } else {
                                    level = None;
                                }
                            }
                        }
                        cx.check()?;
                    }
                    preds.push(level);
                }
                if depth >= self.min {
                    if depth == 1 {
                        // one relationship is a trail
                        certified.extend(&next);
                    } else if directed {
                        certified.extend(&onward);
                    } else {
                        let back = &shell[depth - 2];
                        certified.extend(onward.iter().filter(|v| !back.contains(v)));
                    }
                    collected.extend(&next);
                }
                // nothing goes past the last depth: no shell for it
                if depth < self.max {
                    let first: FxHashSet<NodeId> =
                        next.iter().filter(|v| !seen.contains(v)).copied().collect();
                    seen.extend(&first);
                    shell.push(first);
                }
                let done = next.is_empty();
                walk.push(next);
                cx.check()?;
                if done {
                    break;
                }
            }
            let lo = self.min.max(1);
            let doubtful: Vec<NodeId> = collected
                .iter()
                .filter(|_| !settled)
                .filter(|t| !certified.contains(t) && !(self.min == 0 && walk[0].contains(t)))
                .copied()
                .collect();
            for t in doubtful {
                let mut ok = false;
                for depth in lo..walk.len() {
                    if walk[depth].contains(&t) {
                        used.clear();
                        if trail_back(cx, &walk, &preds, rev, &tf, t, depth, &mut used, &mut steps)?
                        {
                            ok = true;
                            break;
                        }
                    }
                }
                if !ok {
                    collected.remove(&t);
                }
            }
        }
        let mut ids: Vec<NodeId> = collected.into_iter().collect();
        ids.sort_unstable();
        if self.global_dedup {
            // under a target filter a target counts as seen once it passed (`emit_pending`): a
            // filter may read the source, and a target one source's row rejects may pass for
            // another's
            if self.target_filter.is_some() {
                ids.retain(|id| !self.global_seen.contains(id));
            } else {
                ids.retain(|id| self.global_seen.insert(*id));
            }
        }
        if let Some(label) = self.target_label {
            match cx.view().strings().get(label) {
                Some(raw) => {
                    let mut has = Vec::with_capacity(ids.len());
                    cx.view().nodes_have_label(&ids, LabelId(raw), &mut has);
                    ids = ids
                        .into_iter()
                        .zip(has)
                        .filter_map(|(id, ok)| ok.then_some(id))
                        .collect();
                }
                None => ids.clear(),
            }
        }
        Ok(ids)
    }

    /// Installs the next chunk of pending targets; `false` when the target filter drops all of
    /// them.
    fn emit_pending(
        &mut self,
        rs: &mut ResultSet,
        cx: &mut ExecCtx<'_>,
    ) -> Result<bool, CypherError> {
        let end = (self.pos + VECTOR_CAPACITY).min(self.pending.len());
        let ids = self.pending[self.pos..end].to_vec();
        self.pos = end;
        if self.pos >= self.pending.len() {
            self.pending.clear();
            self.pos = 0;
        }
        let n = ids.len();
        let seen = self.global_dedup && self.target_filter.is_some();
        let kept_ids = if seen { ids.clone() } else { Vec::new() };
        install(
            rs,
            self.out_group,
            n,
            vec![(self.to.col, Vector::nodes(ids))],
        );
        if let Some(f) = self.target_filter {
            let keep: Vec<u32> = {
                let ecx = EvalCtx::new(rs, cx.view(), Some(self.out_group));
                let mask = f.eval(&ecx, &mut self.scratch)?;
                (0..n)
                    .filter(|&i| kernels::lane_bool3(mask.get(i)) == Some(true))
                    .map(|i| i as u32)
                    .collect()
            };
            if seen {
                self.global_seen
                    .extend(keep.iter().map(|&i| kept_ids[i as usize]));
            }
            if keep.is_empty() {
                return Ok(false);
            }
            if keep.len() != n {
                rs.group_mut(self.out_group).narrow(keep);
            }
        }
        cx.tick()?;
        Ok(true)
    }

    /// Multi-source: the sources of every row of the input chunk in `rs`.
    fn take_sources(&mut self, rs: &ResultSet, in_groups: &[GroupId]) {
        if rs.row_count(in_groups) == 0 {
            return;
        }
        let g = rs.group(self.from.group);
        if !g.active {
            return;
        }
        let col = &g.cols[self.from.col as usize];
        let mut push = |id: Option<NodeId>| {
            if let Some(id) = id
                && self.queued.insert(id)
            {
                self.queue.push(id);
            }
        };
        match g.current_lane() {
            Some(lane) => push(col.get(lane).node_id()),
            None => {
                for p in 0..g.selected_len() {
                    push(col.get(g.lane(p)).node_id());
                }
            }
        }
    }

    /// Multi-source: takes the input groups out of the row product (once per input chunk).
    fn hide_input(&mut self, rs: &mut ResultSet, in_groups: &[GroupId]) {
        if !self.hidden.is_empty() {
            return;
        }
        for &g in in_groups {
            let gr = rs.group_mut(g);
            self.hidden
                .push((g, gr.len, gr.sel.take(), gr.flat, gr.active));
            gr.clear();
        }
    }

    /// Multi-source: hands the input groups back as the input left them.
    fn show_input(&mut self, rs: &mut ResultSet) {
        for (g, len, sel, flat, active) in self.hidden.drain(..) {
            let gr = rs.group_mut(g);
            gr.len = len;
            gr.sel = sel;
            gr.flat = flat;
            gr.active = active;
        }
    }

    fn next_multi(
        &mut self,
        in_groups: &'p [GroupId],
        rs: &mut ResultSet,
        cx: &mut ExecCtx<'_>,
    ) -> Result<bool, CypherError> {
        loop {
            if self.pos < self.pending.len() {
                self.hide_input(rs, in_groups);
                if self.emit_pending(rs, cx)? {
                    return Ok(true);
                }
                continue;
            }
            // sources for the next walk: all of them, or under a LIMIT a batch that doubles
            let want = match cx.row_budget() {
                None => usize::MAX,
                Some(b) => {
                    if self.batch == 0 {
                        self.batch = b.max(1);
                    }
                    self.batch
                }
            };
            while !self.input_done && self.queue.len() - self.qpos < want {
                self.show_input(rs);
                if self.input.next(rs, cx)? {
                    self.take_sources(rs, in_groups);
                } else {
                    self.input_done = true;
                }
            }
            if self.qpos >= self.queue.len() {
                return Ok(false);
            }
            let end = self.qpos.saturating_add(want).min(self.queue.len());
            if want != usize::MAX {
                self.batch = self.batch.saturating_mul(2);
            }
            let sources = std::mem::take(&mut self.queue);
            self.pending = self.targets_of(&sources[self.qpos..end], cx)?;
            self.pos = 0;
            self.queue = sources;
            self.qpos = end;
            if self.qpos == self.queue.len() {
                self.queue.clear();
                self.qpos = 0;
            }
        }
    }
}

impl PhysOperator for DistinctExpandOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if let Some(in_groups) = self.multi {
            return self.next_multi(in_groups, rs, cx);
        }
        loop {
            if self.pos < self.pending.len() {
                if self.emit_pending(rs, cx)? {
                    return Ok(true);
                }
                continue;
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let Some(src) = rs.scalar_at(self.from).node_id() else {
                continue;
            };
            self.pending = self.targets_of(&[src], cx)?;
            self.pos = 0;
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.pos = 0;
        self.global_seen.clear();
        self.queue.clear();
        self.queued.clear();
        self.qpos = 0;
        self.input_done = false;
        self.hidden.clear();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Index seek per flat input row.
pub struct IndexLookupJoinOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    key: &'p CompiledExpr,
    out: DataPos,
    out_group: GroupId,
    label: &'p str,
    property: &'p str,
    scratch: EvalScratch<'p>,
    pending: Vec<NodeId>,
    pos: usize,
}

impl<'p> IndexLookupJoinOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        key: &'p CompiledExpr,
        out: DataPos,
        out_group: GroupId,
        label: &'p str,
        property: &'p str,
    ) -> Self {
        Self {
            input,
            key,
            out,
            out_group,
            label,
            property,
            scratch: EvalScratch::new(),
            pending: Vec::new(),
            pos: 0,
        }
    }
}

impl PhysOperator for IndexLookupJoinOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.pos < self.pending.len() {
                let end = (self.pos + VECTOR_CAPACITY).min(self.pending.len());
                let ids = self.pending[self.pos..end].to_vec();
                self.pos = end;
                if self.pos >= self.pending.len() {
                    self.pending.clear();
                    self.pos = 0;
                }
                let n = ids.len();
                install(
                    rs,
                    self.out_group,
                    n,
                    vec![(self.out.col, Vector::nodes(ids))],
                );
                cx.tick()?;
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let v = self.key.eval_scalar(rs, cx.view(), &mut self.scratch)?;
            let ids = match index_key(&v) {
                Some(k) => cx
                    .view()
                    .index_lookup(self.label, self.property, &k)
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let mut exist = Vec::with_capacity(ids.len());
            cx.view().nodes_exist(&ids, &mut exist);
            self.pending = ids
                .into_iter()
                .zip(exist)
                .filter_map(|(id, ok)| ok.then_some(id))
                .collect();
            self.pos = 0;
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.pos = 0;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `SEARCH` subclause: the top-k vector hits per flat input row.
pub struct VectorSearchOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    out: DataPos,
    score: Option<DataPos>,
    out_group: GroupId,
    index_name: &'p str,
    query: &'p CompiledExpr,
    index_where: Option<&'p CompiledExpr>,
    limit: &'p CompiledExpr,
    scratch: EvalScratch<'p>,
}

impl<'p> VectorSearchOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        out: DataPos,
        score: Option<DataPos>,
        out_group: GroupId,
        index_name: &'p str,
        query: &'p CompiledExpr,
        index_where: Option<&'p CompiledExpr>,
        limit: &'p CompiledExpr,
    ) -> Self {
        Self {
            input,
            out,
            score,
            out_group,
            index_name,
            query,
            index_where,
            limit,
            scratch: EvalScratch::new(),
        }
    }

    /// Hits that pass the in-index filter, by evaluating it over a candidate chunk.
    fn filter_candidates(
        &mut self,
        rs: &mut ResultSet,
        cx: &mut ExecCtx<'_>,
        candidates: &[(NodeId, f64)],
        pred: &'p CompiledExpr,
        k: usize,
    ) -> Result<Vec<(NodeId, f64)>, CypherError> {
        let mut passing = Vec::with_capacity(k);
        for chunk in candidates.chunks(VECTOR_CAPACITY) {
            let ids: Vec<NodeId> = chunk.iter().map(|(n, _)| *n).collect();
            let n = ids.len();
            let mut cols = vec![(self.out.col, Vector::nodes(ids))];
            if let Some(s) = self.score {
                cols.push((
                    s.col,
                    Vector::Float {
                        vals: chunk.iter().map(|(_, sc)| *sc).collect(),
                        valid: vec![true; n],
                    },
                ));
            }
            install(rs, self.out_group, n, cols);
            let ecx = EvalCtx::new(rs, cx.view(), Some(self.out_group));
            let mask = pred.eval(&ecx, &mut self.scratch)?;
            for (i, c) in chunk.iter().enumerate() {
                if kernels::lane_bool3(mask.get(i)) == Some(true) {
                    passing.push(*c);
                    if passing.len() == k {
                        return Ok(passing);
                    }
                }
            }
        }
        Ok(passing)
    }
}

impl PhysOperator for VectorSearchOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let k = match self.limit.eval_scalar(rs, cx.view(), &mut self.scratch)? {
                CypherValue::Integer(n) if n >= 0 => n as usize,
                other => {
                    return Err(CypherError::type_error(format!(
                        "the LIMIT in a SEARCH clause must be a non-negative INTEGER, got {other:?}"
                    )));
                }
            };
            if k == 0 {
                continue;
            }
            let query = match self.query.eval_scalar(rs, cx.view(), &mut self.scratch)? {
                CypherValue::Null => continue,
                CypherValue::List(items) => crate::procedures::vector_from_list(&items)?,
                other => {
                    return Err(CypherError::type_error(format!(
                        "the query vector in a SEARCH clause must be a LIST<FLOAT | INTEGER>, got {}",
                        other.type_name()
                    )));
                }
            };
            let dim = cx
                .view()
                .vector_index_descriptors()
                .into_iter()
                .find(|(name, ..)| name == self.index_name)
                .map(|(_, _, _, dim, _)| dim)
                .ok_or_else(|| {
                    CypherError::semantic(format!(
                        "there is no such vector schema index: {}",
                        self.index_name
                    ))
                })?;
            if dim > 0 && query.len() != dim {
                return Err(CypherError::semantic(format!(
                    "Vector index {} has a configured dimensionality of {dim}, but the \
                     provided vector has a dimension {}",
                    self.index_name,
                    query.len()
                )));
            }
            let hits: Vec<(NodeId, f64)> = match self.index_where {
                None => cx
                    .view()
                    .query_vector_index(self.index_name, &query, k, None)
                    .unwrap_or_default(),
                Some(pred) => {
                    // filtered kNN: over-fetch and escalate until k candidates pass or the whole
                    // index was seen
                    let cap = cx.view().node_count().max(k);
                    let mut fetch = (k * 4).max(64).min(cap);
                    loop {
                        let candidates = cx
                            .view()
                            .query_vector_index(self.index_name, &query, fetch, None)
                            .unwrap_or_default();
                        let passing = self.filter_candidates(rs, cx, &candidates, pred, k)?;
                        if passing.len() >= k || fetch >= cap || candidates.len() < fetch {
                            break passing;
                        }
                        fetch = (fetch * 4).min(cap);
                        cx.check()?;
                    }
                }
            };
            let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
            let mut exist = Vec::with_capacity(ids.len());
            cx.view().nodes_exist(&ids, &mut exist);
            let hits: Vec<(NodeId, f64)> = hits
                .into_iter()
                .zip(exist)
                .filter_map(|(h, ok)| ok.then_some(h))
                .collect();
            if hits.is_empty() {
                continue;
            }
            let n = hits.len();
            let mut cols = vec![(
                self.out.col,
                Vector::nodes(hits.iter().map(|(id, _)| *id).collect()),
            )];
            if let Some(s) = self.score {
                cols.push((
                    s.col,
                    Vector::Float {
                        vals: hits.iter().map(|(_, sc)| *sc).collect(),
                        valid: vec![true; n],
                    },
                ));
            }
            install(rs, self.out_group, n, cols);
            cx.tick()?;
            return Ok(true);
        }
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// The edge count of every lane of a source column as an integer column of its group: the
/// degree when the far end needs no check and the graph has no other edge type to filter out,
/// else a count over the adjacency batch, with the far end's label tested on the neighbours.
pub struct ExpandCountOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    from: DataPos,
    out: DataPos,
    dir: Direction,
    edge_types: &'p [String],
    to_label: Option<&'p str>,
    prior_edges: &'p [DataPos],
    mode: Option<CountMode>,
    batch: AdjBatch,
    counts: Vec<usize>,
    /// Per source lane, the row's prior edges as `(id, source, target)`.
    priors: Vec<Vec<(EdgeId, NodeId, NodeId)>>,
}

enum CountMode {
    Degree,
    Scan {
        types: Option<FxHashSet<EdgeTypeId>>,
        label: Option<traverse_core::graph::types::LabelId>,
    },
}

impl<'p> ExpandCountOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        from: DataPos,
        out: DataPos,
        dir: Direction,
        edge_types: &'p [String],
        to_label: Option<&'p str>,
        prior_edges: &'p [DataPos],
    ) -> Self {
        Self {
            input,
            from,
            out,
            dir,
            edge_types,
            to_label,
            prior_edges,
            mode: None,
            batch: AdjBatch::new(),
            counts: Vec::new(),
            priors: Vec::new(),
        }
    }

    /// The prior edges of every source lane, from the source's group (per lane) or the flat
    /// groups (one value for all lanes).
    fn collect_priors(&mut self, rs: &ResultSet, lanes: &[usize]) {
        self.priors.clear();
        if self.prior_edges.is_empty() {
            return;
        }
        let mut flat = Vec::new();
        for &p in self.prior_edges {
            if p.group != self.from.group {
                edge_ends_of(rs.scalar_at(p), &mut flat);
            }
        }
        let g = rs.group(self.from.group);
        for &lane in lanes {
            let mut ends = flat.clone();
            for &p in self.prior_edges {
                if p.group == self.from.group {
                    edge_ends_of(g.cols[p.col as usize].get(lane), &mut ends);
                }
            }
            self.priors.push(ends);
        }
    }

    fn mode(&mut self, cx: &ExecCtx<'_>) -> &CountMode {
        if self.mode.is_none() {
            let graph = cx.view();
            let covers = self
                .to_label
                .is_none_or(|l| graph.count_nodes_by_label(l) == graph.node_count());
            let types = type_filter(graph, self.edge_types);
            // every edge is of the wanted type when the graph has no other
            let typed_ok = types.is_none()
                || (self.edge_types.len() == 1
                    && graph.relationship_type_names() == self.edge_types);
            // an undirected count lists a self-loop once; the degree would count it twice
            self.mode = Some(if covers && typed_ok && self.dir != Direction::Both {
                CountMode::Degree
            } else {
                CountMode::Scan {
                    types,
                    label: if covers {
                        None
                    } else {
                        self.to_label.and_then(|l| cx.label_id(l))
                    },
                }
            });
        }
        self.mode.as_ref().unwrap()
    }
}

impl PhysOperator for ExpandCountOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.input.next(rs, cx)? {
            return Ok(false);
        }
        let g = rs.group(self.from.group);
        let len = g.len;
        let lanes: Vec<usize> = match g.current_lane() {
            Some(l) => vec![l],
            None => g.selected_lanes().collect(),
        };
        let col = &g.cols[self.from.col as usize];
        let ids: Vec<NodeId> = lanes
            .iter()
            .map(|&l| col.node_id(l).unwrap_or(NodeId(u64::MAX)))
            .collect();
        self.mode(cx);
        self.counts.clear();
        self.collect_priors(rs, &lanes);
        let g = rs.group(self.from.group);
        let col = &g.cols[self.from.col as usize];
        match self.mode.as_ref().unwrap() {
            CountMode::Degree
                if g.current_lane().is_none() && lanes.len() * 4 >= len && len >= 64 =>
            {
                // most lanes selected: read every lane's degree in id order and pick
                let all: Vec<NodeId> = (0..len)
                    .map(|l| col.node_id(l).unwrap_or(NodeId(u64::MAX)))
                    .collect();
                let mut degrees = Vec::with_capacity(len);
                cx.view().degrees(&all, self.dir, &mut degrees);
                self.counts.extend(lanes.iter().map(|&l| degrees[l]));
            }
            CountMode::Degree => cx.view().degrees(&ids, self.dir, &mut self.counts),
            CountMode::Scan { types, label } => {
                self.counts.resize(ids.len(), 0);
                self.batch.clear();
                cx.view()
                    .expand_batch(&ids, self.dir, types, &mut self.batch)?;
                let priors = &self.priors;
                let fresh = |i: usize| {
                    priors.is_empty()
                        || !priors[self.batch.src_lane[i] as usize]
                            .iter()
                            .any(|(id, _, _)| *id == self.batch.edge[i])
                };
                match label {
                    None => {
                        for i in 0..self.batch.len() {
                            if fresh(i) {
                                self.counts[self.batch.src_lane[i] as usize] += 1;
                            }
                        }
                    }
                    Some(lid) => {
                        let mut has = Vec::with_capacity(self.batch.len());
                        cx.view()
                            .nodes_have_label(&self.batch.neighbor, *lid, &mut has);
                        for (i, ok) in has.into_iter().enumerate() {
                            if ok && fresh(i) {
                                self.counts[self.batch.src_lane[i] as usize] += 1;
                            }
                        }
                    }
                }
            }
        }
        // a degree counts the row's prior edges that start (end) at the source once each
        if !self.priors.is_empty() && matches!(self.mode, Some(CountMode::Degree)) {
            for (c, (ends, &src)) in self.counts.iter_mut().zip(self.priors.iter().zip(&ids)) {
                let dup = ends
                    .iter()
                    .filter(|(_, s, t)| match self.dir {
                        Direction::Outgoing => *s == src,
                        Direction::Incoming => *t == src,
                        Direction::Both => *s == src || *t == src,
                    })
                    .count();
                *c = c.saturating_sub(dup);
            }
        }
        let mut vals = vec![0i64; len];
        let mut valid = vec![false; len];
        for (&lane, &c) in lanes.iter().zip(&self.counts) {
            vals[lane] = c as i64;
            valid[lane] = true;
        }
        rs.group_mut(self.from.group).cols[self.out.col as usize] = Vector::Int { vals, valid };
        cx.tick()?;
        Ok(true)
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
