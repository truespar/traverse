//! Data-source operators: Scan, IndexScan, IndexRangeScan, EmptyRow, ScanFilterProject, ....

use std::ops::Bound;

use traverse_core::graph::types::NodeId;
use traverse_core::tx::view::GraphView;

use super::Operator;
use crate::ast::{CmpOp, Expr};
use crate::error::CypherError;
use crate::executor::{cypher_value_to_index_key, eval_expr, exec_scan_filter_project};
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};
use crate::planner::{LogicalPlan, SpatialQuery, TextPredicateKind};
use crate::types::CypherValue;

// -- EmptyRowOp ---------------------------------------------------------

pub(crate) struct EmptyRowOp {
    emitted: bool,
}

impl EmptyRowOp {
    pub(crate) fn new() -> Self {
        Self { emitted: false }
    }
}

impl Operator for EmptyRowOp {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.emitted {
            Ok(None)
        } else {
            self.emitted = true;
            Ok(Some(new_frame(symtab)))
        }
    }
}

// -- SeededRowsOp -------------------------------------------------------

/// Runtime-only leaf replaying pre-built frames. `ForEachOp` uses it to feed the body's writes one
/// seed row per FOREACH iteration (outer frame + loop variable): write-capable seeding that the
/// read-only `exec_via_operators_seeded` / `exec_subquery` can't do.
pub(crate) struct SeededRowsOp<'p> {
    frames: &'p [Frame],
    pos: usize,
}

impl<'p> SeededRowsOp<'p> {
    pub(crate) fn new(frames: &'p [Frame]) -> Self {
        Self { frames, pos: 0 }
    }
}

impl Operator for SeededRowsOp<'_> {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.pos < self.frames.len() {
            let f = self.frames[self.pos].clone();
            self.pos += 1;
            Ok(Some(f))
        } else {
            Ok(None)
        }
    }
}

// -- ScanOp -------------------------------------------------------------

pub(crate) struct ScanOp<'p> {
    pub(crate) variable: &'p str,
    pub(crate) label: Option<&'p str>,
    node_ids: Option<Vec<NodeId>>,
    pos: usize,
}

impl<'p> ScanOp<'p> {
    pub(crate) fn new(variable: &'p str, label: Option<&'p str>) -> Self {
        Self {
            variable,
            label,
            node_ids: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for ScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.node_ids.is_none() {
            let ids = if let Some(morsel) = super::pipeline_par::take_morsel_ids() {
                morsel
            } else {
                match self.label {
                    Some(lbl) => graph.nodes_by_label(lbl),
                    None => graph.nodes_vec(),
                }
            };
            self.node_ids = Some(ids);
        }
        let ids = self.node_ids.as_ref().unwrap();
        if self.pos < ids.len() {
            let nid = ids[self.pos];
            self.pos += 1;
            let slot = symtab.slot(self.variable).unwrap();
            let mut frame = new_frame(symtab);
            frame[slot] = SlotValue::NodeRef(nid);
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- EdgeTypeScanOp -----------------------------------------------------

/// Every edge of the given types from the edge-type index, binding `(from, rel, to)` per edge;
/// replaces `Scan(no label) + ExpandOut` for anchorless queries. UNDIRECTED (Direction::Both)
/// patterns emit each edge twice, as is and swapped, like `ExpandBoth`.
pub(crate) struct EdgeTypeScanOp<'p> {
    pub(crate) from_var: &'p str,
    pub(crate) rel_var: &'p str,
    pub(crate) to_var: &'p str,
    pub(crate) edge_types: &'p [String],
    pub(crate) direction: traverse_core::graph::types::Direction,
    edge_ids: Option<Vec<traverse_core::graph::types::EdgeId>>,
    pos: usize,
    /// UNDIRECTED: two frames per edge, `0 = forward`, `1 = swapped`; `pos` advances after the
    /// second.
    phase: u8,
}

impl<'p> EdgeTypeScanOp<'p> {
    pub(crate) fn new(
        from_var: &'p str,
        rel_var: &'p str,
        to_var: &'p str,
        edge_types: &'p [String],
        direction: traverse_core::graph::types::Direction,
    ) -> Self {
        Self {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
            edge_ids: None,
            pos: 0,
            phase: 0,
        }
    }
}

impl<'p> Operator for EdgeTypeScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        use traverse_core::graph::types::Direction;

        if self.edge_ids.is_none() {
            let mut ids = Vec::new();
            for ty in self.edge_types {
                ids.extend(graph.edges_by_type(ty));
            }
            self.edge_ids = Some(ids);
        }
        let ids = self.edge_ids.as_ref().unwrap();
        loop {
            if self.pos >= ids.len() {
                return Ok(None);
            }
            let eid = ids[self.pos];
            let edge = match graph.get_edge(eid) {
                Ok(e) => e,
                Err(_) => {
                    self.pos += 1;
                    self.phase = 0;
                    continue;
                }
            };

            // (from_id, to_id) for this emission
            let (from_id, to_id) = match self.direction {
                Direction::Outgoing => (edge.source, edge.target),
                Direction::Incoming => (edge.target, edge.source),
                Direction::Both => {
                    if self.phase == 0 {
                        (edge.source, edge.target)
                    } else {
                        (edge.target, edge.source)
                    }
                }
            };

            // advance counters before emitting so the next call resumes correctly
            match self.direction {
                Direction::Both if self.phase == 0 => {
                    self.phase = 1;
                }
                _ => {
                    self.pos += 1;
                    self.phase = 0;
                }
            }

            let mut frame = new_frame(symtab);
            if let Some(slot) = symtab.slot(self.from_var) {
                frame[slot] = SlotValue::NodeRef(from_id);
            }
            if !self.rel_var.is_empty()
                && let Some(slot) = symtab.slot(self.rel_var)
            {
                frame[slot] = SlotValue::EdgeRef {
                    id: eid,
                    edge_type_id: edge.edge_type,
                    source: edge.source,
                    target: edge.target,
                };
            }
            if let Some(slot) = symtab.slot(self.to_var) {
                frame[slot] = SlotValue::NodeRef(to_id);
            }
            return Ok(Some(frame));
        }
    }
}

/// Bound expression -> u64 node id. Negative integers clamp to `0`; non-integer or missing -> `None`
/// (caller substitutes a default).
fn eval_to_id(
    e: &Expr,
    frame: &Frame,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Option<u64>, CypherError> {
    let v = eval_expr(e, frame, graph, symtab)?;
    Ok(match v {
        crate::types::CypherValue::Integer(n) if n >= 0 => Some(n as u64),
        crate::types::CypherValue::Integer(_) => Some(0),
        _ => None,
    })
}

// -- NodeByIdRangeOp ----------------------------------------------------

/// Node ids in `[lower, upper)` with an optional label check, from `Scan + Filter(id(var) <cmp>
/// lit)`. `WHERE id(n) < 100` visits 100 ids instead of the whole label.
pub(crate) struct NodeByIdRangeOp<'p> {
    pub(crate) variable: &'p str,
    pub(crate) label: Option<&'p str>,
    pub(crate) lower_expr: Option<&'p Expr>,
    pub(crate) upper_expr: Option<&'p Expr>,
    lower: u64,
    upper: u64,
    cur: u64,
    label_id: Option<u32>,
    init: bool,
}

impl<'p> NodeByIdRangeOp<'p> {
    pub(crate) fn new(
        variable: &'p str,
        label: Option<&'p str>,
        lower_expr: Option<&'p Expr>,
        upper_expr: Option<&'p Expr>,
    ) -> Self {
        Self {
            variable,
            label,
            lower_expr,
            upper_expr,
            lower: 0,
            upper: 0,
            cur: 0,
            label_id: None,
            init: false,
        }
    }
}

impl<'p> Operator for NodeByIdRangeOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        // first call: resolve the bounds (may be parameters known only at execute time; staying
        // lazy keeps the plan cacheable across `$__sp0` values), resolve the label, clamp the upper
        // bound to the node count (else `WHERE id(n) >= N` would iterate to u64::MAX)
        if !self.init {
            let node_count = graph.node_count() as u64;
            let lower = match self.lower_expr {
                Some(e) => eval_to_id(e, &new_frame(symtab), graph, symtab)?.unwrap_or(0),
                None => 0,
            };
            let upper = match self.upper_expr {
                Some(e) => eval_to_id(e, &new_frame(symtab), graph, symtab)?.unwrap_or(node_count),
                None => node_count,
            };
            self.lower = lower;
            self.upper = upper.min(node_count);
            self.cur = self.lower;

            if let Some(lbl) = self.label {
                match graph.strings().get(lbl) {
                    Some(raw) => self.label_id = Some(raw),
                    None => {
                        self.cur = self.upper;
                    }
                }
            }
            self.init = true;
        }

        while self.cur < self.upper {
            let nid = NodeId(self.cur);
            self.cur += 1;
            let node = match graph.get_node(nid) {
                Ok(n) => n,
                Err(_) => continue,
            };
            if let Some(want) = self.label_id
                && !node.labels.iter().any(|lid| lid.0 == want)
            {
                continue;
            }
            let slot = symtab.slot(self.variable).unwrap();
            let mut frame = new_frame(symtab);
            frame[slot] = SlotValue::NodeRef(nid);
            return Ok(Some(frame));
        }
        Ok(None)
    }
}

// -- NoRowsOp -----------------------------------------------------------

/// Emits no rows. For Filters whose predicate folded to `false`: the input is never built or run.
pub(crate) struct NoRowsOp;

impl NoRowsOp {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Operator for NoRowsOp {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        Ok(None)
    }
}

// -- NodeByIdOp ---------------------------------------------------------

/// O(1) node lookup by internal id with optional label check, from `Scan + Filter(id(var) =
/// lit-or-param)` (`convert_filter_to_node_by_id`): `MATCH (n:User) WHERE id(n) = 0` fetches
/// directly instead of scanning.
pub(crate) struct NodeByIdOp<'p> {
    pub(crate) variable: &'p str,
    pub(crate) label: Option<&'p str>,
    pub(crate) id_expr: &'p Expr,
    emitted: bool,
}

impl<'p> NodeByIdOp<'p> {
    pub(crate) fn new(variable: &'p str, label: Option<&'p str>, id_expr: &'p Expr) -> Self {
        Self {
            variable,
            label,
            id_expr,
            emitted: false,
        }
    }
}

impl<'p> Operator for NodeByIdOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;

        // evaluate the id; the rewrite already checked it's a literal or parameter, but be
        // defensive: anything else, a missing parameter or a negative id means no match
        let id_val = eval_expr(self.id_expr, &new_frame(symtab), &*graph, symtab)?;
        let id = match id_val {
            crate::types::CypherValue::Integer(n) if n >= 0 => NodeId(n as u64),
            _ => return Ok(None),
        };

        // missing/deleted ids error here: no match
        let node = match graph.get_node(id) {
            Ok(n) => n,
            Err(_) => return Ok(None),
        };

        // the node must carry the label
        if let Some(lbl) = self.label {
            let want = match graph.strings().get(lbl) {
                Some(raw) => raw,
                None => return Ok(None), // label unknown to the graph: no node has it
            };
            if !node.labels.iter().any(|lid| lid.0 == want) {
                return Ok(None);
            }
        }

        let slot = symtab.slot(self.variable).unwrap();
        let mut frame = new_frame(symtab);
        frame[slot] = SlotValue::NodeRef(id);
        Ok(Some(frame))
    }
}

// -- IndexScanOp --------------------------------------------------------

pub(crate) struct IndexScanOp<'p> {
    pub(crate) variable: &'p str,
    pub(crate) label: &'p str,
    pub(crate) property: &'p str,
    pub(crate) value: &'p Expr,
    node_ids: Option<Vec<NodeId>>,
    pos: usize,
}

impl<'p> IndexScanOp<'p> {
    pub(crate) fn new(
        variable: &'p str,
        label: &'p str,
        property: &'p str,
        value: &'p Expr,
    ) -> Self {
        Self {
            variable,
            label,
            property,
            value,
            node_ids: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for IndexScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.node_ids.is_none() {
            let val = eval_expr(self.value, &new_frame(symtab), &*graph, symtab)?;
            let key = cypher_value_to_index_key(&val);
            let ids = match key {
                Some(k) => graph
                    .index_lookup(self.label, self.property, &k)
                    .unwrap_or_default(),
                None => vec![],
            };
            // valid nodes only
            let ids: Vec<NodeId> = ids
                .into_iter()
                .filter(|&nid| graph.get_node(nid).is_ok())
                .collect();
            self.node_ids = Some(ids);
        }
        let ids = self.node_ids.as_ref().unwrap();
        if self.pos < ids.len() {
            let nid = ids[self.pos];
            self.pos += 1;
            let slot = symtab.slot(self.variable).unwrap();
            let mut frame = new_frame(symtab);
            frame[slot] = SlotValue::NodeRef(nid);
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- IndexRangeScanOp --------------------------------------------------

pub(crate) struct IndexRangeScanOp<'p> {
    pub(crate) variable: &'p str,
    pub(crate) label: &'p str,
    pub(crate) property: &'p str,
    pub(crate) op: CmpOp,
    pub(crate) value: &'p Expr,
    node_ids: Option<Vec<NodeId>>,
    pos: usize,
}

impl<'p> IndexRangeScanOp<'p> {
    pub(crate) fn new(
        variable: &'p str,
        label: &'p str,
        property: &'p str,
        op: CmpOp,
        value: &'p Expr,
    ) -> Self {
        Self {
            variable,
            label,
            property,
            op,
            value,
            node_ids: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for IndexRangeScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.node_ids.is_none() {
            // morsel override (pre-computed by pipeline_par)?
            let ids = if let Some(morsel) = super::pipeline_par::take_morsel_ids() {
                morsel
            } else {
                let val = eval_expr(self.value, &new_frame(symtab), &*graph, symtab)?;
                let key = cypher_value_to_index_key(&val);
                let ids = match key {
                    Some(k) => {
                        let (lower, upper) = match self.op {
                            CmpOp::Gt => (Bound::Excluded(k), Bound::Unbounded),
                            CmpOp::Gte => (Bound::Included(k), Bound::Unbounded),
                            CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(k)),
                            CmpOp::Lte => (Bound::Unbounded, Bound::Included(k)),
                            _ => unreachable!("IndexRangeScan only supports Gt/Gte/Lt/Lte"),
                        };
                        graph
                            .index_range_lookup(self.label, self.property, lower, upper)
                            .unwrap_or_default()
                    }
                    None => vec![],
                };
                ids.into_iter()
                    .filter(|&nid| graph.get_node(nid).is_ok())
                    .collect()
            };
            self.node_ids = Some(ids);
        }
        let ids = self.node_ids.as_ref().unwrap();
        if self.pos < ids.len() {
            let nid = ids[self.pos];
            self.pos += 1;
            let slot = symtab.slot(self.variable).unwrap();
            let mut frame = new_frame(symtab);
            frame[slot] = SlotValue::NodeRef(nid);
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- VectorSearchScanOp ------------------------------------------------

/// `SEARCH` subclause (Cypher 25): per input frame, bind the variable to the top-k ANN hits of a
/// vector index (by descending similarity), optionally with the score. An in-index `WHERE` computes
/// top-k *among* passing nodes (filtered kNN by over-fetch + escalate; the HNSW has no native
/// filtered traversal).
pub(crate) struct VectorSearchScanOp<'p> {
    child: Box<dyn Operator + 'p>,
    variable: &'p str,
    index_name: &'p str,
    query_vector: &'p Expr,
    index_where: Option<&'p Expr>,
    limit: &'p Expr,
    score_var: Option<&'p str>,
    /// Output rows pending for the current input frame.
    pending: Vec<Frame>,
    pos: usize,
    child_done: bool,
}

impl<'p> VectorSearchScanOp<'p> {
    pub(crate) fn new(
        child: Box<dyn Operator + 'p>,
        variable: &'p str,
        index_name: &'p str,
        query_vector: &'p Expr,
        index_where: Option<&'p Expr>,
        limit: &'p Expr,
        score_var: Option<&'p str>,
    ) -> Self {
        Self {
            child,
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
            pending: Vec::new(),
            pos: 0,
            child_done: false,
        }
    }
}

/// Runs a `SEARCH` for one input frame: rows (input + node [+ score]) by descending similarity.
/// Shared by [`VectorSearchScanOp`] and the seeded OPTIONAL MATCH path.
pub(crate) fn vector_search_rows(
    frame: &Frame,
    variable: &str,
    index_name: &str,
    query_vector: &Expr,
    index_where: Option<&Expr>,
    limit: &Expr,
    score_var: Option<&str>,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    // LIMIT k: a non-negative integer
    let k = match eval_expr(limit, frame, graph, symtab)? {
        CypherValue::Integer(n) if n >= 0 => n as usize,
        other => {
            return Err(CypherError::type_error(format!(
                "the LIMIT in a SEARCH clause must be a non-negative INTEGER, got {other:?}"
            )));
        }
    };
    if k == 0 {
        return Ok(Vec::new());
    }

    // query vector: NULL -> no rows (Neo4j), else LIST<numeric>
    let query = match eval_expr(query_vector, frame, graph, symtab)? {
        CypherValue::Null => return Ok(Vec::new()),
        CypherValue::List(items) => super::call::vector_from_list(&items)?,
        other => {
            return Err(CypherError::type_error(format!(
                "the query vector in a SEARCH clause must be a LIST<FLOAT | INTEGER>, got {}",
                other.type_name()
            )));
        }
    };

    // the index must exist and dimensions must match
    let dim = graph
        .vector_index_descriptors()
        .into_iter()
        .find(|(name, ..)| name == index_name)
        .map(|(_, _, _, dim, _)| dim)
        .ok_or_else(|| {
            CypherError::semantic(format!(
                "there is no such vector schema index: {index_name}"
            ))
        })?;
    if dim > 0 && query.len() != dim {
        return Err(CypherError::semantic(format!(
            "Vector index {index_name} has a configured dimensionality of {dim}, but the \
             provided vector has a dimension {}",
            query.len()
        )));
    }

    let var_slot = symtab.slot(variable).unwrap();
    let hits: Vec<(NodeId, f64)> = match index_where {
        None => graph
            .query_vector_index(index_name, &query, k, None)
            .unwrap_or_default(),
        Some(pred) => {
            // filtered kNN: over-fetch and escalate until k candidates pass or the whole index was
            // seen
            let cap = graph.node_count().max(k);
            let mut fetch = (k * 4).max(64).min(cap);
            loop {
                let candidates = graph
                    .query_vector_index(index_name, &query, fetch, None)
                    .unwrap_or_default();
                let mut passing = Vec::with_capacity(k);
                for (nid, score) in &candidates {
                    let mut probe = frame.clone();
                    probe[var_slot] = SlotValue::NodeRef(*nid);
                    let keep = matches!(
                        eval_expr(pred, &probe, graph, symtab)?,
                        CypherValue::Bool(true)
                    );
                    if keep {
                        passing.push((*nid, *score));
                        if passing.len() == k {
                            break;
                        }
                    }
                }
                if passing.len() >= k || fetch >= cap || candidates.len() < fetch {
                    break passing;
                }
                fetch = (fetch * 4).min(cap);
                crate::query::check()?;
            }
        }
    };

    let mut out_rows = Vec::with_capacity(hits.len());
    for (nid, score) in hits {
        if graph.get_node(nid).is_err() {
            continue;
        }
        let mut out = frame.clone();
        out[var_slot] = SlotValue::NodeRef(nid);
        if let Some(sv) = score_var {
            let s_slot = symtab.slot(sv).unwrap();
            out[s_slot] = SlotValue::Float(score);
        }
        out_rows.push(out);
    }
    Ok(out_rows)
}

impl<'p> Operator for VectorSearchScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        loop {
            if self.pos < self.pending.len() {
                let f = std::mem::take(&mut self.pending[self.pos]);
                self.pos += 1;
                return Ok(Some(f));
            }
            if self.child_done {
                return Ok(None);
            }
            self.pending.clear();
            self.pos = 0;
            match self.child.next(graph, symtab)? {
                Some(frame) => {
                    self.pending = vector_search_rows(
                        &frame,
                        self.variable,
                        self.index_name,
                        self.query_vector,
                        self.index_where,
                        self.limit,
                        self.score_var,
                        &*graph,
                        symtab,
                    )?;
                }
                None => self.child_done = true,
            }
        }
    }
}

// -- TextIndexScanOp ---------------------------------------------------

/// Trigram-accelerated candidates for a string predicate: a SUPERSET the enclosing `Filter`
/// refines. The pattern is evaluated once on an empty frame; a non-string or null pattern yields
/// the full label scan, so the filter keeps exact semantics (type errors, null drops).
pub(crate) struct TextIndexScanOp<'p> {
    variable: &'p str,
    label: &'p str,
    property: &'p str,
    kind: TextPredicateKind,
    pattern: &'p Expr,
    node_ids: Option<Vec<NodeId>>,
    pos: usize,
}

impl<'p> TextIndexScanOp<'p> {
    pub(crate) fn new(
        variable: &'p str,
        label: &'p str,
        property: &'p str,
        kind: TextPredicateKind,
        pattern: &'p Expr,
    ) -> Self {
        Self {
            variable,
            label,
            property,
            kind,
            pattern,
            node_ids: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for TextIndexScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.node_ids.is_none() {
            let val = eval_expr(self.pattern, &new_frame(symtab), &*graph, symtab)?;
            let ids: Vec<NodeId> = match val {
                CypherValue::String(s) => {
                    let kind = match self.kind {
                        TextPredicateKind::Equals => {
                            traverse_core::graph::text::TextQueryKind::Equals
                        }
                        TextPredicateKind::StartsWith => {
                            traverse_core::graph::text::TextQueryKind::StartsWith
                        }
                        TextPredicateKind::EndsWith => {
                            traverse_core::graph::text::TextQueryKind::EndsWith
                        }
                        TextPredicateKind::Contains => {
                            traverse_core::graph::text::TextQueryKind::Contains
                        }
                    };
                    graph
                        .text_index_candidates(self.label, self.property, kind, &s)
                        // index dropped between plan and execute: label scan
                        .unwrap_or_else(|| graph.nodes_by_label(self.label))
                }
                // null / non-string pattern: full label scan, the Filter keeps exact semantics
                _ => graph.nodes_by_label(self.label),
            };
            let ids = ids
                .into_iter()
                .filter(|&nid| graph.get_node(nid).is_ok())
                .collect();
            self.node_ids = Some(ids);
        }
        let ids = self.node_ids.as_ref().unwrap();
        if self.pos < ids.len() {
            let nid = ids[self.pos];
            self.pos += 1;
            let slot = symtab.slot(self.variable).unwrap();
            let mut frame = new_frame(symtab);
            frame[slot] = SlotValue::NodeRef(nid);
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- PointIndexScanOp --------------------------------------------------

/// R-tree-accelerated candidates for a spatial predicate, refined by the enclosing
/// `Filter`/`Sort`/`Limit`. Point / radius / bbox / `k` are evaluated once on an empty frame (they
/// use no bound variables).
pub(crate) struct PointIndexScanOp<'p> {
    variable: &'p str,
    label: &'p str,
    property: &'p str,
    query: &'p SpatialQuery,
    node_ids: Option<Vec<NodeId>>,
    pos: usize,
}

impl<'p> PointIndexScanOp<'p> {
    pub(crate) fn new(
        variable: &'p str,
        label: &'p str,
        property: &'p str,
        query: &'p SpatialQuery,
    ) -> Self {
        Self {
            variable,
            label,
            property,
            query,
            node_ids: None,
            pos: 0,
        }
    }

    fn compute_ids(
        &self,
        graph: &dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Vec<NodeId>, CypherError> {
        let frame = new_frame(symtab);
        let ids: Vec<NodeId> = match self.query {
            SpatialQuery::Distance { center, radius } => {
                let Some((c, srid)) = eval_point(center, &frame, graph, symtab)? else {
                    return Ok(vec![]);
                };
                let Some(r) = eval_number(radius, &frame, graph, symtab)? else {
                    return Ok(vec![]);
                };
                graph
                    .point_index_within_distance(self.label, self.property, c, r, srid)
                    .map(|v| v.into_iter().map(|(n, _)| n).collect())
                    .unwrap_or_default()
            }
            SpatialQuery::BBox { ll, ur } => {
                let (Some((ll_c, ll_srid)), Some((ur_c, ur_srid))) = (
                    eval_point(ll, &frame, graph, symtab)?,
                    eval_point(ur, &frame, graph, symtab)?,
                ) else {
                    return Ok(vec![]);
                };
                if ll_srid != ur_srid {
                    return Ok(vec![]);
                }
                graph
                    .point_index_within_bbox(self.label, self.property, ll_c, ur_c, ll_srid)
                    .unwrap_or_default()
            }
            SpatialQuery::Knn { center, k } => {
                let Some((c, srid)) = eval_point(center, &frame, graph, symtab)? else {
                    return Ok(vec![]);
                };
                let k = match eval_number(k, &frame, graph, symtab)? {
                    Some(k) if k >= 0.0 => k as usize,
                    _ => return Ok(vec![]),
                };
                graph
                    .point_index_knn(self.label, self.property, c, k, srid)
                    .map(|v| v.into_iter().map(|(n, _)| n).collect())
                    .unwrap_or_default()
            }
        };
        Ok(ids
            .into_iter()
            .filter(|&nid| graph.get_node(nid).is_ok())
            .collect())
    }
}

impl<'p> Operator for PointIndexScanOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.node_ids.is_none() {
            let ids = self.compute_ids(&*graph, symtab)?;
            self.node_ids = Some(ids);
        }
        let ids = self.node_ids.as_ref().unwrap();
        if self.pos < ids.len() {
            let nid = ids[self.pos];
            self.pos += 1;
            let slot = symtab.slot(self.variable).unwrap();
            let mut frame = new_frame(symtab);
            frame[slot] = SlotValue::NodeRef(nid);
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

/// Expression expected to be a point -> `([x,y,z], srid)`; `None` if null or not a point map.
fn eval_point(
    e: &Expr,
    frame: &Frame,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Option<([f64; 3], i64)>, CypherError> {
    let v = eval_expr(e, frame, graph, symtab)?;
    Ok(cypher_point_coords(&v))
}

/// Numeric expression -> `f64`; `None` if null / non-numeric.
fn eval_number(
    e: &Expr,
    frame: &Frame,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Option<f64>, CypherError> {
    Ok(match eval_expr(e, frame, graph, symtab)? {
        CypherValue::Integer(i) => Some(i as f64),
        CypherValue::Float(f) => Some(f),
        _ => None,
    })
}

/// `([x, y, z], srid)` of a `{x, y, z?, srid, crs}` point map (what `point()` builds); like core
/// `spatial::point_coords`.
fn cypher_point_coords(v: &CypherValue) -> Option<([f64; 3], i64)> {
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

// -- ScanFilterProjectOp ------------------------------------------------

/// Fused scan + filter + project. Blocking; delegates to `exec_scan_filter_project` (pre-resolved
/// property keys, batched evaluation).
pub(crate) struct ScanFilterProjectOp<'p> {
    pub(crate) plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> ScanFilterProjectOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for ScanFilterProjectOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::ScanFilterProject {
                variable,
                label,
                predicate,
                items,
                distinct,
                limit,
            } = self.plan
            else {
                unreachable!();
            };
            let frames = exec_scan_filter_project(
                variable, label, predicate, items, *distinct, graph, *limit, symtab,
            )?;
            self.buffer = Some(frames);
        }
        let buf = self.buffer.as_mut().unwrap();
        if self.pos < buf.len() {
            let frame = std::mem::take(&mut buf[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- IndexLookupJoinOp -------------------------------------------------

/// Input-driven index seek: per input frame, evaluate the key (may use input variables) and join
/// the frame with each hit. Replaces `Filter(var.prop = key) over CrossProduct(input, Scan)`.
pub(crate) struct IndexLookupJoinOp<'p> {
    pub(crate) input: Box<dyn Operator + 'p>,
    pub(crate) variable: &'p str,
    pub(crate) label: &'p str,
    pub(crate) property: &'p str,
    pub(crate) key: &'p Expr,
    current: Option<(Frame, Vec<NodeId>, usize)>,
}

impl<'p> IndexLookupJoinOp<'p> {
    pub(crate) fn new(
        input: Box<dyn Operator + 'p>,
        variable: &'p str,
        label: &'p str,
        property: &'p str,
        key: &'p Expr,
    ) -> Self {
        Self {
            input,
            variable,
            label,
            property,
            key,
            current: None,
        }
    }
}

impl<'p> Operator for IndexLookupJoinOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        loop {
            if let Some((frame, ids, pos)) = &mut self.current {
                if *pos < ids.len() {
                    let nid = ids[*pos];
                    *pos += 1;
                    let mut out = frame.clone();
                    let slot = symtab.slot(self.variable).unwrap();
                    out[slot] = SlotValue::NodeRef(nid);
                    return Ok(Some(out));
                }
                self.current = None;
            }
            let Some(frame) = self.input.next(graph, symtab)? else {
                return Ok(None);
            };
            let val = eval_expr(self.key, &frame, &*graph, symtab)?;
            let ids = match cypher_value_to_index_key(&val) {
                Some(k) => {
                    let hits = graph
                        .index_lookup(self.label, self.property, &k)
                        .unwrap_or_default();
                    hits.into_iter()
                        .filter(|&nid| graph.get_node(nid).is_ok())
                        .collect()
                }
                None => vec![],
            };
            self.current = Some((frame, ids, 0));
        }
    }
}

// -- ContextSeedOp -----------------------------------------------------

/// Emits one frame: the executor's current subquery seed (thread-local). Leaf of slot-native
/// pattern-comprehension plans.
pub(crate) struct ContextSeedOp {
    done: bool,
}

impl ContextSeedOp {
    pub(crate) fn new() -> Self {
        Self { done: false }
    }
}

impl Operator for ContextSeedOp {
    fn next(
        &mut self,
        _graph: &mut dyn GraphView,
        _symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(crate::executor::current_subquery_seed())
    }
}
