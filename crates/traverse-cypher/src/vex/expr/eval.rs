//! Evaluation of compiled expressions over a chunk, vectorized where the operands are typed and
//! lane by lane otherwise.
//!
//! An expression evaluates over one *evaluation group*: the unflat group its variables vary with
//! (every other referenced group is flat, so their variables are scalars for the chunk). The
//! result has one lane per selected position of that group, or is a scalar when no group varies.
//!
//! Errors: a vectorized kernel may raise an error on a lane that lane-by-lane evaluation would
//! never have reached (`false AND 1/0 = 1`). The entry points catch a kernel error and re-run the
//! expression lane by lane, where short-circuits and branches behave exactly as in the scalar
//! semantics; the error stands only if a lane really raises it.

use std::sync::Arc;

use rustc_hash::FxHashMap;
use traverse_core::graph::batch::ColumnSink;
use traverse_core::graph::property::IndexKey;
use traverse_core::graph::types::{EdgeId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::ast::{CmpOp, QuantifierKind};
use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::{ExecCtx, ReadOnlyView};
use super::super::ops::{PhysOperator, build_operator};
use super::super::plan::{SeedSource, SubPlan};
use super::super::values::{edge_to_cypher_value, node_to_cypher_value, resolve_ref};
use super::super::vector::{ValueRef, Vector, VectorBuilder};
use super::kernels::{self, owned};
use super::scalar;
use super::{CompiledExpr, ExprNode, FuncKind};

/// A value for every lane of the evaluation group.
#[derive(Debug)]
pub enum Operand<'a> {
    /// One value for every lane.
    Scalar(CypherValue),
    /// A column of the result set with the group's selection.
    Ref(&'a Vector, Option<&'a [u32]>),
    /// A computed vector, one lane per selected position.
    Owned(Vector),
}

impl<'a> Operand<'a> {
    /// The value at selected position `i`.
    #[inline]
    pub fn get(&self, i: usize) -> ValueRef<'_> {
        match self {
            Operand::Scalar(v) => ValueRef::of(v),
            Operand::Ref(v, sel) => v.get(match sel {
                Some(s) => s[i] as usize,
                None => i,
            }),
            Operand::Owned(v) => v.get(i),
        }
    }

    /// Materializes into a vector of `n` lanes.
    pub fn into_vector(self, n: usize) -> Vector {
        match self {
            Operand::Owned(v) => v,
            Operand::Ref(v, sel) => match sel {
                Some(s) => v.take(s),
                None => v.clone(),
            },
            Operand::Scalar(v) => {
                let mut b = VectorBuilder::with_capacity(n);
                let r = ValueRef::of(&v);
                for _ in 0..n {
                    b.push(r);
                }
                b.finish()
            }
        }
    }

    /// The scalar value, when the operand is one.
    pub fn as_scalar(&self) -> Option<&CypherValue> {
        match self {
            Operand::Scalar(v) => Some(v),
            _ => None,
        }
    }
}

/// What an evaluation runs over.
pub struct EvalCtx<'a> {
    /// The chunk.
    pub rs: &'a ResultSet,
    /// The graph.
    pub graph: &'a dyn GraphView,
    /// The evaluation group; `None` when every referenced group is flat.
    pub group: Option<GroupId>,
    /// Lanes: the group's selected length, or 1.
    pub n: usize,
}

impl<'a> EvalCtx<'a> {
    /// A context over `rs` evaluating along `group`.
    pub fn new(rs: &'a ResultSet, graph: &'a dyn GraphView, group: Option<GroupId>) -> Self {
        let n = match group {
            Some(g) => rs.group(g).selected_len(),
            None => 1,
        };
        Self {
            rs,
            graph,
            group,
            n,
        }
    }

    /// The value at `pos` for selected position `i` of the evaluation group; a scalar for other
    /// groups.
    #[inline]
    pub fn lane(&self, pos: DataPos, i: usize) -> ValueRef<'a> {
        if Some(pos.group) == self.group {
            let g = self.rs.group(pos.group);
            g.cols[pos.col as usize].get(g.lane(i))
        } else {
            self.rs.scalar_at(pos)
        }
    }
}

/// Per-operator evaluation state: local bindings and the operator instances of subqueries.
pub struct EvalScratch<'p> {
    locals: Vec<CypherValue>,
    subs: FxHashMap<usize, SubExec<'p>>,
}

impl Default for EvalScratch<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'p> EvalScratch<'p> {
    /// Empty state.
    pub fn new() -> Self {
        Self {
            locals: Vec::new(),
            subs: FxHashMap::default(),
        }
    }
}

struct SubExec<'p> {
    op: Box<dyn PhysOperator + 'p>,
    rs: ResultSet,
}

/// How a subquery's rows are consumed.
enum SubMode {
    Exists,
    Count,
    Collect,
}

impl CompiledExpr {
    /// Evaluates over the lanes of `cx.group`.
    pub fn eval<'a, 'p>(
        &'p self,
        cx: &EvalCtx<'a>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<Operand<'a>, CypherError>
    where
        'p: 'a,
    {
        match self.eval_node(&self.root, cx, sc) {
            Ok(op) => Ok(op),
            // see the module docs: the error stands only if a lane raises it on its own
            Err(_) => self.eval_lanewise(&self.root, cx, sc).map(Operand::Owned),
        }
    }

    /// Evaluates as a scalar: the evaluation group is `None` (or has one selected lane).
    pub fn eval_scalar<'p>(
        &'p self,
        rs: &ResultSet,
        graph: &dyn GraphView,
        sc: &mut EvalScratch<'p>,
    ) -> Result<CypherValue, CypherError> {
        let cx = EvalCtx::new(rs, graph, None);
        self.eval_lane(&self.root, &cx, sc, 0)
    }

    /// Evaluates at selected position `i` of `cx.group`.
    pub fn eval_at<'p>(
        &'p self,
        cx: &EvalCtx<'_>,
        sc: &mut EvalScratch<'p>,
        i: usize,
    ) -> Result<CypherValue, CypherError> {
        self.eval_lane(&self.root, cx, sc, i)
    }

    /// Evaluates at an explicit lane assignment (`lanes[group]` = lane), for row consumers that
    /// iterate the cross product of several unflat groups.
    pub fn eval_row<'p>(
        &'p self,
        rs: &ResultSet,
        graph: &dyn GraphView,
        lanes: &[usize],
        sc: &mut EvalScratch<'p>,
    ) -> Result<CypherValue, CypherError> {
        let cx = RowCtx { rs, graph, lanes };
        self.eval_row_node(&self.root, &cx, sc)
    }

    fn eval_lanewise<'p>(
        &'p self,
        node: &'p ExprNode,
        cx: &EvalCtx<'_>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<Vector, CypherError> {
        let mut b = VectorBuilder::with_capacity(cx.n);
        for i in 0..cx.n {
            b.push_owned(self.eval_lane(node, cx, sc, i)?);
        }
        Ok(b.finish())
    }

    // -- vectorized ------------------------------------------------------

    #[recursive::recursive]
    fn eval_node<'a, 'p>(
        &'p self,
        node: &'p ExprNode,
        cx: &EvalCtx<'a>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<Operand<'a>, CypherError>
    where
        'p: 'a,
    {
        let n = cx.n;
        Ok(match node {
            ExprNode::Const(v) => Operand::Scalar(v.clone()),
            ExprNode::Param(name) => Operand::Scalar(param(name)?),
            ExprNode::Unbound(_) => Operand::Scalar(CypherValue::Null),
            ExprNode::Var(pos) => {
                if Some(pos.group) == cx.group {
                    let g = cx.rs.group(pos.group);
                    Operand::Ref(&g.cols[pos.col as usize], g.sel.as_deref())
                } else {
                    Operand::Scalar(cx.rs.scalar_at(*pos).to_owned())
                }
            }
            ExprNode::Local(_) => Operand::Owned(self.eval_lanewise(node, cx, sc)?),
            ExprNode::Prop { base, key } => {
                let b = self.eval_node(base, cx, sc)?;
                self.gather_prop(b, key, cx)?
            }
            ExprNode::Cmp { op, l, r } => {
                let lo = self.eval_node(l, cx, sc)?;
                let ro = self.eval_node(r, cx, sc)?;
                if let (Some(a), Some(b)) = (lo.as_scalar(), ro.as_scalar()) {
                    Operand::Scalar(scalar::eval_comparison(a, *op, b))
                } else {
                    Operand::Owned(kernels::compare(*op, &lo, &ro, n))
                }
            }
            ExprNode::Arith { op, l, r } => {
                let lo = self.eval_node(l, cx, sc)?;
                let ro = self.eval_node(r, cx, sc)?;
                if let (Some(a), Some(b)) = (lo.as_scalar(), ro.as_scalar()) {
                    Operand::Scalar(scalar::eval_binary_op(a, *op, b)?)
                } else {
                    Operand::Owned(kernels::arith(*op, &lo, &ro, n)?)
                }
            }
            ExprNode::And(l, r) => {
                let lo = self.eval_node(l, cx, sc)?;
                // lanes where the right side matters: left not false
                let mut need = false;
                for i in 0..n {
                    if kernels::lane_bool3(lo.get(i)) != Some(false) {
                        need = true;
                        break;
                    }
                }
                if !need {
                    return Ok(Operand::Owned(Vector::bools(vec![false; n])));
                }
                let ro = match self.eval_node(r, cx, sc) {
                    Ok(o) => o,
                    Err(_) => {
                        // the right side fails on some lane: run it only where it is needed
                        return Ok(Operand::Owned(self.eval_lanewise(node, cx, sc)?));
                    }
                };
                Operand::Owned(kernels::bool3_from(n, |i| {
                    let a = kernels::lane_bool3(lo.get(i));
                    if a == Some(false) {
                        return Some(false);
                    }
                    let b = kernels::lane_bool3(ro.get(i));
                    match (a, b) {
                        (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    }
                }))
            }
            ExprNode::Or(l, r) => {
                let lo = self.eval_node(l, cx, sc)?;
                let mut need = false;
                for i in 0..n {
                    if kernels::lane_bool3(lo.get(i)) != Some(true) {
                        need = true;
                        break;
                    }
                }
                if !need {
                    return Ok(Operand::Owned(Vector::bools(vec![true; n])));
                }
                let ro = match self.eval_node(r, cx, sc) {
                    Ok(o) => o,
                    Err(_) => return Ok(Operand::Owned(self.eval_lanewise(node, cx, sc)?)),
                };
                Operand::Owned(kernels::bool3_from(n, |i| {
                    let a = kernels::lane_bool3(lo.get(i));
                    if a == Some(true) {
                        return Some(true);
                    }
                    let b = kernels::lane_bool3(ro.get(i));
                    match (a, b) {
                        (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    }
                }))
            }
            ExprNode::Xor(l, r) => {
                let lo = self.eval_node(l, cx, sc)?;
                let ro = self.eval_node(r, cx, sc)?;
                Operand::Owned(kernels::xor(&lo, &ro, n))
            }
            ExprNode::Not(e) => {
                let o = self.eval_node(e, cx, sc)?;
                Operand::Owned(kernels::not(&o, n))
            }
            ExprNode::Neg(e) => {
                let o = self.eval_node(e, cx, sc)?;
                Operand::Owned(kernels::neg(&o, n)?)
            }
            ExprNode::IsNull(e) => {
                let o = self.eval_node(e, cx, sc)?;
                Operand::Owned(kernels::is_null(&o, n, false))
            }
            ExprNode::IsNotNull(e) => {
                let o = self.eval_node(e, cx, sc)?;
                Operand::Owned(kernels::is_null(&o, n, true))
            }
            ExprNode::IsTyped {
                e,
                type_name,
                negated,
                not_null,
            } => {
                let o = self.eval_node(e, cx, sc)?;
                let mut vals = Vec::with_capacity(n);
                for i in 0..n {
                    let v = owned(o.get(i));
                    vals.push(scalar::type_predicate_matches(&v, type_name, *not_null) != *negated);
                }
                Operand::Owned(Vector::bools(vals))
            }
            ExprNode::In { item, list } => {
                let io = self.eval_node(item, cx, sc)?;
                let lo = self.eval_node(list, cx, sc)?;
                Operand::Owned(kernels::in_list(&io, &lo, n)?)
            }
            ExprNode::StringOp { op, l, r } => {
                let lo = self.eval_node(l, cx, sc)?;
                let ro = self.eval_node(r, cx, sc)?;
                Operand::Owned(kernels::string_op(*op, &lo, &ro, n)?)
            }
            ExprNode::Func { name, kind, args } => match kind {
                FuncKind::Generic
                    if args.len() == 1
                        && matches!(name.as_str(), "tointeger" | "toint" | "tofloat") =>
                {
                    let o = self.eval_node(&args[0], cx, sc)?;
                    let to_float = name == "tofloat";
                    let mut b = VectorBuilder::with_capacity(n);
                    for i in 0..n {
                        match (o.get(i), to_float) {
                            (ValueRef::Null, _) => b.push_null(),
                            (ValueRef::Int(v), false) => b.push_int(v),
                            (ValueRef::Float(f), false) => b.push_int(f as i64),
                            (ValueRef::Int(v), true) => b.push_float(v as f64),
                            (ValueRef::Float(f), true) => b.push_float(f),
                            // strings, booleans, the type errors: as the function answers them
                            _ => return Ok(Operand::Owned(self.eval_lanewise(node, cx, sc)?)),
                        }
                    }
                    Operand::Owned(b.finish())
                }
                FuncKind::Id | FuncKind::ElementId if args.len() == 1 => {
                    let o = self.eval_node(&args[0], cx, sc)?;
                    let is_elem = *kind == FuncKind::ElementId;
                    let mut b = VectorBuilder::with_capacity(n);
                    for i in 0..n {
                        match o.get(i) {
                            ValueRef::Node(id) => push_id(&mut b, id.0, is_elem),
                            ValueRef::Edge { id, .. } => push_id(&mut b, id.0, is_elem),
                            ValueRef::Any(CypherValue::Node { id, .. }) => {
                                push_id(&mut b, id.0, is_elem)
                            }
                            ValueRef::Any(CypherValue::Edge { id, .. }) => {
                                push_id(&mut b, id.0, is_elem)
                            }
                            ValueRef::Null => b.push_null(),
                            other => {
                                return Err(CypherError::type_error(format!(
                                    "{}() requires a node or relationship, got {}",
                                    if is_elem { "elementId" } else { "id" },
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    Operand::Owned(b.finish())
                }
                FuncKind::Type if args.len() == 1 => {
                    let o = self.eval_node(&args[0], cx, sc)?;
                    let mut b = VectorBuilder::with_capacity(n);
                    for i in 0..n {
                        match o.get(i) {
                            ValueRef::Edge { etype, .. } => {
                                b.push_str(cx.graph.strings().resolve(etype.0));
                            }
                            ValueRef::Any(CypherValue::Edge { edge_type, .. }) => {
                                b.push_str(edge_type);
                            }
                            ValueRef::Null => b.push_null(),
                            other => {
                                return Err(CypherError::type_error(format!(
                                    "type() requires a relationship, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    Operand::Owned(b.finish())
                }
                FuncKind::StartNode | FuncKind::EndNode if args.len() == 1 => {
                    let o = self.eval_node(&args[0], cx, sc)?;
                    let start = *kind == FuncKind::StartNode;
                    let mut b = VectorBuilder::with_capacity(n);
                    for i in 0..n {
                        match o.get(i) {
                            ValueRef::Edge { src, tgt, .. } => {
                                b.push_node(if start { src } else { tgt });
                            }
                            ValueRef::Any(CypherValue::Edge { source, target, .. }) => {
                                b.push_node(if start { *source } else { *target });
                            }
                            ValueRef::Null => b.push_null(),
                            other => {
                                return Err(CypherError::type_error(format!(
                                    "{}() requires a relationship, got {}",
                                    if start { "startNode" } else { "endNode" },
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    Operand::Owned(b.finish())
                }
                FuncKind::HasLabel(label) if args.len() == 2 => {
                    let o = self.eval_node(&args[0], cx, sc)?;
                    self.has_label_vec(&o, label, cx)?
                }
                FuncKind::AsNode => {
                    let o = self.eval_node(&args[0], cx, sc)?;
                    let mut lane_ids: Vec<Option<NodeId>> = Vec::with_capacity(n);
                    for i in 0..n {
                        lane_ids.push(match o.get(i) {
                            ValueRef::Int(v) if v >= 0 => Some(NodeId(v as u64)),
                            ValueRef::Node(id) => Some(id),
                            ValueRef::Null | ValueRef::Int(_) => None,
                            // anything else as the function answers it, lane by lane
                            _ => return Ok(Operand::Owned(self.eval_lanewise(node, cx, sc)?)),
                        });
                    }
                    // one existence check for the chunk
                    let batch: Vec<NodeId> = lane_ids.iter().flatten().copied().collect();
                    let mut exists = Vec::with_capacity(batch.len());
                    cx.graph.nodes_exist(&batch, &mut exists);
                    let mut found = exists.into_iter();
                    let mut b = VectorBuilder::with_capacity(n);
                    for id in lane_ids {
                        match id {
                            Some(id) if found.next() == Some(true) => b.push_node(id),
                            _ => b.push_null(),
                        }
                    }
                    Operand::Owned(b.finish())
                }
                FuncKind::Coalesce => {
                    // left to right, each argument only for the lanes still null
                    let mut out: Vec<Option<CypherValue>> = vec![None; n];
                    let mut remaining = n;
                    for a in args {
                        if remaining == 0 {
                            break;
                        }
                        let o = match self.eval_node(a, cx, sc) {
                            Ok(o) => o,
                            Err(_) => return Ok(Operand::Owned(self.eval_lanewise(node, cx, sc)?)),
                        };
                        for (i, slot) in out.iter_mut().enumerate() {
                            if slot.is_none() {
                                let v = o.get(i);
                                if !v.is_null() {
                                    *slot = Some(v.to_owned());
                                    remaining -= 1;
                                }
                            }
                        }
                    }
                    Operand::Owned(Vector::from_values(
                        out.into_iter()
                            .map(|v| v.unwrap_or(CypherValue::Null))
                            .collect(),
                    ))
                }
                _ => Operand::Owned(self.eval_lanewise(node, cx, sc)?),
            },
            // everything else evaluates lane by lane
            _ => Operand::Owned(self.eval_lanewise(node, cx, sc)?),
        })
    }

    /// `base.key` over the evaluation group: one gather per chunk for entity vectors.
    fn gather_prop<'a>(
        &self,
        base: Operand<'a>,
        key: &str,
        cx: &EvalCtx<'a>,
    ) -> Result<Operand<'a>, CypherError> {
        let n = cx.n;
        if let Operand::Scalar(v) = &base {
            return Ok(Operand::Scalar(scalar::property_of(v, key, cx.graph)?));
        }
        let kid = cx
            .graph
            .strings()
            .get(key)
            .map(traverse_core::graph::types::PropertyKeyId);
        // which kind of entity column is it?
        let (vec, sel) = match &base {
            Operand::Ref(v, sel) => (*v, *sel),
            Operand::Owned(v) => (v, None),
            Operand::Scalar(_) => unreachable!(),
        };
        match vec {
            Vector::Node { ids, valid } => {
                let mut nulls = 0usize;
                let lane_ids: Vec<NodeId> = if sel.is_none() && valid.iter().all(|v| *v) {
                    ids.clone()
                } else {
                    let mut lane_ids: Vec<NodeId> = Vec::with_capacity(n);
                    for i in 0..n {
                        let lane = sel.map_or(i, |s| s[i] as usize);
                        if valid[lane] {
                            lane_ids.push(ids[lane]);
                        } else {
                            nulls += 1;
                            lane_ids.push(NodeId(u64::MAX));
                        }
                    }
                    lane_ids
                };
                let Some(kid) = kid else {
                    // the key was never interned: every value is null, but a deleted node is still
                    // an error
                    self.check_nodes_exist(&lane_ids, nulls, cx.graph)?;
                    return Ok(Operand::Owned(Vector::nulls(n)));
                };
                let mut b = VectorBuilder::with_capacity(n);
                let missing = cx
                    .graph
                    .gather_node_props(&lane_ids, &[kid], &mut [&mut b])?;
                if missing > nulls {
                    self.check_nodes_exist(&lane_ids, nulls, cx.graph)?;
                }
                Ok(Operand::Owned(b.finish()))
            }
            Vector::Edge { ids, valid, .. } => {
                let mut lane_ids: Vec<EdgeId> = Vec::with_capacity(n);
                let mut nulls = 0usize;
                for i in 0..n {
                    let lane = sel.map_or(i, |s| s[i] as usize);
                    if valid[lane] {
                        lane_ids.push(ids[lane]);
                    } else {
                        nulls += 1;
                        lane_ids.push(EdgeId(u64::MAX));
                    }
                }
                let Some(kid) = kid else {
                    self.check_edges_exist(&lane_ids, nulls, cx.graph)?;
                    return Ok(Operand::Owned(Vector::nulls(n)));
                };
                let mut b = VectorBuilder::with_capacity(n);
                let missing = cx
                    .graph
                    .gather_edge_props(&lane_ids, &[kid], &mut [&mut b])?;
                if missing > nulls {
                    self.check_edges_exist(&lane_ids, nulls, cx.graph)?;
                }
                Ok(Operand::Owned(b.finish()))
            }
            _ => {
                let mut b = VectorBuilder::with_capacity(n);
                for i in 0..n {
                    let v = owned(base.get(i));
                    b.push_owned(scalar::property_of(&v, key, cx.graph)?);
                }
                Ok(Operand::Owned(b.finish()))
            }
        }
    }

    fn check_nodes_exist(
        &self,
        ids: &[NodeId],
        nulls: usize,
        graph: &dyn GraphView,
    ) -> Result<(), CypherError> {
        let mut exist = Vec::with_capacity(ids.len());
        graph.nodes_exist(ids, &mut exist);
        let mut missing = 0;
        for (i, ok) in exist.iter().enumerate() {
            if !ok && ids[i].0 != u64::MAX {
                missing += 1;
            }
        }
        let _ = nulls;
        if missing > 0 {
            return Err(scalar::deleted_entity());
        }
        Ok(())
    }

    fn check_edges_exist(
        &self,
        ids: &[EdgeId],
        nulls: usize,
        graph: &dyn GraphView,
    ) -> Result<(), CypherError> {
        let mut exist = Vec::with_capacity(ids.len());
        graph.edges_exist(ids, &mut exist);
        let mut missing = 0;
        for (i, ok) in exist.iter().enumerate() {
            if !ok && ids[i].0 != u64::MAX {
                missing += 1;
            }
        }
        let _ = nulls;
        if missing > 0 {
            return Err(scalar::deleted_entity());
        }
        Ok(())
    }

    /// `__has_label(entity, 'L')` over the evaluation group: one batch check for node vectors.
    fn has_label_vec<'a>(
        &self,
        o: &Operand<'a>,
        label: &str,
        cx: &EvalCtx<'a>,
    ) -> Result<Operand<'a>, CypherError> {
        let n = cx.n;
        let (vec, sel) = match o {
            Operand::Ref(v, sel) => (*v, *sel),
            Operand::Owned(v) => (v, None),
            Operand::Scalar(v) => {
                return Ok(Operand::Scalar(scalar::has_label(v, label, cx.graph)?));
            }
        };
        if let Vector::Node { ids, valid } = vec {
            let Some(lid) = cx
                .graph
                .strings()
                .get(label)
                .map(traverse_core::graph::types::LabelId)
            else {
                // a label no node ever had: false everywhere, null for null lanes
                return Ok(Operand::Owned(kernels::bool3_from(n, |i| {
                    let lane = sel.map_or(i, |s| s[i] as usize);
                    valid[lane].then_some(false)
                })));
            };
            let mut lane_ids: Vec<NodeId> = Vec::with_capacity(n);
            for i in 0..n {
                let lane = sel.map_or(i, |s| s[i] as usize);
                lane_ids.push(if valid[lane] {
                    ids[lane]
                } else {
                    NodeId(u64::MAX)
                });
            }
            let mut has = Vec::with_capacity(n);
            cx.graph.nodes_have_label(&lane_ids, lid, &mut has);
            return Ok(Operand::Owned(kernels::bool3_from(n, |i| {
                let lane = sel.map_or(i, |s| s[i] as usize);
                valid[lane].then_some(has[i])
            })));
        }
        let mut b = VectorBuilder::with_capacity(n);
        for i in 0..n {
            let v = owned(o.get(i));
            b.push_owned(scalar::has_label(&v, label, cx.graph)?);
        }
        Ok(Operand::Owned(b.finish()))
    }

    // -- lane by lane -------------------------------------------------------

    #[recursive::recursive]
    fn eval_lane<'p>(
        &'p self,
        node: &'p ExprNode,
        cx: &EvalCtx<'_>,
        sc: &mut EvalScratch<'p>,
        i: usize,
    ) -> Result<CypherValue, CypherError> {
        let env = LaneEnv::Pos(cx, i);
        self.eval_in(node, &env, sc)
    }

    #[recursive::recursive]
    fn eval_row_node<'p>(
        &'p self,
        node: &'p ExprNode,
        cx: &RowCtx<'_>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<CypherValue, CypherError> {
        let env = LaneEnv::Row(cx);
        self.eval_in(node, &env, sc)
    }

    /// The scalar evaluator: Cypher semantics on one lane.
    #[recursive::recursive]
    fn eval_in<'p>(
        &'p self,
        node: &'p ExprNode,
        env: &LaneEnv<'_, '_>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<CypherValue, CypherError> {
        let graph = env.graph();
        Ok(match node {
            ExprNode::Const(v) => v.clone(),
            ExprNode::Param(name) => param(name)?,
            ExprNode::Unbound(_) => CypherValue::Null,
            ExprNode::Var(pos) => env.value(*pos).to_owned(),
            ExprNode::Local(k) => sc
                .locals
                .get(*k as usize)
                .cloned()
                .unwrap_or(CypherValue::Null),
            ExprNode::Prop { base, key } => {
                let b = self.eval_in(base, env, sc)?;
                scalar::property_of(&b, key, graph)?
            }
            ExprNode::Index { base, idx } => {
                let b = self.eval_in(base, env, sc)?;
                let ix = self.eval_in(idx, env, sc)?;
                scalar::index_value(&b, &ix, graph)?
            }
            ExprNode::Slice { base, from, to } => {
                let b = self.eval_in(base, env, sc)?;
                let f = match from {
                    Some(f) => Some(self.eval_in(f, env, sc)?),
                    None => None,
                };
                let t = match to {
                    Some(t) => Some(self.eval_in(t, env, sc)?),
                    None => None,
                };
                scalar::slice_value(&b, f.as_ref(), t.as_ref())?
            }
            ExprNode::Cmp { op, l, r } => {
                let a = self.eval_in(l, env, sc)?;
                let b = self.eval_in(r, env, sc)?;
                scalar::eval_comparison(&a, *op, &b)
            }
            ExprNode::CmpChain { first, rest } => {
                let mut current = self.eval_in(first, env, sc)?;
                for (op, e) in rest {
                    let right = self.eval_in(e, env, sc)?;
                    let result = scalar::eval_comparison(&current, *op, &right);
                    if result.is_null() {
                        return Ok(CypherValue::Null);
                    }
                    if !scalar::is_truthy(&result) {
                        return Ok(CypherValue::Bool(false));
                    }
                    current = right;
                }
                CypherValue::Bool(true)
            }
            ExprNode::Arith { op, l, r } => {
                let a = self.eval_in(l, env, sc)?;
                let b = self.eval_in(r, env, sc)?;
                scalar::eval_binary_op(&a, *op, &b)?
            }
            ExprNode::And(l, r) => {
                let a = self.eval_in(l, env, sc)?;
                if matches!(a, CypherValue::Bool(false)) {
                    return Ok(CypherValue::Bool(false));
                }
                let b = self.eval_in(r, env, sc)?;
                if matches!(b, CypherValue::Bool(false)) {
                    return Ok(CypherValue::Bool(false));
                }
                if a.is_null() || b.is_null() {
                    return Ok(CypherValue::Null);
                }
                CypherValue::Bool(scalar::is_truthy(&a) && scalar::is_truthy(&b))
            }
            ExprNode::Or(l, r) => {
                let a = self.eval_in(l, env, sc)?;
                if scalar::is_truthy(&a) {
                    return Ok(CypherValue::Bool(true));
                }
                let b = self.eval_in(r, env, sc)?;
                if scalar::is_truthy(&b) {
                    return Ok(CypherValue::Bool(true));
                }
                if a.is_null() || b.is_null() {
                    return Ok(CypherValue::Null);
                }
                CypherValue::Bool(false)
            }
            ExprNode::Xor(l, r) => {
                let a = self.eval_in(l, env, sc)?;
                let b = self.eval_in(r, env, sc)?;
                if a.is_null() || b.is_null() {
                    CypherValue::Null
                } else {
                    CypherValue::Bool(scalar::is_truthy(&a) ^ scalar::is_truthy(&b))
                }
            }
            ExprNode::Not(e) => scalar::not(&self.eval_in(e, env, sc)?),
            ExprNode::Neg(e) => scalar::negate(&self.eval_in(e, env, sc)?)?,
            ExprNode::IsNull(e) => CypherValue::Bool(self.eval_in(e, env, sc)?.is_null()),
            ExprNode::IsNotNull(e) => CypherValue::Bool(!self.eval_in(e, env, sc)?.is_null()),
            ExprNode::IsTyped {
                e,
                type_name,
                negated,
                not_null,
            } => {
                let v = self.eval_in(e, env, sc)?;
                CypherValue::Bool(
                    scalar::type_predicate_matches(&v, type_name, *not_null) != *negated,
                )
            }
            ExprNode::In { item, list } => {
                let it = self.eval_in(item, env, sc)?;
                let li = self.eval_in(list, env, sc)?;
                scalar::in_list(&it, &li)?
            }
            ExprNode::StringOp { op, l, r } => {
                let a = self.eval_in(l, env, sc)?;
                let b = self.eval_in(r, env, sc)?;
                scalar::string_op(&a, *op, &b)?
            }
            ExprNode::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(op) = operand {
                    let ov = self.eval_in(op, env, sc)?;
                    for (w, t) in whens {
                        let wv = self.eval_in(w, env, sc)?;
                        if matches!(scalar::cypher_eq(&ov, &wv), CypherValue::Bool(true)) {
                            return self.eval_in(t, env, sc);
                        }
                    }
                } else {
                    for (w, t) in whens {
                        let wv = self.eval_in(w, env, sc)?;
                        if scalar::is_truthy(&wv) {
                            return self.eval_in(t, env, sc);
                        }
                    }
                }
                match else_ {
                    Some(e) => self.eval_in(e, env, sc)?,
                    None => CypherValue::Null,
                }
            }
            ExprNode::ListLit(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.eval_in(it, env, sc)?);
                }
                CypherValue::List(out)
            }
            ExprNode::MapLit(entries) => {
                let mut map = std::collections::BTreeMap::new();
                for (k, e) in entries {
                    map.insert(k.clone(), self.eval_in(e, env, sc)?);
                }
                CypherValue::Map(map)
            }
            ExprNode::Func { name, kind, args } => self.eval_func(name, kind, args, env, sc)?,
            ExprNode::ListComp { list, filter, map } => {
                let lv = self.eval_in(list, env, sc)?;
                match lv {
                    CypherValue::List(items) => {
                        let mut out = Vec::new();
                        let depth = sc.locals.len();
                        sc.locals.push(CypherValue::Null);
                        let r = (|| -> Result<(), CypherError> {
                            for item in items {
                                sc.locals[depth] = item;
                                if let Some(f) = filter {
                                    let fv = self.eval_in(f, env, sc)?;
                                    if !scalar::is_truthy(&fv) {
                                        continue;
                                    }
                                }
                                let v = match map {
                                    Some(m) => self.eval_in(m, env, sc)?,
                                    None => sc.locals[depth].clone(),
                                };
                                out.push(v);
                            }
                            Ok(())
                        })();
                        sc.locals.truncate(depth);
                        r?;
                        CypherValue::List(out)
                    }
                    CypherValue::Null => CypherValue::Null,
                    other => {
                        return Err(CypherError::type_error(format!(
                            "list comprehension requires a list, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            ExprNode::Quantifier { kind, list, pred } => {
                let lv = self.eval_in(list, env, sc)?;
                let items = match lv {
                    CypherValue::List(l) => l,
                    CypherValue::Null => return Ok(CypherValue::Null),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "quantifier expects a list, got {}",
                            other.type_name()
                        )));
                    }
                };
                let depth = sc.locals.len();
                sc.locals.push(CypherValue::Null);
                let mut true_count = 0usize;
                let mut has_null = false;
                let mut has_false = false;
                let r = (|| -> Result<(), CypherError> {
                    for item in items {
                        sc.locals[depth] = item;
                        let v = self.eval_in(pred, env, sc)?;
                        match v {
                            CypherValue::Null => has_null = true,
                            _ if scalar::is_truthy(&v) => {
                                true_count += 1;
                                if matches!(kind, QuantifierKind::Any | QuantifierKind::None) {
                                    break;
                                }
                                if matches!(kind, QuantifierKind::Single) && true_count > 1 {
                                    break;
                                }
                            }
                            _ => {
                                has_false = true;
                                if matches!(kind, QuantifierKind::All) {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(())
                })();
                sc.locals.truncate(depth);
                r?;
                match kind {
                    QuantifierKind::All => {
                        if has_false {
                            CypherValue::Bool(false)
                        } else if has_null {
                            CypherValue::Null
                        } else {
                            CypherValue::Bool(true)
                        }
                    }
                    QuantifierKind::Any => {
                        if true_count > 0 {
                            CypherValue::Bool(true)
                        } else if has_null {
                            CypherValue::Null
                        } else {
                            CypherValue::Bool(false)
                        }
                    }
                    QuantifierKind::None => {
                        if true_count > 0 {
                            CypherValue::Bool(false)
                        } else if has_null {
                            CypherValue::Null
                        } else {
                            CypherValue::Bool(true)
                        }
                    }
                    QuantifierKind::Single => {
                        if true_count > 1 {
                            CypherValue::Bool(false)
                        } else if has_null {
                            CypherValue::Null
                        } else {
                            CypherValue::Bool(true_count == 1)
                        }
                    }
                }
            }
            ExprNode::Reduce { init, list, update } => {
                let lv = self.eval_in(list, env, sc)?;
                match lv {
                    CypherValue::List(items) => {
                        let iv = self.eval_in(init, env, sc)?;
                        let depth = sc.locals.len();
                        sc.locals.push(iv);
                        sc.locals.push(CypherValue::Null);
                        let r = (|| -> Result<CypherValue, CypherError> {
                            for item in items {
                                sc.locals[depth + 1] = item;
                                let nv = self.eval_in(update, env, sc)?;
                                sc.locals[depth] = nv;
                            }
                            Ok(sc.locals[depth].clone())
                        })();
                        sc.locals.truncate(depth);
                        r?
                    }
                    CypherValue::Null => CypherValue::Null,
                    other => {
                        return Err(CypherError::type_error(format!(
                            "reduce requires a list, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            ExprNode::MapProj {
                base,
                props,
                all,
                literal,
            } => {
                let b = self.eval_in(base, env, sc)?;
                let mut map = if *all {
                    scalar::all_properties_of(&b, graph)
                } else {
                    std::collections::BTreeMap::new()
                };
                for key in props {
                    let v = match &b {
                        CypherValue::NodeRef(_) | CypherValue::EdgeRef { .. } => {
                            scalar::property_of(&b, key, graph).unwrap_or(CypherValue::Null)
                        }
                        CypherValue::Null => CypherValue::Null,
                        other => scalar::get_property(other, key).unwrap_or(CypherValue::Null),
                    };
                    map.insert(key.clone(), v);
                }
                for (k, e) in literal {
                    map.insert(k.clone(), self.eval_in(e, env, sc)?);
                }
                CypherValue::Map(map)
            }
            ExprNode::Exists(sp) => self.run_sub(sp, SubMode::Exists, env, sc)?,
            ExprNode::CountSub(sp) => self.run_sub(sp, SubMode::Count, env, sc)?,
            ExprNode::CollectSub(sp) | ExprNode::PatternComp(sp) => {
                self.run_sub(sp, SubMode::Collect, env, sc)?
            }
        })
    }

    fn eval_func<'p>(
        &'p self,
        name: &str,
        kind: &FuncKind,
        args: &'p [ExprNode],
        env: &LaneEnv<'_, '_>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<CypherValue, CypherError> {
        let graph = env.graph();
        match kind {
            FuncKind::HasLabel(label) => {
                let v = self.eval_in(&args[0], env, sc)?;
                return scalar::has_label(&v, label, graph);
            }
            FuncKind::HasLabelDynamic => {
                let v = self.eval_in(&args[0], env, sc)?;
                let label = match self.eval_in(&args[1], env, sc)? {
                    CypherValue::String(s) => s,
                    CypherValue::Null => return Ok(CypherValue::Bool(false)),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "dynamic label expression must evaluate to STRING, got {}",
                            other.type_name()
                        )));
                    }
                };
                return scalar::has_label(&v, &label, graph);
            }
            FuncKind::CompositeKey => {
                let mut parts = Vec::with_capacity(args.len());
                for a in args {
                    let v = self.eval_in(a, env, sc)?;
                    let ik = match v {
                        CypherValue::Bool(b) => IndexKey::Bool(b),
                        CypherValue::Integer(i) => IndexKey::Int(i),
                        CypherValue::Float(f) => IndexKey::Float(f),
                        CypherValue::String(s) => IndexKey::String(s),
                        _ => return Ok(CypherValue::Null),
                    };
                    parts.push(ik);
                }
                return Ok(match IndexKey::composite_encode(&parts) {
                    IndexKey::String(s) => CypherValue::String(s),
                    _ => CypherValue::Null,
                });
            }
            FuncKind::File(ctx) | FuncKind::LineNumber(ctx) => {
                if !args.is_empty() {
                    return Err(CypherError::type_error(format!(
                        "{name}() expected 0 arguments, got {}",
                        args.len()
                    )));
                }
                let idx = ctx.and_then(|p| match env.value(p) {
                    ValueRef::Int(i) if i >= 0 => Some(i as usize),
                    _ => None,
                });
                let c = idx.and_then(crate::vex::ops::load_csv::get_csv_context);
                return Ok(match (kind, c) {
                    (FuncKind::File(_), Some(c)) => CypherValue::String(c.file),
                    (FuncKind::LineNumber(_), Some(c)) => CypherValue::Integer(c.line),
                    _ => CypherValue::Null,
                });
            }
            FuncKind::Aggregate => {
                return Err(CypherError::semantic(format!(
                    "aggregate function {name}() is not allowed here"
                ))
                .with_status(crate::status::GqlStatus::InvalidAggregation));
            }
            _ => {}
        }
        let mut arg_vals: Vec<CypherValue> = Vec::with_capacity(args.len());
        for a in args {
            arg_vals.push(self.eval_in(a, env, sc)?);
        }
        match kind {
            FuncKind::Apoc => {
                if let Some(r) = crate::apoc::eval(name, &arg_vals) {
                    return r;
                }
                if let Some(r) = crate::apoc::eval_graph(name, &arg_vals, graph) {
                    return r;
                }
                return Err(CypherError::semantic(format!(
                    "Unknown function: {name} (not in Traverse's APOC compatibility subset)"
                ))
                .with_status(crate::status::GqlStatus::UnknownFunction));
            }
            FuncKind::Gds | FuncKind::AsNode => {
                if let Some(r) = crate::gds_catalog::eval_fn(name, &arg_vals, graph) {
                    return r;
                }
                return Err(CypherError::semantic(format!(
                    "Unknown function: {name} (not in Traverse's GDS compatibility subset)"
                ))
                .with_status(crate::status::GqlStatus::UnknownFunction));
            }
            FuncKind::StartNode | FuncKind::EndNode => {
                if let Some(
                    CypherValue::Edge { source, target, .. }
                    | CypherValue::EdgeRef { source, target, .. },
                ) = arg_vals.first()
                {
                    return Ok(CypherValue::NodeRef(if *kind == FuncKind::StartNode {
                        *source
                    } else {
                        *target
                    }));
                }
            }
            FuncKind::Id => match arg_vals.first() {
                Some(CypherValue::NodeRef(id)) => return Ok(CypherValue::Integer(id.0 as i64)),
                Some(CypherValue::EdgeRef { id, .. }) => {
                    return Ok(CypherValue::Integer(id.0 as i64));
                }
                _ => {}
            },
            FuncKind::ElementId => match arg_vals.first() {
                Some(CypherValue::NodeRef(id)) => return Ok(CypherValue::String(id.0.to_string())),
                Some(CypherValue::EdgeRef { id, .. }) => {
                    return Ok(CypherValue::String(id.0.to_string()));
                }
                _ => {}
            },
            FuncKind::Type => {
                if let Some(CypherValue::EdgeRef { edge_type_id, .. }) = arg_vals.first() {
                    return Ok(CypherValue::String(
                        graph.strings().resolve(edge_type_id.0).to_string(),
                    ));
                }
            }
            FuncKind::Labels => {
                if let Some(val) = arg_vals.first() {
                    match val {
                        CypherValue::Node { id, .. } | CypherValue::NodeRef(id)
                            if id.0 < super::super::values::VIRTUAL_ID_BASE
                                && graph.get_node(*id).is_err() =>
                        {
                            return Err(scalar::deleted_entity());
                        }
                        CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. }
                            if id.0 < super::super::values::VIRTUAL_ID_BASE
                                && graph.get_edge(*id).is_err() =>
                        {
                            return Err(scalar::deleted_entity());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let resolved: Vec<CypherValue> = arg_vals
            .into_iter()
            .map(|v| resolve_ref(v, graph))
            .collect();
        crate::functions::call_function(name, &resolved)
    }

    /// Runs a subquery for one lane and consumes its rows as `mode` says.
    fn run_sub<'p>(
        &'p self,
        sp: &'p Arc<SubPlan>,
        mode: SubMode,
        env: &LaneEnv<'_, '_>,
        sc: &mut EvalScratch<'p>,
    ) -> Result<CypherValue, CypherError> {
        // the seed row, from the outer lane and the locals
        let mut row = Vec::with_capacity(sp.seeds.len());
        for (src, _) in &sp.seeds {
            row.push(match src {
                SeedSource::Outer(p) => env.value(*p).to_owned(),
                SeedSource::Local(k) => sc
                    .locals
                    .get(*k as usize)
                    .cloned()
                    .unwrap_or(CypherValue::Null),
            });
        }
        let key = Arc::as_ptr(sp) as usize;
        let exec = match sc.subs.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(SubExec {
                op: build_operator(&sp.root)?,
                rs: ResultSet::new(&sp.widths),
            }),
        };
        exec.op.seed(vec![row]);
        exec.op.reset();
        let mut view = ReadOnlyView(env.graph());
        let mut ecx = ExecCtx::new(&mut view);
        let out_pos = sp.out.first().map(|(_, p)| *p);
        match mode {
            SubMode::Exists => {
                while exec.op.next(&mut exec.rs, &mut ecx)? {
                    if exec.rs.row_count(&sp.out_groups) > 0 {
                        return Ok(CypherValue::Bool(true));
                    }
                }
                Ok(CypherValue::Bool(false))
            }
            SubMode::Count => {
                let mut n = 0usize;
                while exec.op.next(&mut exec.rs, &mut ecx)? {
                    n += exec.rs.row_count(&sp.out_groups);
                }
                Ok(CypherValue::Integer(n as i64))
            }
            SubMode::Collect => {
                let mut out = Vec::new();
                while exec.op.next(&mut exec.rs, &mut ecx)? {
                    if let Some(p) = out_pos {
                        let rs = &exec.rs;
                        rs.for_each_row(&sp.out_groups, |lanes| {
                            out.push(rs.value_at(p, lanes).to_owned());
                            Ok(())
                        })?;
                    } else {
                        // no column: one null per row
                        for _ in 0..exec.rs.row_count(&sp.out_groups) {
                            out.push(CypherValue::Null);
                        }
                    }
                }
                Ok(CypherValue::List(out))
            }
        }
    }
}

/// Evaluation of one lane: either a position of the evaluation group or an explicit lane
/// assignment per group.
enum LaneEnv<'c, 'a> {
    Pos(&'c EvalCtx<'a>, usize),
    Row(&'c RowCtx<'a>),
}

impl<'c, 'a> LaneEnv<'c, 'a> {
    #[inline]
    fn graph(&self) -> &'a dyn GraphView {
        match self {
            LaneEnv::Pos(cx, _) => cx.graph,
            LaneEnv::Row(cx) => cx.graph,
        }
    }

    #[inline]
    fn value(&self, pos: DataPos) -> ValueRef<'a> {
        match self {
            LaneEnv::Pos(cx, i) => cx.lane(pos, *i),
            LaneEnv::Row(cx) => cx.rs.value_at(pos, cx.lanes),
        }
    }
}

/// An explicit lane per group.
struct RowCtx<'a> {
    rs: &'a ResultSet,
    graph: &'a dyn GraphView,
    lanes: &'a [usize],
}

#[inline]
fn push_id(b: &mut VectorBuilder, id: u64, as_string: bool) {
    if as_string {
        b.push_str(&id.to_string());
    } else {
        b.push_int(id as i64);
    }
}

/// A query parameter, or the missing-parameter error.
pub fn param(name: &str) -> Result<CypherValue, CypherError> {
    crate::query::param(name).ok_or_else(|| {
        CypherError::semantic(format!("Expected parameter(s): {name}"))
            .with_status(crate::status::GqlStatus::InvalidParameter)
    })
}

/// A compiled expression's value when it is a literal or parameter, without a chunk.
pub fn eval_constant(
    expr: &CompiledExpr,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    let rs = ResultSet::default();
    let mut sc = EvalScratch::new();
    expr.eval_scalar(&rs, graph, &mut sc)
}

/// Materializes an entity value for the function table (nodes and edges by reference).
pub fn materialize_entity(v: ValueRef<'_>, graph: &dyn GraphView) -> CypherValue {
    match v {
        ValueRef::Node(id) => match graph.get_node(id) {
            Ok(n) => node_to_cypher_value(&n, graph),
            Err(_) => CypherValue::Null,
        },
        ValueRef::Edge { id, .. } => match graph.get_edge(id) {
            Ok(e) => edge_to_cypher_value(&e, graph),
            Err(_) => v.to_owned(),
        },
        other => other.to_owned(),
    }
}

/// Keeps the scalar comparison in the evaluator's public surface for operators that compare
/// values directly.
pub fn compare_scalar(a: &CypherValue, op: CmpOp, b: &CypherValue) -> CypherValue {
    scalar::eval_comparison(a, op, b)
}
