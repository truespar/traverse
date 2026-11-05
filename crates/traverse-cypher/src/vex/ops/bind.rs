//! Bindings derived from columns already in the chunk: paths, quantified-group lists, list
//! variables, and the endpoints of pre-bound edges and edge lists.

use traverse_core::graph::types::NodeId;

use crate::ast::RelDirection;
use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::eval::EvalCtx;
use super::super::plan::SeedRow;
use super::super::vector::{ValueRef, Vector};
use super::{PhysOperator, Staged, install, move_cols};

/// Fills column `out` of its group from `f(ecx, i)` per selected lane (or once, for a group that
/// has no lanes yet).
fn bind_column<F>(
    rs: &mut ResultSet,
    graph: &dyn traverse_core::tx::view::GraphView,
    out: DataPos,
    mut f: F,
) where
    F: FnMut(&EvalCtx<'_>, usize) -> CypherValue,
{
    let g = out.group;
    let active = rs.group(g).active;
    let vec = {
        let ecx = EvalCtx::new(rs, graph, if active { Some(g) } else { None });
        let n = ecx.n;
        let vals: Vec<CypherValue> = (0..n).map(|i| f(&ecx, i)).collect();
        if !active {
            Vector::from_values(vals)
        } else {
            let group = rs.group(g);
            match &group.sel {
                None => Vector::from_values(vals),
                Some(sel) => {
                    let mut full = vec![CypherValue::Null; group.len];
                    for (i, &lane) in sel.iter().enumerate() {
                        full[lane as usize] = vals[i].clone();
                    }
                    Vector::from_values(full)
                }
            }
        }
    };
    if active {
        rs.group_mut(g).cols[out.col as usize] = vec;
    } else {
        install(rs, g, 1, vec![(out.col, vec)]);
    }
}

/// A path from node and edge columns.
pub struct BindPathOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    out: DataPos,
    node_vars: &'p [DataPos],
    edge_vars: &'p [DataPos],
}

impl<'p> BindPathOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        out: DataPos,
        node_vars: &'p [DataPos],
        edge_vars: &'p [DataPos],
    ) -> Self {
        Self {
            input,
            out,
            node_vars,
            edge_vars,
        }
    }
}

/// The path over `nodes` and `edges` (each edge a relationship or a list of them).
pub(crate) fn build_path(nodes: &[CypherValue], edges: &[CypherValue]) -> CypherValue {
    let mut path_nodes: Vec<CypherValue> = Vec::new();
    let mut path_edges: Vec<CypherValue> = Vec::new();
    if let Some(first) = nodes.first() {
        path_nodes.push(first.clone());
    }
    for (i, val) in edges.iter().enumerate() {
        match val {
            CypherValue::List(items) => {
                if !items.is_empty() {
                    let item_count = items.len();
                    let mut current_id = match path_nodes.last() {
                        Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => *id,
                        _ => NodeId(0),
                    };
                    for (j, edge_val) in items.iter().enumerate() {
                        let (src, tgt) = match edge_val {
                            CypherValue::Edge { source, target, .. }
                            | CypherValue::EdgeRef { source, target, .. } => {
                                (Some(*source), Some(*target))
                            }
                            _ => (None, None),
                        };
                        path_edges.push(edge_val.clone());
                        if let (Some(src), Some(tgt)) = (src, tgt) {
                            let next_id = if src == current_id { tgt } else { src };
                            current_id = next_id;
                            if j < item_count - 1 {
                                path_nodes.push(CypherValue::NodeRef(next_id));
                            }
                        }
                    }
                    if let Some(n) = nodes.get(i + 1) {
                        path_nodes.push(n.clone());
                    }
                }
            }
            other => {
                path_edges.push(other.clone());
                if let Some(n) = nodes.get(i + 1) {
                    path_nodes.push(n.clone());
                }
            }
        }
    }
    CypherValue::Path {
        nodes: path_nodes,
        edges: path_edges,
    }
}

impl PhysOperator for BindPathOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.input.next(rs, cx)? {
            return Ok(false);
        }
        let node_vars = self.node_vars;
        let edge_vars = self.edge_vars;
        bind_column(rs, cx.view(), self.out, |ecx, i| {
            let nodes: Vec<CypherValue> = node_vars
                .iter()
                .map(|p| ecx.lane(*p, i).to_owned())
                .collect();
            let edges: Vec<CypherValue> = edge_vars
                .iter()
                .map(|p| ecx.lane(*p, i).to_owned())
                .collect();
            build_path(&nodes, &edges)
        });
        Ok(true)
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// The per-hop source and target lists of a quantified group, walked from the anchor.
pub struct BindQppGroupsOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    anchor: DataPos,
    rel: DataPos,
    src: Option<DataPos>,
    dst: Option<DataPos>,
}

impl<'p> BindQppGroupsOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        anchor: DataPos,
        rel: DataPos,
        src: Option<DataPos>,
        dst: Option<DataPos>,
    ) -> Self {
        Self {
            input,
            anchor,
            rel,
            src,
            dst,
        }
    }
}

/// `(sources, targets)` of the hops in `rel` from `anchor`; `None` for a null anchor or list.
fn qpp_lists(anchor: ValueRef<'_>, rel: ValueRef<'_>) -> Option<(CypherValue, CypherValue)> {
    let anchor = anchor.node_id()?;
    let edges: Vec<(NodeId, NodeId)> = match rel {
        ValueRef::Any(CypherValue::List(items)) => items
            .iter()
            .map(|e| match e {
                CypherValue::EdgeRef { source, target, .. } => Some((*source, *target)),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?,
        ValueRef::Edge { src, tgt, .. } => vec![(src, tgt)],
        _ => return None,
    };
    let mut cur = anchor;
    let mut srcs = Vec::with_capacity(edges.len());
    let mut dsts = Vec::with_capacity(edges.len());
    for (s, t) in edges {
        let next = if s == cur { t } else { s };
        srcs.push(CypherValue::NodeRef(cur));
        dsts.push(CypherValue::NodeRef(next));
        cur = next;
    }
    Some((CypherValue::List(srcs), CypherValue::List(dsts)))
}

impl PhysOperator for BindQppGroupsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.input.next(rs, cx)? {
            return Ok(false);
        }
        let anchor = self.anchor;
        let rel = self.rel;
        if let Some(out) = self.src {
            bind_column(rs, cx.view(), out, |ecx, i| {
                qpp_lists(ecx.lane(anchor, i), ecx.lane(rel, i))
                    .map(|(s, _)| s)
                    .unwrap_or(CypherValue::Null)
            });
        }
        if let Some(out) = self.dst {
            bind_column(rs, cx.view(), out, |ecx, i| {
                qpp_lists(ecx.lane(anchor, i), ecx.lane(rel, i))
                    .map(|(_, d)| d)
                    .unwrap_or(CypherValue::Null)
            });
        }
        Ok(true)
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// List variables from member columns.
pub struct BindVarListsOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    groups: &'p [(DataPos, Vec<Option<DataPos>>)],
}

impl<'p> BindVarListsOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        groups: &'p [(DataPos, Vec<Option<DataPos>>)],
    ) -> Self {
        Self { input, groups }
    }
}

impl PhysOperator for BindVarListsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.input.next(rs, cx)? {
            return Ok(false);
        }
        for (out, members) in self.groups {
            bind_column(rs, cx.view(), *out, |ecx, i| {
                CypherValue::List(
                    members
                        .iter()
                        .map(|m| match m {
                            Some(p) => ecx.lane(*p, i).to_owned(),
                            None => CypherValue::Null,
                        })
                        .collect(),
                )
            });
        }
        Ok(true)
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Does a bound endpoint column admit `id`? Null does; a node must be `id`; anything else doesn't.
fn endpoint_admits(v: ValueRef<'_>, id: NodeId) -> bool {
    match v {
        ValueRef::Null => true,
        ValueRef::Node(n) => n == id,
        ValueRef::Any(CypherValue::Node { id: n, .. }) => *n == id,
        ValueRef::Any(CypherValue::Null) => true,
        _ => false,
    }
}

/// Shared shape of the two endpoint binders: a row consumer that stages the surviving rows.
struct EndpointRows<'p> {
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: Vec<(DataPos, u16)>,
    from: DataPos,
    to: DataPos,
    from_bound: Option<DataPos>,
    to_bound: Option<DataPos>,
    staged: Option<Staged>,
    done: bool,
}

impl EndpointRows<'_> {
    /// Stages the current row with `from`/`to` bound when the bound endpoints admit them.
    fn push(
        &self,
        st: &mut Staged,
        rs: &ResultSet,
        lanes: &[usize],
        from_id: NodeId,
        to_id: NodeId,
    ) -> Result<(), CypherError> {
        if let Some(b) = self.from_bound
            && !endpoint_admits(rs.value_at(b, lanes), from_id)
        {
            return Ok(());
        }
        if let Some(b) = self.to_bound
            && !endpoint_admits(rs.value_at(b, lanes), to_id)
        {
            return Ok(());
        }
        let from_col = self.from.col as usize;
        let to_col = self.to.col as usize;
        st.push_row(|cols| {
            for (src, c) in &self.moves {
                cols[*c as usize].push(rs.value_at(*src, lanes));
            }
            cols[from_col].push_node(from_id);
            cols[to_col].push_node(to_id);
        })
    }
}

/// Endpoints of a pre-bound variable-length edge list.
pub struct BindPathEndpointsOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    rel: DataPos,
    direction: RelDirection,
    rows: EndpointRows<'p>,
}

impl<'p> BindPathEndpointsOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        rel: DataPos,
        from: DataPos,
        to: DataPos,
        direction: RelDirection,
        out_group: GroupId,
        in_groups: &'p [GroupId],
        moves: &'p [(DataPos, DataPos)],
        from_bound: Option<DataPos>,
        to_bound: Option<DataPos>,
    ) -> Self {
        Self {
            input,
            rel,
            direction,
            rows: EndpointRows {
                in_groups,
                out_group,
                moves: move_cols(moves),
                from,
                to,
                from_bound,
                to_bound,
                staged: None,
                done: false,
            },
        }
    }
}

/// `(start, end)` of a connected edge list walked in `direction`; `None` if it isn't one.
pub(crate) fn list_endpoints(
    edges: &[CypherValue],
    direction: &RelDirection,
) -> Option<(NodeId, NodeId)> {
    if edges.is_empty() {
        return None;
    }
    let mut current: Option<NodeId> = None;
    let mut start: Option<NodeId> = None;
    for (i, cv) in edges.iter().enumerate() {
        let (src, tgt) = match cv {
            CypherValue::Edge { source, target, .. }
            | CypherValue::EdgeRef { source, target, .. } => (*source, *target),
            _ => return None,
        };
        let (step_from, step_to) = match direction {
            RelDirection::Outgoing => (src, tgt),
            RelDirection::Incoming => (tgt, src),
            RelDirection::Both => match current {
                Some(cur) if src == cur => (src, tgt),
                Some(cur) if tgt == cur => (tgt, src),
                Some(_) => return None,
                None => (src, tgt),
            },
        };
        if i == 0 {
            start = Some(step_from);
            current = Some(step_to);
        } else if current == Some(step_from) {
            current = Some(step_to);
        } else {
            return None;
        }
    }
    Some((start?, current?))
}

impl PhysOperator for BindPathEndpointsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(st) = &mut self.rows.staged {
                if st.emit(rs, self.rows.out_group)? {
                    return Ok(true);
                }
                self.rows.staged = None;
            }
            if self.rows.done || !self.input.next(rs, cx)? {
                self.rows.done = true;
                return Ok(false);
            }
            let width = rs.group(self.rows.out_group).cols.len();
            let mut st = Staged::new(width);
            let rel = self.rel;
            let direction = &self.direction;
            let rows = &self.rows;
            rs.for_each_row(self.rows.in_groups, |lanes| {
                let ValueRef::Any(CypherValue::List(edges)) = rs.value_at(rel, lanes) else {
                    return Ok(());
                };
                let Some((start, end)) = list_endpoints(edges, direction) else {
                    return Ok(());
                };
                rows.push(&mut st, rs, lanes, start, end)
            })?;
            cx.tick()?;
            self.rows.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.rows.staged = None;
        self.rows.done = false;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Endpoints of a pre-bound single edge.
pub struct BindEdgeEndpointsOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    edge: DataPos,
    direction: RelDirection,
    edge_types: &'p [String],
    rows: EndpointRows<'p>,
}

impl<'p> BindEdgeEndpointsOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        edge: DataPos,
        from: DataPos,
        to: DataPos,
        direction: RelDirection,
        edge_types: &'p [String],
        out_group: GroupId,
        in_groups: &'p [GroupId],
        moves: &'p [(DataPos, DataPos)],
        from_bound: Option<DataPos>,
        to_bound: Option<DataPos>,
    ) -> Self {
        Self {
            input,
            edge,
            direction,
            edge_types,
            rows: EndpointRows {
                in_groups,
                out_group,
                moves: move_cols(moves),
                from,
                to,
                from_bound,
                to_bound,
                staged: None,
                done: false,
            },
        }
    }
}

impl PhysOperator for BindEdgeEndpointsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(st) = &mut self.rows.staged {
                if st.emit(rs, self.rows.out_group)? {
                    return Ok(true);
                }
                self.rows.staged = None;
            }
            if self.rows.done || !self.input.next(rs, cx)? {
                self.rows.done = true;
                return Ok(false);
            }
            // a required type the graph never interned matches no edge
            let type_ids: Option<Vec<u32>> = if self.edge_types.is_empty() {
                None
            } else {
                let st = cx.view().strings();
                let ids: Vec<u32> = self.edge_types.iter().filter_map(|t| st.get(t)).collect();
                if ids.is_empty() {
                    continue;
                }
                Some(ids)
            };
            let width = rs.group(self.rows.out_group).cols.len();
            let mut st = Staged::new(width);
            let edge = self.edge;
            let direction = &self.direction;
            let rows = &self.rows;
            let graph = cx.view();
            rs.for_each_row(self.rows.in_groups, |lanes| {
                let (etid, src, tgt) = match rs.value_at(edge, lanes) {
                    ValueRef::Edge {
                        etype, src, tgt, ..
                    } => (etype.0, src, tgt),
                    ValueRef::Any(CypherValue::EdgeRef {
                        edge_type_id,
                        source,
                        target,
                        ..
                    }) => (edge_type_id.0, *source, *target),
                    ValueRef::Any(CypherValue::Edge {
                        edge_type,
                        source,
                        target,
                        ..
                    }) => match graph.strings().get(edge_type) {
                        Some(tid) => (tid, *source, *target),
                        None => return Ok(()),
                    },
                    _ => return Ok(()),
                };
                if let Some(ids) = &type_ids
                    && !ids.contains(&etid)
                {
                    return Ok(());
                }
                match direction {
                    RelDirection::Outgoing => rows.push(&mut st, rs, lanes, src, tgt),
                    RelDirection::Incoming => rows.push(&mut st, rs, lanes, tgt, src),
                    RelDirection::Both => {
                        rows.push(&mut st, rs, lanes, src, tgt)?;
                        if src != tgt {
                            rows.push(&mut st, rs, lanes, tgt, src)?;
                        }
                        Ok(())
                    }
                }
            })?;
            cx.tick()?;
            self.rows.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.rows.staged = None;
        self.rows.done = false;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
