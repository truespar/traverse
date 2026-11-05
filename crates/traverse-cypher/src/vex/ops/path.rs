//! Shortest paths between bound endpoints: BFS variants and weighted Dijkstra, one input row at
//! a time (an unbound-endpoint sweep is an N² product that must never materialize).

use traverse_core::graph::types::{Direction, EdgeId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::ast::RelDirection;
use crate::error::CypherError;
use crate::executor::{
    bfs_all_shortest, bfs_k_groups, bfs_k_shortest, bfs_single_shortest, dijkstra_single_shortest,
};
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::plan::SeedRow;
use super::super::vector::{VECTOR_CAPACITY, Vector, VectorBuilder};
use super::{PhysOperator, install};

fn direction_of(d: &RelDirection) -> Direction {
    match d {
        RelDirection::Outgoing => Direction::Outgoing,
        RelDirection::Incoming => Direction::Incoming,
        RelDirection::Both => Direction::Both,
    }
}

/// The path value and its edge list for an edge sequence from `start`.
fn path_values(
    graph: &dyn GraphView,
    start_value: CypherValue,
    start: NodeId,
    dir: Direction,
    edges: &[EdgeId],
) -> Result<(CypherValue, CypherValue), CypherError> {
    let mut nodes: Vec<CypherValue> = Vec::with_capacity(edges.len() + 1);
    let mut rels: Vec<CypherValue> = Vec::with_capacity(edges.len());
    nodes.push(start_value);
    let mut current = start;
    for eid in edges {
        let edge = graph.get_edge(*eid).map_err(CypherError::from)?;
        rels.push(CypherValue::EdgeRef {
            id: *eid,
            edge_type_id: edge.edge_type,
            source: edge.source,
            target: edge.target,
        });
        let next = match dir {
            Direction::Outgoing => edge.target,
            Direction::Incoming => edge.source,
            Direction::Both => {
                if edge.source == current {
                    edge.target
                } else {
                    edge.source
                }
            }
        };
        current = next;
        nodes.push(CypherValue::NodeRef(next));
    }
    Ok((
        CypherValue::Path {
            nodes,
            edges: rels.clone(),
        },
        CypherValue::List(rels),
    ))
}

/// Pending `(path, edge list[, cost])` rows, emitted in chunks into the output group.
struct Pending {
    rows: Vec<(CypherValue, CypherValue, f64)>,
    pos: usize,
}

impl Pending {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            pos: 0,
        }
    }

    fn has_more(&self) -> bool {
        self.pos < self.rows.len()
    }

    fn emit(
        &mut self,
        rs: &mut ResultSet,
        out_group: GroupId,
        path: Option<DataPos>,
        rel: Option<DataPos>,
        cost: Option<DataPos>,
    ) {
        let end = (self.pos + VECTOR_CAPACITY).min(self.rows.len());
        let n = end - self.pos;
        let mut path_b = VectorBuilder::new();
        let mut rel_b = VectorBuilder::new();
        let mut costs = Vec::with_capacity(n);
        for (p, r, c) in self.rows.drain(self.pos..end) {
            path_b.push_any(p);
            rel_b.push_any(r);
            costs.push(c);
        }
        // drained rows: the rest shift down to the front
        self.rows.drain(..self.pos);
        self.pos = 0;
        let mut cols = Vec::new();
        if let Some(p) = path {
            cols.push((p.col, path_b.finish()));
        }
        if let Some(r) = rel {
            cols.push((r.col, rel_b.finish()));
        }
        if let Some(c) = cost {
            cols.push((
                c.col,
                Vector::Float {
                    vals: costs,
                    valid: vec![true; n],
                },
            ));
        }
        install(rs, out_group, n, cols);
    }
}

/// `shortestPath` / `allShortestPaths` / `SHORTEST k [GROUPS]`.
pub struct ShortestPathOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    from: DataPos,
    to: DataPos,
    edge_types: &'p [String],
    dir: Direction,
    min_depth: usize,
    max_depth: usize,
    path: Option<DataPos>,
    rel: Option<DataPos>,
    all: bool,
    k: Option<usize>,
    k_groups: bool,
    out_group: GroupId,
    pending: Pending,
}

impl<'p> ShortestPathOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        from: DataPos,
        to: DataPos,
        edge_types: &'p [String],
        direction: RelDirection,
        min_depth: usize,
        max_depth: usize,
        path: Option<DataPos>,
        rel: Option<DataPos>,
        all: bool,
        k: Option<usize>,
        k_groups: bool,
        out_group: GroupId,
    ) -> Self {
        Self {
            input,
            from,
            to,
            edge_types,
            dir: direction_of(&direction),
            min_depth,
            max_depth,
            path,
            rel,
            all,
            k,
            k_groups,
            out_group,
            pending: Pending::new(),
        }
    }
}

impl PhysOperator for ShortestPathOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.pending.has_more() {
                self.pending
                    .emit(rs, self.out_group, self.path, self.rel, None);
                cx.tick()?;
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            // both endpoints unbound is an N² sweep over node pairs: honour the deadline per pair
            cx.check()?;
            let Some(start) = rs.scalar_at(self.from).node_id() else {
                continue;
            };
            let Some(target) = rs.scalar_at(self.to).node_id() else {
                continue;
            };
            // the same node at both ends with a positive minimum length: no such path
            if self.k.is_none() && start == target && self.min_depth >= 1 {
                continue;
            }
            let graph = cx.view();
            let paths = match self.k {
                Some(k) if self.k_groups => bfs_k_groups(
                    graph,
                    start,
                    target,
                    self.dir,
                    self.edge_types,
                    self.min_depth,
                    self.max_depth,
                    k,
                )?,
                Some(k) => bfs_k_shortest(
                    graph,
                    start,
                    target,
                    self.dir,
                    self.edge_types,
                    self.min_depth,
                    self.max_depth,
                    k,
                )?,
                None if self.all => bfs_all_shortest(
                    graph,
                    start,
                    target,
                    self.dir,
                    self.edge_types,
                    self.min_depth,
                    self.max_depth,
                )?,
                None => bfs_single_shortest(
                    graph,
                    start,
                    target,
                    self.dir,
                    self.edge_types,
                    self.min_depth,
                    self.max_depth,
                )?,
            };
            if paths.is_empty() {
                continue;
            }
            let start_value = rs.scalar_at(self.from).to_owned();
            for edges in &paths {
                let (p, r) = path_values(graph, start_value.clone(), start, self.dir, edges)?;
                self.pending.rows.push((p, r, 0.0));
            }
        }
    }

    fn reset(&mut self) {
        self.pending = Pending::new();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Weighted shortest path (Dijkstra) between bound endpoints.
pub struct WeightedShortestPathOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    from: DataPos,
    to: DataPos,
    edge_types: &'p [String],
    dir: Direction,
    weight_property: &'p str,
    path: Option<DataPos>,
    rel: Option<DataPos>,
    cost: Option<DataPos>,
    out_group: GroupId,
    pending: Pending,
}

impl<'p> WeightedShortestPathOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        from: DataPos,
        to: DataPos,
        edge_types: &'p [String],
        direction: RelDirection,
        weight_property: &'p str,
        path: Option<DataPos>,
        rel: Option<DataPos>,
        cost: Option<DataPos>,
        out_group: GroupId,
    ) -> Self {
        Self {
            input,
            from,
            to,
            edge_types,
            dir: direction_of(&direction),
            weight_property,
            path,
            rel,
            cost,
            out_group,
            pending: Pending::new(),
        }
    }
}

impl PhysOperator for WeightedShortestPathOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.pending.has_more() {
                self.pending
                    .emit(rs, self.out_group, self.path, self.rel, self.cost);
                cx.tick()?;
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            cx.check()?;
            let Some(start) = rs.scalar_at(self.from).node_id() else {
                continue;
            };
            let Some(target) = rs.scalar_at(self.to).node_id() else {
                continue;
            };
            let graph = cx.view();
            let Some((edges, total)) = dijkstra_single_shortest(
                graph,
                start,
                target,
                self.dir,
                self.edge_types,
                self.weight_property,
            )?
            else {
                continue;
            };
            let start_value = rs.scalar_at(self.from).to_owned();
            let (p, r) = path_values(graph, start_value, start, self.dir, &edges)?;
            self.pending.rows.push((p, r, total));
        }
    }

    fn reset(&mut self) {
        self.pending = Pending::new();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
