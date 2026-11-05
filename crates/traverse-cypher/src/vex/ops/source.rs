//! Sources: scans, seeks, index ranges, edge scans, seeds, counts.

use std::ops::Bound;

use traverse_core::graph::batch::{ColumnBound, ColumnCmp};
use traverse_core::graph::types::{Direction, EdgeId, NodeId};

use crate::ast::CmpOp;
use crate::error::CypherError;
use crate::planner::TextPredicateKind;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::{CompiledExpr, EvalScratch, eval::eval_constant};
use super::super::plan::{CountKind, NodeSeek, PhysNode, PhysSpatialQuery, SeedRow};
use super::super::values::index_key;
use super::super::vector::{VECTOR_CAPACITY, Vector, VectorBuilder};
use super::{PhysOperator, install};

/// One row without columns.
pub struct EmptyRowOp {
    group: GroupId,
    done: bool,
}

impl EmptyRowOp {
    pub(crate) fn new(group: GroupId) -> Self {
        Self { group, done: false }
    }
}

impl PhysOperator for EmptyRowOp {
    fn next(&mut self, rs: &mut ResultSet, _cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.done {
            return Ok(false);
        }
        self.done = true;
        install(rs, self.group, 1, Vec::new());
        Ok(true)
    }

    fn reset(&mut self) {
        self.done = false;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Rows supplied at run time.
pub struct SeedOp<'p> {
    group: GroupId,
    cols: &'p [DataPos],
    rows: Vec<SeedRow>,
    pos: usize,
}

impl<'p> SeedOp<'p> {
    pub(crate) fn new(group: GroupId, cols: &'p [DataPos]) -> Self {
        Self {
            group,
            cols,
            rows: Vec::new(),
            pos: 0,
        }
    }
}

impl PhysOperator for SeedOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, _cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.pos >= self.rows.len() {
            return Ok(false);
        }
        let end = (self.pos + VECTOR_CAPACITY).min(self.rows.len());
        let n = end - self.pos;
        let mut builders: Vec<VectorBuilder> =
            (0..self.cols.len()).map(|_| VectorBuilder::new()).collect();
        for row in &self.rows[self.pos..end] {
            for (c, b) in builders.iter_mut().enumerate() {
                match row.get(c) {
                    Some(v) => b.push_owned(v.clone()),
                    None => b.push_owned(CypherValue::Null),
                }
            }
        }
        let cols: Vec<(u16, Vector)> = self
            .cols
            .iter()
            .zip(builders)
            .map(|(p, b)| (p.col, b.finish()))
            .collect();
        install(rs, self.group, n, cols);
        self.pos = end;
        Ok(true)
    }

    fn reset(&mut self) {
        self.pos = 0;
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.rows = rows;
        self.pos = 0;
    }
}

/// Runs `inner` to completion for every chunk of `input`.
pub struct NestOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    inner: Box<dyn PhysOperator + 'p>,
    have_outer: bool,
}

impl<'p> NestOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        inner: Box<dyn PhysOperator + 'p>,
    ) -> Self {
        Self {
            input,
            inner,
            have_outer: false,
        }
    }
}

impl PhysOperator for NestOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.have_outer && self.inner.next(rs, cx)? {
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                self.have_outer = false;
                return Ok(false);
            }
            self.have_outer = true;
            self.inner.reset();
        }
    }

    fn reset(&mut self) {
        self.have_outer = false;
        self.input.reset();
        self.inner.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Emits nothing.
pub struct NoRowsOp;

impl PhysOperator for NoRowsOp {
    fn next(&mut self, _rs: &mut ResultSet, _cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        Ok(false)
    }

    fn reset(&mut self) {}

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Chunks of node ids into one output column.
struct IdSource {
    out: DataPos,
    ids: Option<Vec<NodeId>>,
    pos: usize,
}

impl IdSource {
    fn new(out: DataPos) -> Self {
        Self {
            out,
            ids: None,
            pos: 0,
        }
    }

    fn preset(out: DataPos, ids: Vec<NodeId>) -> Self {
        Self {
            out,
            ids: Some(ids),
            pos: 0,
        }
    }

    fn emit(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        let ids = self.ids.as_ref().unwrap();
        if self.pos >= ids.len() {
            return Ok(false);
        }
        let end = (self.pos + VECTOR_CAPACITY).min(ids.len());
        let chunk = ids[self.pos..end].to_vec();
        self.pos = end;
        let n = chunk.len();
        install(
            rs,
            self.out.group,
            n,
            vec![(self.out.col, Vector::nodes(chunk))],
        );
        cx.tick()?;
        Ok(true)
    }

    fn reset(&mut self) {
        self.ids = None;
        self.pos = 0;
    }
}

/// Every node, optionally with a label; a label-less one with a seek reads the label indexes
/// when they cover every node.
pub struct NodeScanOp<'p> {
    src: IdSource,
    label: Option<&'p str>,
    seek: Option<&'p NodeSeek>,
}

impl<'p> NodeScanOp<'p> {
    pub(crate) fn new(out: DataPos, label: Option<&'p str>, seek: Option<&'p NodeSeek>) -> Self {
        Self {
            src: IdSource::new(out),
            label,
            seek,
        }
    }

    /// A scan over a morsel of ids a worker was handed.
    pub(crate) fn morsel(out: DataPos, ids: Vec<NodeId>) -> Self {
        Self {
            src: IdSource::preset(out, ids),
            label: None,
            seek: None,
        }
    }
}

impl PhysOperator for NodeScanOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() {
            let sought = match self.seek {
                Some(seek) => seek_ids(seek, rs, cx)?,
                None => None,
            };
            self.src.ids = Some(sought.unwrap_or_else(|| scan_ids(self.label, cx)));
            self.src.pos = 0;
        }
        self.src.emit(rs, cx)
    }

    fn reset(&mut self) {
        self.src.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Keeps the ids that name a node this view sees.
fn keep_existing(ids: Vec<NodeId>, cx: &ExecCtx<'_>) -> Vec<NodeId> {
    let mut exist = Vec::with_capacity(ids.len());
    cx.view().nodes_exist(&ids, &mut exist);
    ids.into_iter()
        .zip(exist)
        .filter_map(|(id, ok)| ok.then_some(id))
        .collect()
}

/// Node lookup by internal id.
pub struct NodeByIdSeekOp<'p> {
    src: IdSource,
    label: Option<&'p str>,
    id: &'p CompiledExpr,
}

impl<'p> NodeByIdSeekOp<'p> {
    pub(crate) fn new(out: DataPos, label: Option<&'p str>, id: &'p CompiledExpr) -> Self {
        Self {
            src: IdSource::new(out),
            label,
            id,
        }
    }
}

impl PhysOperator for NodeByIdSeekOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() {
            let mut sc = EvalScratch::new();
            let v = self.id.eval_scalar(rs, cx.view(), &mut sc)?;
            let mut ids = Vec::new();
            if let CypherValue::Integer(n) = v
                && n >= 0
            {
                let id = NodeId(n as u64);
                if let Ok(node) = cx.view().get_node(id) {
                    let ok = match self.label {
                        Some(l) => match cx.view().strings().get(l) {
                            Some(raw) => node.labels.iter().any(|lid| lid.0 == raw),
                            None => false,
                        },
                        None => true,
                    };
                    if ok {
                        ids.push(id);
                    }
                }
            }
            self.src.ids = Some(ids);
            self.src.pos = 0;
        }
        self.src.emit(rs, cx)
    }

    fn reset(&mut self) {
        self.src.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Node ids in `[lower, upper)`.
pub struct NodeByIdRangeOp<'p> {
    src: IdSource,
    label: Option<&'p str>,
    lower: Option<&'p CompiledExpr>,
    upper: Option<&'p CompiledExpr>,
}

impl<'p> NodeByIdRangeOp<'p> {
    pub(crate) fn new(
        out: DataPos,
        label: Option<&'p str>,
        lower: Option<&'p CompiledExpr>,
        upper: Option<&'p CompiledExpr>,
    ) -> Self {
        Self {
            src: IdSource::new(out),
            label,
            lower,
            upper,
        }
    }
}

impl PhysOperator for NodeByIdRangeOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() {
            self.src.ids = Some(id_range_ids(self.label, self.lower, self.upper, cx)?);
            self.src.pos = 0;
        }
        self.src.emit(rs, cx)
    }

    fn reset(&mut self) {
        self.src.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Equality seek in a property index.
pub struct IndexSeekOp<'p> {
    src: IdSource,
    label: &'p str,
    property: &'p str,
    value: &'p CompiledExpr,
}

impl<'p> IndexSeekOp<'p> {
    pub(crate) fn new(
        out: DataPos,
        label: &'p str,
        property: &'p str,
        value: &'p CompiledExpr,
    ) -> Self {
        Self {
            src: IdSource::new(out),
            label,
            property,
            value,
        }
    }
}

impl PhysOperator for IndexSeekOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() {
            self.src.ids = Some(index_seek_ids(
                self.label,
                self.property,
                self.value,
                rs,
                cx,
            )?);
            self.src.pos = 0;
        }
        self.src.emit(rs, cx)
    }

    fn reset(&mut self) {
        self.src.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Hits above this share of the label are read with a label scan and a typed filter instead of
/// being enumerated from the index: a broad range through the b-tree lists most of the label in
/// index order and loses locality for nothing.
const INDEX_RANGE_MAX_SELECTIVITY: f64 = 0.25;

/// Range scan of a property index. At open it counts the range and, for a broad one, scans the
/// label and filters the property column instead.
pub struct IndexRangeOp<'p> {
    src: IdSource,
    /// The broad form: the label scanned through the typed filter, chunk by chunk.
    inner: Option<Box<dyn PhysOperator + 'p>>,
    label: &'p str,
    property: &'p str,
    op: CmpOp,
    value: &'p CompiledExpr,
}

impl<'p> IndexRangeOp<'p> {
    pub(crate) fn new(
        out: DataPos,
        label: &'p str,
        property: &'p str,
        op: CmpOp,
        value: &'p CompiledExpr,
    ) -> Self {
        Self {
            src: IdSource::new(out),
            inner: None,
            label,
            property,
            op,
            value,
        }
    }
}

/// Index bounds of a range comparison.
pub(crate) fn range_bounds(
    op: CmpOp,
    k: traverse_core::graph::property::IndexKey,
) -> (
    Bound<traverse_core::graph::property::IndexKey>,
    Bound<traverse_core::graph::property::IndexKey>,
) {
    match op {
        CmpOp::Gt => (Bound::Excluded(k), Bound::Unbounded),
        CmpOp::Gte => (Bound::Included(k), Bound::Unbounded),
        CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(k)),
        CmpOp::Lte => (Bound::Unbounded, Bound::Included(k)),
        CmpOp::Eq => (Bound::Included(k.clone()), Bound::Included(k)),
        CmpOp::Neq => (Bound::Unbounded, Bound::Unbounded),
    }
}

impl PhysOperator for IndexRangeOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() && self.inner.is_none() {
            match index_range_leaf(self.label, self.property, self.op, self.value, rs, cx)? {
                LeafIds::Nodes(ids) => {
                    self.src.ids = Some(ids);
                    self.src.pos = 0;
                }
                LeafIds::Bits { words, filter, .. } => {
                    let bits = words.len() * 64;
                    let scan = BitScanOp::new(self.src.out, words, 0, bits);
                    self.inner = Some(match filter {
                        Some(f) => Box::new(RangeFilterOp::new(Box::new(scan), self.src.out, f)),
                        None => Box::new(scan),
                    });
                }
                LeafIds::Edges(_) => unreachable!("a node range"),
            }
        }
        match &mut self.inner {
            Some(inner) => inner.next(rs, cx),
            None => self.src.emit(rs, cx),
        }
    }

    fn reset(&mut self) {
        self.src.reset();
        self.inner = None;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Label scan with the range predicate on a typed property column: the broad-range access path.
fn scan_label_with_range(
    cx: &ExecCtx<'_>,
    label: &str,
    property: &str,
    op: CmpOp,
    bound: &CypherValue,
) -> Result<Vec<NodeId>, CypherError> {
    let ids = cx.view().nodes_by_label(label);
    let Some(kid) = cx.key_id(property) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut b = VectorBuilder::new();
    for chunk in ids.chunks(VECTOR_CAPACITY) {
        b = VectorBuilder::new();
        cx.view().gather_node_props(chunk, &[kid], &mut [&mut b])?;
        let col = std::mem::take(&mut b).finish();
        let mask = super::super::expr::kernels::compare(
            op,
            &super::super::expr::Operand::Owned(col),
            &super::super::expr::Operand::Scalar(bound.clone()),
            chunk.len(),
        );
        if let Vector::Bool { vals, valid } = &mask {
            for (i, &id) in chunk.iter().enumerate() {
                if valid[i] && vals[i] {
                    out.push(id);
                }
            }
        }
        cx.check()?;
    }
    let _ = b;
    Ok(out)
}

/// Trigram candidates for a string predicate.
pub struct TextIndexScanOp<'p> {
    src: IdSource,
    label: &'p str,
    property: &'p str,
    kind: TextPredicateKind,
    pattern: &'p CompiledExpr,
}

impl<'p> TextIndexScanOp<'p> {
    pub(crate) fn new(
        out: DataPos,
        label: &'p str,
        property: &'p str,
        kind: TextPredicateKind,
        pattern: &'p CompiledExpr,
    ) -> Self {
        Self {
            src: IdSource::new(out),
            label,
            property,
            kind,
            pattern,
        }
    }
}

impl PhysOperator for TextIndexScanOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() {
            self.src.ids = Some(text_scan_ids(
                self.label,
                self.property,
                self.kind,
                self.pattern,
                rs,
                cx,
            )?);
            self.src.pos = 0;
        }
        self.src.emit(rs, cx)
    }

    fn reset(&mut self) {
        self.src.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// R-tree candidates for a spatial predicate.
pub struct PointIndexScanOp<'p> {
    src: IdSource,
    label: &'p str,
    property: &'p str,
    query: &'p PhysSpatialQuery,
}

impl<'p> PointIndexScanOp<'p> {
    pub(crate) fn new(
        out: DataPos,
        label: &'p str,
        property: &'p str,
        query: &'p PhysSpatialQuery,
    ) -> Self {
        Self {
            src: IdSource::new(out),
            label,
            property,
            query,
        }
    }
}

/// `([x, y, z], srid)` of a point map.
pub(crate) fn point_coords(v: &CypherValue) -> Option<([f64; 3], i64)> {
    let CypherValue::Map(m) = v else { return None };
    let num = |k: &str| -> Option<f64> {
        match m.get(k)? {
            CypherValue::Float(f) => Some(*f),
            CypherValue::Integer(i) => Some(*i as f64),
            _ => None,
        }
    };
    let x = num("x")?;
    let y = num("y")?;
    let z = num("z");
    let srid = match m.get("srid") {
        Some(CypherValue::Integer(s)) => *s,
        _ => {
            if z.is_some() {
                9157
            } else {
                7203
            }
        }
    };
    Some(([x, y, z.unwrap_or(0.0)], srid))
}

impl PhysOperator for PointIndexScanOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.src.ids.is_none() {
            self.src.ids = Some(point_scan_ids(
                self.label,
                self.property,
                self.query,
                rs,
                cx,
            )?);
            self.src.pos = 0;
        }
        self.src.emit(rs, cx)
    }

    fn reset(&mut self) {
        self.src.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Every edge of the given types, with both endpoints.
pub struct EdgeTypeScanOp<'p> {
    from: DataPos,
    rel: Option<DataPos>,
    to: DataPos,
    edge_types: &'p [String],
    direction: Direction,
    ids: Option<Vec<EdgeId>>,
    pos: usize,
    /// Undirected: each edge goes out twice; `phase` 1 is the swapped copy of the chunk.
    phase: u8,
}

impl<'p> EdgeTypeScanOp<'p> {
    pub(crate) fn new(
        from: DataPos,
        rel: Option<DataPos>,
        to: DataPos,
        edge_types: &'p [String],
        direction: Direction,
    ) -> Self {
        Self {
            from,
            rel,
            to,
            edge_types,
            direction,
            ids: None,
            pos: 0,
            phase: 0,
        }
    }

    /// A scan over a morsel of edge ids a worker was handed.
    pub(crate) fn morsel(
        from: DataPos,
        rel: Option<DataPos>,
        to: DataPos,
        direction: Direction,
        ids: Vec<EdgeId>,
    ) -> Self {
        Self {
            from,
            rel,
            to,
            edge_types: &[],
            direction,
            ids: Some(ids),
            pos: 0,
            phase: 0,
        }
    }
}

impl PhysOperator for EdgeTypeScanOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.ids.is_none() {
            self.ids = Some(edge_scan_ids(self.edge_types, cx));
            self.pos = 0;
            self.phase = 0;
        }
        let ids = self.ids.as_ref().unwrap();
        loop {
            if self.pos >= ids.len() {
                return Ok(false);
            }
            let end = (self.pos + VECTOR_CAPACITY).min(ids.len());
            let chunk = &ids[self.pos..end];
            let swapped = self.direction == Direction::Both && self.phase == 1;
            let mut from_b = VectorBuilder::new();
            let mut rel_b = VectorBuilder::new();
            let mut to_b = VectorBuilder::new();
            let mut n = 0usize;
            for &eid in chunk {
                let Ok(edge) = cx.view().get_edge(eid) else {
                    continue;
                };
                let (f, t) = match self.direction {
                    Direction::Outgoing => (edge.source, edge.target),
                    Direction::Incoming => (edge.target, edge.source),
                    Direction::Both => {
                        if swapped {
                            (edge.target, edge.source)
                        } else {
                            (edge.source, edge.target)
                        }
                    }
                };
                from_b.push_node(f);
                rel_b.push_edge(eid, edge.edge_type, edge.source, edge.target);
                to_b.push_node(t);
                n += 1;
            }
            // advance: the undirected scan replays the chunk swapped before moving on
            if self.direction == Direction::Both && self.phase == 0 {
                self.phase = 1;
            } else {
                self.phase = 0;
                self.pos = end;
            }
            if n == 0 {
                continue;
            }
            let mut cols = vec![
                (self.from.col, from_b.finish()),
                (self.to.col, to_b.finish()),
            ];
            if let Some(r) = self.rel {
                cols.push((r.col, rel_b.finish()));
            }
            install(rs, self.from.group, n, cols);
            cx.tick()?;
            return Ok(true);
        }
    }

    fn reset(&mut self) {
        self.ids = None;
        self.pos = 0;
        self.phase = 0;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// A count straight from the storage structure.
pub struct CountSourceOp<'p> {
    kind: &'p CountKind,
    outs: &'p [DataPos],
    out_group: GroupId,
    done: bool,
}

impl<'p> CountSourceOp<'p> {
    pub(crate) fn new(kind: &'p CountKind, outs: &'p [DataPos], out_group: GroupId) -> Self {
        Self {
            kind,
            outs,
            out_group,
            done: false,
        }
    }
}

impl PhysOperator for CountSourceOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.done {
            return Ok(false);
        }
        self.done = true;
        let n: usize = match self.kind {
            CountKind::Nodes { label } => match label {
                Some(l) => cx.view().count_nodes_by_label(l),
                None => cx.view().node_count(),
            },
            CountKind::Edges {
                edge_types,
                direction,
                from_label,
                to_label,
            } => {
                // a label every node has filters nothing: the type counts answer directly. An
                // undirected pattern counts a self-loop once and every other edge twice, so it
                // goes through the adjacency
                let graph = cx.view();
                let total = graph.node_count();
                let covers = |l: &Option<String>| {
                    l.as_ref()
                        .is_none_or(|l| graph.count_nodes_by_label(l) == total)
                };
                if *direction != Direction::Both && covers(from_label) && covers(to_label) {
                    if edge_types.is_empty() {
                        graph.edge_count()
                    } else {
                        edge_types
                            .iter()
                            .map(|t| graph.count_edges_by_type(t))
                            .sum()
                    }
                } else {
                    count_labelled_edges(
                        cx,
                        edge_types,
                        *direction,
                        from_label.as_deref(),
                        to_label.as_deref(),
                    )?
                }
            }
            CountKind::IndexRange {
                label,
                property,
                op,
                value,
            } => {
                let v = eval_constant(value, cx.view())?;
                match index_key(&v) {
                    Some(k) => {
                        let (lower, upper) = range_bounds(*op, k);
                        cx.view()
                            .count_index_range(label, property, lower, upper)
                            .unwrap_or(0)
                    }
                    None => 0,
                }
            }
            CountKind::IndexEq {
                label,
                property,
                value,
            } => {
                let v = eval_constant(value, cx.view())?;
                match index_key(&v) {
                    Some(k) => cx
                        .view()
                        .count_index_lookup(label, property, &k)
                        .unwrap_or(0),
                    None => 0,
                }
            }
            CountKind::DegreeSum {
                label,
                property,
                op,
                value,
                dir,
                edge_types,
                to_label,
            } => {
                let v = eval_constant(value, cx.view())?;
                let (cmp, bound) = column_compare(*op, &v);
                let sources =
                    match bound.and_then(|b| cx.view().nodes_where(label, property, cmp, &b)) {
                        Some(ids) => ids,
                        None => scan_label_with_range(cx, label, property, *op, &v)?,
                    };
                count_edges_from(cx, sources, edge_types, *dir, to_label.as_deref())?
            }
            CountKind::ColumnRange {
                label,
                property,
                op,
                value,
            } => {
                let v = eval_constant(value, cx.view())?;
                let (cmp, bound) = column_compare(*op, &v);
                match bound.and_then(|b| cx.view().count_nodes_where(label, property, cmp, &b)) {
                    Some(n) => n,
                    None => scan_label_with_range(cx, label, property, *op, &v)?.len(),
                }
            }
        };
        let cols: Vec<(u16, Vector)> = self
            .outs
            .iter()
            .map(|p| (p.col, Vector::ints(vec![n as i64])))
            .collect();
        install(rs, self.out_group, 1, cols);
        Ok(true)
    }

    fn reset(&mut self) {
        self.done = false;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Edges of the given types out of the nodes labelled `from_label` (all nodes without one) whose
/// far end has `to_label` (any node without one), counted over the adjacency without
/// materializing rows; in parallel over a `Sync` view.
fn count_labelled_edges(
    cx: &ExecCtx<'_>,
    edge_types: &[String],
    direction: Direction,
    from_label: Option<&str>,
    to_label: Option<&str>,
) -> Result<usize, CypherError> {
    let graph = cx.view();
    let sources: Vec<NodeId> = match from_label {
        Some(l) => graph.nodes_by_label(l),
        None => graph.nodes_vec(),
    };
    count_edges_from(cx, sources, edge_types, direction, to_label)
}

/// The storage comparison for `op` against `v` (`None` bound: no scalar to compare in storage).
fn column_compare(op: CmpOp, v: &CypherValue) -> (ColumnCmp, Option<ColumnBound>) {
    let bound = match v {
        CypherValue::Integer(i) => Some(ColumnBound::Int(*i)),
        CypherValue::Float(f) => Some(ColumnBound::Float(*f)),
        CypherValue::String(s) => Some(ColumnBound::Str(s.clone())),
        _ => None,
    };
    let cmp = match op {
        CmpOp::Lt => ColumnCmp::Lt,
        CmpOp::Lte => ColumnCmp::Lte,
        CmpOp::Gt => ColumnCmp::Gt,
        CmpOp::Gte => ColumnCmp::Gte,
        CmpOp::Eq => ColumnCmp::Eq,
        CmpOp::Neq => ColumnCmp::Neq,
    };
    (cmp, bound)
}

/// Edges of the given types out of `sources` in `direction` whose far end has `to_label` (any
/// node without one): a degree sum when nothing on the far end needs checking and the graph has
/// no other edge type, else counted over the adjacency without materializing rows; in parallel
/// over a `Sync` view.
fn count_edges_from(
    cx: &ExecCtx<'_>,
    sources: Vec<NodeId>,
    edge_types: &[String],
    direction: Direction,
    to_label: Option<&str>,
) -> Result<usize, CypherError> {
    let graph = cx.view();
    let covers = to_label.is_none_or(|l| graph.count_nodes_by_label(l) == graph.node_count());
    let typed_ok = edge_types.is_empty()
        || (edge_types.len() == 1 && graph.relationship_type_names() == edge_types);
    if covers && typed_ok && direction != Direction::Both {
        let degree_sum = |g: &dyn traverse_core::tx::view::GraphView, chunk: &[NodeId]| {
            let mut d = Vec::with_capacity(chunk.len());
            g.degrees(chunk, direction, &mut d);
            d.iter().sum::<usize>()
        };
        return Ok(match graph.as_sync_ref() {
            Some(sync) if sources.len() >= 10_000 && rayon::current_num_threads() > 1 => {
                use rayon::prelude::*;
                let query = crate::query::current();
                sources
                    .par_chunks(4096)
                    .map(|chunk| {
                        let _q = crate::query::enter_opt(query.as_ref());
                        crate::query::check_interrupt()?;
                        Ok::<usize, CypherError>(degree_sum(sync, chunk))
                    })
                    .try_reduce(|| 0usize, |a, b| Ok(a + b))?
            }
            _ => {
                let mut n = 0usize;
                for chunk in sources.chunks(4096) {
                    cx.check()?;
                    n += degree_sum(graph, chunk);
                }
                n
            }
        });
    }
    let tf = super::expand::type_filter(graph, edge_types);
    let to_id = match to_label {
        Some(l) => match graph.strings().get(l) {
            Some(raw) => Some(traverse_core::graph::types::LabelId(raw)),
            None => return Ok(0),
        },
        None => None,
    };
    let count_chunk = |g: &dyn traverse_core::tx::view::GraphView,
                       chunk: &[NodeId]|
     -> Result<usize, CypherError> {
        let mut batch = traverse_core::graph::batch::AdjBatch::new();
        g.expand_batch(chunk, direction, &tf, &mut batch)?;
        Ok(match to_id {
            None => batch.len(),
            Some(lid) => {
                let mut has = Vec::with_capacity(batch.len());
                g.nodes_have_label(&batch.neighbor, lid, &mut has);
                has.iter().filter(|h| **h).count()
            }
        })
    };
    const CHUNK: usize = 4096;
    match graph.as_sync_ref() {
        Some(sync) if sources.len() >= 10_000 && rayon::current_num_threads() > 1 => {
            use rayon::prelude::*;
            let query = crate::query::current();
            let counts: Result<Vec<usize>, CypherError> = sources
                .par_chunks(CHUNK)
                .map(|chunk| {
                    let _q = crate::query::enter_opt(query.as_ref());
                    crate::query::check_interrupt()?;
                    count_chunk(sync, chunk)
                })
                .collect();
            Ok(counts?.into_iter().sum())
        }
        _ => {
            let mut n = 0usize;
            for chunk in sources.chunks(CHUNK) {
                cx.check()?;
                n += count_chunk(graph, chunk)?;
            }
            Ok(n)
        }
    }
}

/// The ids a leaf source enumerates: the scheduler lists them once and hands the workers
/// morsels of them. A label's members come as a bitmap over node ids, so nothing is listed.
pub(crate) enum LeafIds {
    Nodes(Vec<NodeId>),
    Edges(Vec<EdgeId>),
    /// Bit `i` set: node `i` is in the set; `count` bits are set. With `filter`, only the
    /// nodes whose property passes it (a broad index range read as a filtered label scan).
    Bits {
        words: std::sync::Arc<Vec<u64>>,
        count: usize,
        filter: Option<RangeFilter>,
    },
}

/// A comparison on one property, applied to scanned nodes with the typed column.
#[derive(Clone)]
pub(crate) struct RangeFilter {
    kid: traverse_core::graph::types::PropertyKeyId,
    op: CmpOp,
    bound: CypherValue,
}

/// Keeps the lanes of a scanned node group whose property passes a comparison, gathered from
/// the typed column a chunk at a time.
pub struct RangeFilterOp<'p> {
    inner: Box<dyn PhysOperator + 'p>,
    out: DataPos,
    filter: RangeFilter,
}

impl<'p> RangeFilterOp<'p> {
    pub(crate) fn new(
        inner: Box<dyn PhysOperator + 'p>,
        out: DataPos,
        filter: RangeFilter,
    ) -> Self {
        Self { inner, out, filter }
    }
}

impl PhysOperator for RangeFilterOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if !self.inner.next(rs, cx)? {
                return Ok(false);
            }
            let g = rs.group(self.out.group);
            let n = g.len;
            let ids: Vec<NodeId> = match &g.cols[self.out.col as usize] {
                Vector::Node { ids, .. } => ids.clone(),
                other => (0..n)
                    .map(|i| other.node_id(i).unwrap_or(NodeId(u64::MAX)))
                    .collect(),
            };
            let mut b = VectorBuilder::with_capacity(n);
            cx.view()
                .gather_node_props(&ids, &[self.filter.kid], &mut [&mut b])?;
            let mask = super::super::expr::kernels::compare(
                self.filter.op,
                &super::super::expr::Operand::Owned(b.finish()),
                &super::super::expr::Operand::Scalar(self.filter.bound.clone()),
                n,
            );
            let keep: Vec<u32> = match &mask {
                Vector::Bool { vals, valid } => (0..n)
                    .filter(|&i| valid[i] && vals[i])
                    .map(|i| i as u32)
                    .collect(),
                _ => Vec::new(),
            };
            if keep.is_empty() {
                continue;
            }
            if keep.len() < n {
                rs.group_mut(self.out.group).narrow(keep);
            }
            return Ok(true);
        }
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

impl LeafIds {
    pub(crate) fn len(&self) -> usize {
        match self {
            LeafIds::Nodes(v) => v.len(),
            LeafIds::Edges(v) => v.len(),
            LeafIds::Bits { count, .. } => *count,
        }
    }
}

/// Scans the set bits of a bitmap in `[from, to)` as node ids, a chunk at a time.
pub struct BitScanOp {
    out: DataPos,
    words: std::sync::Arc<Vec<u64>>,
    from: usize,
    to: usize,
    cur: usize,
}

impl BitScanOp {
    pub(crate) fn new(
        out: DataPos,
        words: std::sync::Arc<Vec<u64>>,
        from: usize,
        to: usize,
    ) -> Self {
        Self {
            out,
            words,
            from,
            to,
            cur: from,
        }
    }
}

impl PhysOperator for BitScanOp {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        let mut ids: Vec<NodeId> = Vec::with_capacity(VECTOR_CAPACITY);
        let words = &self.words;
        while self.cur < self.to && ids.len() < VECTOR_CAPACITY {
            let w = self.cur / 64;
            let Some(&word) = words.get(w) else {
                self.cur = self.to;
                break;
            };
            // the bits of this word from `cur` on, clipped to `to`
            let mut bits = word >> (self.cur % 64) << (self.cur % 64);
            let word_end = (w + 1) * 64;
            if self.to < word_end {
                bits &= (1u64 << (self.to % 64)) - 1;
            }
            if bits == u64::MAX && ids.len() + 64 <= VECTOR_CAPACITY {
                // a full word: 64 consecutive ids
                ids.extend((w * 64..w * 64 + 64).map(|i| NodeId(i as u64)));
                bits = 0;
            }
            while bits != 0 && ids.len() < VECTOR_CAPACITY {
                let b = bits.trailing_zeros() as usize;
                ids.push(NodeId((w * 64 + b) as u64));
                bits &= bits - 1;
            }
            if bits == 0 {
                self.cur = word_end.min(self.to);
            } else {
                // the chunk filled mid-word: resume at the next set bit
                self.cur = w * 64 + bits.trailing_zeros() as usize;
            }
        }
        if ids.is_empty() {
            return Ok(false);
        }
        let n = ids.len();
        install(
            rs,
            self.out.group,
            n,
            vec![(self.out.col, Vector::nodes(ids))],
        );
        cx.tick()?;
        Ok(true)
    }

    fn reset(&mut self) {
        self.cur = self.from;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// The ids `node` (an id-enumerating leaf) would scan.
pub(crate) fn leaf_ids(
    node: &PhysNode,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
) -> Result<LeafIds, CypherError> {
    Ok(match node {
        PhysNode::NodeScan {
            seek: Some(seek), ..
        } if let Some(ids) = seek_ids(seek, rs, cx)? => LeafIds::Nodes(ids),
        PhysNode::NodeScan { label, .. } => {
            match label.as_deref().and_then(|l| cx.view().label_bitmap(l)) {
                Some(words) => {
                    let count = words.iter().map(|w| w.count_ones() as usize).sum();
                    LeafIds::Bits {
                        words: std::sync::Arc::new(words),
                        count,
                        filter: None,
                    }
                }
                None => LeafIds::Nodes(scan_ids(label.as_deref(), cx)),
            }
        }
        PhysNode::NodeByIdRange {
            label,
            lower,
            upper,
            ..
        } => LeafIds::Nodes(id_range_ids(
            label.as_deref(),
            lower.as_ref(),
            upper.as_ref(),
            cx,
        )?),
        PhysNode::IndexSeek {
            label,
            property,
            value,
            ..
        } => LeafIds::Nodes(index_seek_ids(label, property, value, rs, cx)?),
        PhysNode::IndexRange {
            label,
            property,
            op,
            value,
            ..
        } => index_range_leaf(label, property, *op, value, rs, cx)?,
        PhysNode::TextIndexScan {
            label,
            property,
            kind,
            pattern,
            ..
        } => LeafIds::Nodes(text_scan_ids(label, property, *kind, pattern, rs, cx)?),
        PhysNode::PointIndexScan {
            label,
            property,
            query,
            ..
        } => LeafIds::Nodes(point_scan_ids(label, property, query, rs, cx)?),
        PhysNode::EdgeTypeScan { edge_types, .. } => LeafIds::Edges(edge_scan_ids(edge_types, cx)),
        _ => unreachable!("not an id-enumerating leaf"),
    })
}

/// The operator standing in for leaf `node` on a worker: a scan over `ids[range]`.
pub(crate) fn leaf_stand_in<'p>(
    node: &'p PhysNode,
    ids: &LeafIds,
    range: std::ops::Range<usize>,
) -> Box<dyn PhysOperator + 'p> {
    match (node, ids) {
        (
            PhysNode::NodeScan { out, .. }
            | PhysNode::NodeByIdRange { out, .. }
            | PhysNode::IndexSeek { out, .. }
            | PhysNode::IndexRange { out, .. }
            | PhysNode::TextIndexScan { out, .. }
            | PhysNode::PointIndexScan { out, .. },
            LeafIds::Nodes(v),
        ) => Box::new(NodeScanOp::morsel(*out, v[range].to_vec())),
        (
            PhysNode::NodeScan { out, .. },
            LeafIds::Bits {
                words,
                filter: None,
                ..
            },
        ) => Box::new(BitScanOp::new(*out, words.clone(), range.start, range.end)),
        (
            PhysNode::IndexRange { out, .. },
            LeafIds::Bits {
                words,
                filter: Some(f),
                ..
            },
        ) => Box::new(RangeFilterOp::new(
            Box::new(BitScanOp::new(*out, words.clone(), range.start, range.end)),
            *out,
            f.clone(),
        )),
        (
            PhysNode::EdgeTypeScan {
                from,
                rel,
                to,
                direction,
                ..
            },
            LeafIds::Edges(v),
        ) => Box::new(EdgeTypeScanOp::morsel(
            *from,
            *rel,
            *to,
            *direction,
            v[range].to_vec(),
        )),
        _ => unreachable!("a leaf and its ids match"),
    }
}

/// The nodes whose property is the seek's key, from the label indexes; `None` when they don't
/// cover every node, or the key isn't one an index holds: then the scan reads every node and
/// the filter above it decides.
fn seek_ids(
    seek: &NodeSeek,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
) -> Result<Option<Vec<NodeId>>, CypherError> {
    let mut sc = EvalScratch::new();
    let v = seek.value.eval_scalar(rs, cx.view(), &mut sc)?;
    Ok(index_key(&v).and_then(|k| cx.view().property_seek(&seek.property, &k)))
}

/// Ids of a scan: a label's members, or every node.
fn scan_ids(label: Option<&str>, cx: &ExecCtx<'_>) -> Vec<NodeId> {
    match label {
        Some(l) => cx.view().nodes_by_label(l),
        None => cx.view().nodes_vec(),
    }
}

/// Ids in `[lower, upper)` that name a node, with `label` when given.
fn id_range_ids(
    label: Option<&str>,
    lower: Option<&CompiledExpr>,
    upper: Option<&CompiledExpr>,
    cx: &ExecCtx<'_>,
) -> Result<Vec<NodeId>, CypherError> {
    let node_count = cx.view().highest_node_id();
    let to_id = |v: CypherValue| match v {
        CypherValue::Integer(n) if n >= 0 => Some(n as u64),
        CypherValue::Integer(_) => Some(0),
        _ => None,
    };
    let lower = match lower {
        Some(e) => to_id(eval_constant(e, cx.view())?).unwrap_or(0),
        None => 0,
    };
    let upper = match upper {
        Some(e) => to_id(eval_constant(e, cx.view())?).unwrap_or(node_count),
        None => node_count,
    };
    let end = upper.min(node_count);
    let label_id = match label {
        Some(l) => match cx.view().strings().get(l) {
            Some(raw) => Some(traverse_core::graph::types::LabelId(raw)),
            None => return Ok(Vec::new()),
        },
        None => None,
    };
    let mut out = Vec::new();
    let mut cur = lower;
    while cur < end {
        let span_end = (cur + VECTOR_CAPACITY as u64).min(end);
        let candidates: Vec<NodeId> = (cur..span_end).map(NodeId).collect();
        cur = span_end;
        let mut ids = keep_existing(candidates, cx);
        if let Some(want) = label_id {
            let mut has = Vec::with_capacity(ids.len());
            cx.view().nodes_have_label(&ids, want, &mut has);
            ids = ids
                .into_iter()
                .zip(has)
                .filter_map(|(id, ok)| ok.then_some(id))
                .collect();
        }
        out.extend(ids);
        cx.check()?;
    }
    Ok(out)
}

/// Ids an equality seek in a property index finds.
fn index_seek_ids(
    label: &str,
    property: &str,
    value: &CompiledExpr,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
) -> Result<Vec<NodeId>, CypherError> {
    let mut sc = EvalScratch::new();
    let v = value.eval_scalar(rs, cx.view(), &mut sc)?;
    let ids = match index_key(&v) {
        Some(k) => cx
            .view()
            .index_lookup(label, property, &k)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    Ok(keep_existing(ids, cx))
}

/// What a range scan of a property index reads: the ids enumerated from the index, or, for a
/// broad range, the label's bitmap through a typed filter (listed ids when the label has no
/// bitmap).
fn index_range_leaf(
    label: &str,
    property: &str,
    op: CmpOp,
    value: &CompiledExpr,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
) -> Result<LeafIds, CypherError> {
    let mut sc = EvalScratch::new();
    let v = value.eval_scalar(rs, cx.view(), &mut sc)?;
    let Some(k) = index_key(&v) else {
        return Ok(LeafIds::Nodes(Vec::new()));
    };
    let (lower, upper) = range_bounds(op, k.clone());
    let label_count = cx.view().count_nodes_by_label(label);
    let broad = match cx
        .view()
        .index_range_estimate(label, property, &lower, &upper)
    {
        Some(f) => {
            f > INDEX_RANGE_MAX_SELECTIVITY && f * label_count as f64 > VECTOR_CAPACITY as f64
        }
        None => {
            let hits = cx
                .view()
                .count_index_range(label, property, lower.clone(), upper.clone());
            matches!(hits, Some(h) if label_count > 0
                && h as f64 > label_count as f64 * INDEX_RANGE_MAX_SELECTIVITY
                && h > VECTOR_CAPACITY)
        }
    };
    if !broad {
        let ids = cx
            .view()
            .index_range_lookup(label, property, lower, upper)
            .unwrap_or_default();
        return Ok(LeafIds::Nodes(keep_existing(ids, cx)));
    }
    if let (Some(words), Some(kid)) = (cx.view().label_bitmap(label), cx.key_id(property)) {
        let count = words.iter().map(|w| w.count_ones() as usize).sum();
        return Ok(LeafIds::Bits {
            words: std::sync::Arc::new(words),
            count,
            filter: Some(RangeFilter { kid, op, bound: v }),
        });
    }
    let ids = scan_label_with_range(cx, label, property, op, &v)?;
    Ok(LeafIds::Nodes(keep_existing(ids, cx)))
}

/// Trigram candidates for a string predicate (the whole label when the index can't answer).
fn text_scan_ids(
    label: &str,
    property: &str,
    kind: TextPredicateKind,
    pattern: &CompiledExpr,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
) -> Result<Vec<NodeId>, CypherError> {
    let mut sc = EvalScratch::new();
    let v = pattern.eval_scalar(rs, cx.view(), &mut sc)?;
    let ids = match v {
        CypherValue::String(s) => {
            let kind = match kind {
                TextPredicateKind::Equals => traverse_core::graph::text::TextQueryKind::Equals,
                TextPredicateKind::StartsWith => {
                    traverse_core::graph::text::TextQueryKind::StartsWith
                }
                TextPredicateKind::EndsWith => traverse_core::graph::text::TextQueryKind::EndsWith,
                TextPredicateKind::Contains => traverse_core::graph::text::TextQueryKind::Contains,
            };
            cx.view()
                .text_index_candidates(label, property, kind, &s)
                .unwrap_or_else(|| cx.view().nodes_by_label(label))
        }
        _ => cx.view().nodes_by_label(label),
    };
    Ok(keep_existing(ids, cx))
}

/// R-tree candidates for a spatial predicate.
fn point_scan_ids(
    label: &str,
    property: &str,
    query: &PhysSpatialQuery,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
) -> Result<Vec<NodeId>, CypherError> {
    let mut sc = EvalScratch::new();
    fn number<'p>(
        e: &'p CompiledExpr,
        sc: &mut EvalScratch<'p>,
        rs: &ResultSet,
        graph: &dyn traverse_core::tx::view::GraphView,
    ) -> Result<Option<f64>, CypherError> {
        Ok(match e.eval_scalar(rs, graph, sc)? {
            CypherValue::Integer(i) => Some(i as f64),
            CypherValue::Float(f) => Some(f),
            _ => None,
        })
    }
    let ids: Vec<NodeId> = match query {
        PhysSpatialQuery::Distance { center, radius } => {
            let c = center.eval_scalar(rs, cx.view(), &mut sc)?;
            match (point_coords(&c), number(radius, &mut sc, rs, cx.view())?) {
                (Some((c, srid)), Some(r)) => cx
                    .view()
                    .point_index_within_distance(label, property, c, r, srid)
                    .map(|v| v.into_iter().map(|(n, _)| n).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        PhysSpatialQuery::BBox { ll, ur } => {
            let a = ll.eval_scalar(rs, cx.view(), &mut sc)?;
            let b = ur.eval_scalar(rs, cx.view(), &mut sc)?;
            match (point_coords(&a), point_coords(&b)) {
                (Some((ll_c, ll_srid)), Some((ur_c, ur_srid))) if ll_srid == ur_srid => cx
                    .view()
                    .point_index_within_bbox(label, property, ll_c, ur_c, ll_srid)
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        PhysSpatialQuery::Knn { center, k } => {
            let c = center.eval_scalar(rs, cx.view(), &mut sc)?;
            match (point_coords(&c), number(k, &mut sc, rs, cx.view())?) {
                (Some((c, srid)), Some(k)) if k >= 0.0 => cx
                    .view()
                    .point_index_knn(label, property, c, k as usize, srid)
                    .map(|v| v.into_iter().map(|(n, _)| n).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
    };
    Ok(keep_existing(ids, cx))
}

/// Every edge of the given types.
fn edge_scan_ids(edge_types: &[String], cx: &ExecCtx<'_>) -> Vec<EdgeId> {
    let mut ids = Vec::new();
    for t in edge_types {
        ids.extend(cx.view().edges_by_type(t));
    }
    ids
}
