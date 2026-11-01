//! Frame-based records for the executor: a [`Frame`] is a `Vec<SlotValue>` indexed through a
//! [`SymbolTable`] (name -> slot). [`SlotValue`] (~40 bytes) keeps hot types (NodeRef, EdgeRef,
//! Integer, Float, Bool) inline and boxes cold ones (String, List, Map, Path, ...) in `Heap`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut, Index, IndexMut};

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeId, EdgeTypeId, NodeId};

use crate::ast::{Expr, ProjectionItem};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

/// Slot type inferred by the planner. Metadata only for now; typed compact storage (`Vec<NodeId>`
/// instead of `Vec<CypherValue>`) could build on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    /// Node reference (scan, expand target, path endpoints).
    NodeRef,
    /// Single-hop relationship reference.
    EdgeRef,
    /// Full node (CREATE).
    Node,
    /// Full edge (CREATE).
    Edge,
    /// Path (shortestPath, path binding).
    Path,
    /// Float (avg(), float expressions, weighted cost).
    Float,
    /// List (variable-length rel_var, collect()).
    List,
    /// Unknown / mixed (UNWIND alias, projections, parameters).
    Dynamic,
}

/// Compact per-slot value of a [`Frame`]. Hot variants (NodeRef, EdgeRef,
/// Integer, Float, Bool) are inline, no allocation; cold ones (String, List,
/// Map, Node, Edge, Path, temporals) are boxed in `Heap`. ~40 bytes vs ~80 for
/// [`CypherValue`], halving frame memory.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotValue {
    /// Null / unset.
    Null,
    /// Boolean value.
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float.
    Float(f64),
    /// Lightweight node reference.
    NodeRef(NodeId),
    /// Lightweight edge reference.
    EdgeRef {
        /// Edge id.
        id: EdgeId,
        /// Edge type id.
        edge_type_id: EdgeTypeId,
        /// Source node.
        source: NodeId,
        /// Target node.
        target: NodeId,
    },
    /// Inline single-hop path, no heap allocation. `outgoing`: edge goes start->end, else end->start.
    SingleHopPath {
        /// Start node.
        start: NodeId,
        /// End node.
        end: NodeId,
        /// Edge id.
        edge_id: EdgeId,
        /// Edge type id.
        edge_type: EdgeTypeId,
        /// `true`: edge.source = start, edge.target = end; else reversed.
        outgoing: bool,
    },
    /// Boxed CypherValue for cold types (String, List, Map, Node, Edge, Path, temporals).
    Heap(Box<CypherValue>),
}

impl SlotValue {
    /// Null / unset?
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, SlotValue::Null)
    }

    /// -> [`CypherValue`]: inline variants copy cheaply, Heap clones the box.
    #[inline(never)]
    pub fn to_cypher_value(&self) -> CypherValue {
        match self {
            SlotValue::Null => CypherValue::Null,
            SlotValue::Bool(b) => CypherValue::Bool(*b),
            SlotValue::Integer(i) => CypherValue::Integer(*i),
            SlotValue::Float(f) => CypherValue::Float(*f),
            SlotValue::NodeRef(id) => CypherValue::NodeRef(*id),
            SlotValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => CypherValue::EdgeRef {
                id: *id,
                edge_type_id: *edge_type_id,
                source: *source,
                target: *target,
            },
            SlotValue::SingleHopPath {
                start,
                end,
                edge_id,
                edge_type,
                outgoing,
            } => {
                let (source, target) = if *outgoing {
                    (*start, *end)
                } else {
                    (*end, *start)
                };
                CypherValue::Path {
                    nodes: vec![CypherValue::NodeRef(*start), CypherValue::NodeRef(*end)],
                    edges: vec![CypherValue::EdgeRef {
                        id: *edge_id,
                        edge_type_id: *edge_type,
                        source,
                        target,
                    }],
                }
            }
            SlotValue::Heap(cv) => cv.as_ref().clone(),
        }
    }

    /// Owned conversion - like [`Self::to_cypher_value`] but consumes the
    /// slot, so heap values move instead of cloning.
    pub fn into_cypher_value(self) -> CypherValue {
        match self {
            SlotValue::Heap(cv) => *cv,
            other => other.to_cypher_value(),
        }
    }

    /// [`PropertyValue`] -> [`SlotValue`] directly, skipping [`CypherValue`] for hot types.
    #[inline(never)]
    #[recursive::recursive]
    pub fn from_property(pv: PropertyValue) -> SlotValue {
        match pv {
            PropertyValue::Null => SlotValue::Null,
            PropertyValue::Bool(b) => SlotValue::Bool(b),
            PropertyValue::Int(i) => SlotValue::Integer(i),
            PropertyValue::Float(f) => SlotValue::Float(f),
            other => SlotValue::Heap(Box::new(CypherValue::from_property(&other))),
        }
    }

    /// Type name for error messages.
    #[inline(never)]
    pub fn type_name(&self) -> &'static str {
        match self {
            SlotValue::Null => "NULL",
            SlotValue::Bool(_) => "BOOLEAN",
            SlotValue::Integer(_) => "INTEGER",
            SlotValue::Float(_) => "FLOAT",
            SlotValue::NodeRef(_) => "NODE",
            SlotValue::EdgeRef { .. } => "RELATIONSHIP",
            SlotValue::SingleHopPath { .. } => "PATH",
            SlotValue::Heap(cv) => cv.type_name(),
        }
    }
}

impl From<CypherValue> for SlotValue {
    #[inline(never)]
    fn from(cv: CypherValue) -> Self {
        match cv {
            CypherValue::Null => SlotValue::Null,
            CypherValue::Bool(b) => SlotValue::Bool(b),
            CypherValue::Integer(i) => SlotValue::Integer(i),
            CypherValue::Float(f) => SlotValue::Float(f),
            CypherValue::NodeRef(id) => SlotValue::NodeRef(id),
            CypherValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => SlotValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            },
            other => SlotValue::Heap(Box::new(other)),
        }
    }
}

/// Variable name -> contiguous slot index of a [`Frame`].
#[derive(Clone)]
pub struct SymbolTable {
    names: Vec<String>,
    // FxHashMap: `slot(name)` runs for every Variable/Property reference on every row; FxHash on
    // short names is several times faster than SipHash
    lookup: rustc_hash::FxHashMap<String, usize>,
    types: Vec<SlotType>,
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolTable {
    /// Empty table.
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            lookup: rustc_hash::FxHashMap::default(),
            types: Vec::new(),
        }
    }

    /// Slot count.
    pub fn width(&self) -> usize {
        self.names.len()
    }

    /// Get-or-allocate a slot for `name` as [`SlotType::Dynamic`].
    pub fn add(&mut self, name: String) -> usize {
        self.add_typed(name, SlotType::Dynamic)
    }

    /// Get-or-allocate a slot for `name` with a known type; an existing slot keeps its type.
    pub fn add_typed(&mut self, name: String, ty: SlotType) -> usize {
        if let Some(&slot) = self.lookup.get(&name) {
            return slot;
        }
        let slot = self.names.len();
        self.lookup.insert(name.clone(), slot);
        self.names.push(name);
        self.types.push(ty);
        slot
    }

    /// Inferred type of a slot.
    pub fn slot_type(&self, slot: usize) -> SlotType {
        self.types[slot]
    }

    /// Slot of `name`, if registered.
    pub fn slot(&self, name: &str) -> Option<usize> {
        self.lookup.get(name).copied()
    }

    /// Name of a slot.
    pub fn name(&self, slot: usize) -> &str {
        &self.names[slot]
    }
}

// -- Thread-local frame pool ------------------------------------------

const MAX_POOL_SIZE: usize = 2048;

thread_local! {
    static FRAME_POOL: RefCell<Vec<Vec<SlotValue>>> = const { RefCell::new(Vec::new()) };
}

/// Pooled Vec filled with `Null` (for `new_frame`).
#[inline(never)]
fn take_from_pool(width: usize) -> Vec<SlotValue> {
    FRAME_POOL.with(|pool| {
        if let Some(mut vec) = pool.borrow_mut().pop() {
            // cleared in return_to_pool (len 0, capacity kept); resize writes Null into each slot
            vec.resize(width, SlotValue::Null);
            vec
        } else {
            vec![SlotValue::Null; width]
        }
    })
}

/// Pooled Vec for cloning; the caller overwrites every slot via `extend_from_slice`, so no zeroing.
#[inline(never)]
fn take_from_pool_for_clone() -> Vec<SlotValue> {
    FRAME_POOL.with(|pool| pool.borrow_mut().pop().unwrap_or_default())
}

#[inline(never)]
fn return_to_pool(mut vec: Vec<SlotValue>) {
    // drops any Heap boxes
    vec.clear();
    FRAME_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < MAX_POOL_SIZE {
            pool.push(vec);
        }
        // else drop the vec (capacity freed)
    });
}

// -- Frame newtype ----------------------------------------------------

/// Record as a contiguous vector of compact slot values, indexed by [`SymbolTable`]. Pooled per
/// thread: `Drop` returns the `Vec` to a thread-local free list; `new_frame`/`Clone` reuse pooled
/// Vecs.
pub struct Frame {
    slots: Vec<SlotValue>,
}

impl Frame {
    /// Frame from an existing Vec (no pool).
    #[inline(always)]
    pub fn from_vec(slots: Vec<SlotValue>) -> Self {
        Self { slots }
    }
}

impl Deref for Frame {
    type Target = [SlotValue];
    #[inline(always)]
    fn deref(&self) -> &[SlotValue] {
        &self.slots
    }
}

impl DerefMut for Frame {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [SlotValue] {
        &mut self.slots
    }
}

impl Index<usize> for Frame {
    type Output = SlotValue;
    #[inline(always)]
    fn index(&self, idx: usize) -> &SlotValue {
        &self.slots[idx]
    }
}

impl IndexMut<usize> for Frame {
    #[inline(always)]
    fn index_mut(&mut self, idx: usize) -> &mut SlotValue {
        &mut self.slots[idx]
    }
}

impl Default for Frame {
    #[inline(always)]
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl PartialEq for Frame {
    fn eq(&self, other: &Self) -> bool {
        self.slots == other.slots
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.slots.fmt(f)
    }
}

impl Clone for Frame {
    #[inline(never)]
    fn clone(&self) -> Self {
        // fast path: cleared pooled Vec (len 0, capacity kept) + extend_from_slice; no zeroing, no
        // dropping old slots
        let mut vec = take_from_pool_for_clone();
        vec.extend_from_slice(&self.slots);
        Self { slots: vec }
    }
}

impl Drop for Frame {
    #[inline(never)]
    fn drop(&mut self) {
        if self.slots.capacity() > 0 {
            let vec = std::mem::take(&mut self.slots);
            return_to_pool(vec);
        }
    }
}

/// New frame, every slot `Null`.
pub fn new_frame(symtab: &SymbolTable) -> Frame {
    Frame {
        slots: take_from_pool(symtab.width()),
    }
}

/// [`Frame`] -> `HashMap` record (cold-path interop).
pub fn frame_to_record(frame: &Frame, symtab: &SymbolTable) -> HashMap<String, CypherValue> {
    let mut rec = HashMap::with_capacity(symtab.width());
    for i in 0..symtab.width() {
        if !frame[i].is_null() {
            rec.insert(symtab.name(i).to_string(), frame[i].to_cypher_value());
        }
    }
    rec
}

/// [`frame_to_record`] for the named slots only: nested constructs (pattern comprehensions) know
/// which outer variables they use, and converting the rest would clone unrelated heap values
/// (paths, collected lists) per evaluation.
pub fn frame_to_record_filtered(
    frame: &Frame,
    symtab: &SymbolTable,
    keep: &std::collections::HashSet<String>,
) -> HashMap<String, CypherValue> {
    let mut rec = HashMap::with_capacity(keep.len());
    for name in keep {
        if let Some(i) = symtab.slot(name)
            && !frame[i].is_null()
        {
            rec.insert(name.clone(), frame[i].to_cypher_value());
        }
    }
    rec
}

/// Temporary [`SymbolTable`] from a record's keys (cold path).
pub fn record_to_symtab(rec: &HashMap<String, CypherValue>) -> SymbolTable {
    let mut st = SymbolTable::new();
    for k in rec.keys() {
        st.add(k.clone());
    }
    st
}

/// [`SymbolTable`] over the union of keys of several records (cold path).
pub fn records_to_symtab(recs: &[HashMap<String, CypherValue>]) -> SymbolTable {
    let mut st = SymbolTable::new();
    for rec in recs {
        for k in rec.keys() {
            if st.slot(k).is_none() {
                st.add(k.clone());
            }
        }
    }
    st
}

/// `HashMap` record -> [`Frame`] (cold-path interop).
pub fn record_to_frame(rec: &HashMap<String, CypherValue>, symtab: &SymbolTable) -> Frame {
    let mut frame = new_frame(symtab);
    for (k, v) in rec {
        if let Some(slot) = symtab.slot(k) {
            frame[slot] = SlotValue::from(v.clone());
        }
    }
    frame
}

/// Consumed Record -> Frame, moving values instead of cloning.
pub fn record_to_frame_move(mut rec: HashMap<String, CypherValue>, symtab: &SymbolTable) -> Frame {
    let mut frame = new_frame(symtab);
    for (k, v) in rec.drain() {
        if let Some(slot) = symtab.slot(&k) {
            frame[slot] = SlotValue::from(v);
        }
    }
    frame
}

// -- Symbol table construction -----------------------------------------

/// [`SymbolTable`] for a plan: every variable name that may appear at runtime.
pub fn build_symbol_table(plan: &LogicalPlan) -> SymbolTable {
    let mut symtab = SymbolTable::new();
    collect_plan_variables(plan, &mut symtab);
    symtab
}

fn projection_name_for_symtab(item: &ProjectionItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    if let Some(ref source) = item.source_text {
        return source.clone();
    }
    expr_name_simple(&item.expr)
}

/// Simplified expr_name (like executor::expr_name, without the circular dependency).
#[recursive::recursive]
fn expr_name_simple(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(base, key) => format!("{}.{}", expr_name_simple(base), key),
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => {
            if args.is_empty() {
                if *distinct {
                    format!("{name}(DISTINCT *)")
                } else {
                    format!("{name}(*)")
                }
            } else {
                let arg_names: Vec<String> = args.iter().map(expr_name_simple).collect();
                if *distinct {
                    format!("{name}(DISTINCT {})", arg_names.join(", "))
                } else {
                    format!("{name}({})", arg_names.join(", "))
                }
            }
        }
        Expr::Literal(lit) => match lit {
            crate::ast::Literal::Integer(n) => n.to_string(),
            crate::ast::Literal::Float(f) => f.to_string(),
            crate::ast::Literal::String(s) => format!("\"{s}\""),
            crate::ast::Literal::Bool(b) => b.to_string(),
            crate::ast::Literal::Null => "null".to_string(),
        },
        Expr::IsNull(inner) => format!("{} IS NULL", expr_name_simple(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_name_simple(inner)),
        Expr::BinaryOp(l, op, r) => {
            let op_str = match op {
                crate::ast::BinOp::Add => "+",
                crate::ast::BinOp::Sub => "-",
                crate::ast::BinOp::Mul => "*",
                crate::ast::BinOp::Div => "/",
                crate::ast::BinOp::Mod => "%",
                crate::ast::BinOp::Pow => "^",
                crate::ast::BinOp::Concat => "||",
                crate::ast::BinOp::And => "AND",
                crate::ast::BinOp::Or => "OR",
                crate::ast::BinOp::Xor => "XOR",
            };
            format!("{} {} {}", expr_name_simple(l), op_str, expr_name_simple(r))
        }
        Expr::UnaryOp(op, inner) => {
            let op_str = match op {
                crate::ast::UnaryOp::Not => "NOT",
                crate::ast::UnaryOp::Neg => "-",
            };
            format!("{op_str} {}", expr_name_simple(inner))
        }
        Expr::Comparison(base, pairs) => {
            let mut s = expr_name_simple(base);
            for (op, e) in pairs {
                let op_str = match op {
                    crate::ast::CmpOp::Eq => "=",
                    crate::ast::CmpOp::Neq => "<>",
                    crate::ast::CmpOp::Lt => "<",
                    crate::ast::CmpOp::Gt => ">",
                    crate::ast::CmpOp::Lte => "<=",
                    crate::ast::CmpOp::Gte => ">=",
                };
                s = format!("{s} {op_str} {}", expr_name_simple(e));
            }
            s
        }
        _ => "expr".to_string(),
    }
}

/// Adds every variable of a [`LogicalPlan`] to `symtab`, recursively.
#[recursive::recursive]
pub(crate) fn collect_plan_variables(plan: &LogicalPlan, symtab: &mut SymbolTable) {
    match plan {
        // -- Leaf nodes -----------------------------------------------
        LogicalPlan::ContextSeed => {}
        LogicalPlan::Scan { variable, .. } => {
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
        }
        LogicalPlan::IndexScan { variable, .. }
        | LogicalPlan::IndexRangeScan { variable, .. }
        | LogicalPlan::PointIndexScan { variable, .. }
        | LogicalPlan::TextIndexScan { variable, .. } => {
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
        }
        LogicalPlan::IndexLookupJoin {
            input, variable, ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
        }
        LogicalPlan::VectorSearchScan {
            input,
            variable,
            query_vector,
            index_where,
            limit,
            score_var,
            ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
            if let Some(sv) = score_var {
                symtab.add_typed(sv.clone(), SlotType::Float);
            }
            collect_expr_variables(query_vector, symtab);
            if let Some(w) = index_where {
                collect_expr_variables(w, symtab);
            }
            collect_expr_variables(limit, symtab);
        }
        LogicalPlan::BindQppGroups {
            input,
            src_var,
            dst_var,
            ..
        } => {
            collect_plan_variables(input, symtab);
            if let Some(v) = src_var {
                symtab.add_typed(v.clone(), SlotType::List);
            }
            if let Some(v) = dst_var {
                symtab.add_typed(v.clone(), SlotType::List);
            }
        }
        LogicalPlan::BindVarLists { input, groups } => {
            collect_plan_variables(input, symtab);
            for (v, _) in groups {
                symtab.add_typed(v.clone(), SlotType::List);
            }
        }
        LogicalPlan::NodeById {
            variable, id_expr, ..
        } => {
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
            collect_expr_variables(id_expr, symtab);
        }
        LogicalPlan::NodeByIdRange { variable, .. } => {
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
        }
        LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            ..
        } => {
            symtab.add_typed(from_var.clone(), SlotType::NodeRef);
            if !rel_var.is_empty() {
                symtab.add_typed(rel_var.clone(), SlotType::EdgeRef);
            }
            symtab.add_typed(to_var.clone(), SlotType::NodeRef);
        }
        LogicalPlan::ScanFilterProject {
            variable,
            items,
            predicate,
            ..
        } => {
            symtab.add_typed(variable.clone(), SlotType::NodeRef);
            for item in items {
                symtab.add(projection_name_for_symtab(item));
                collect_expr_variables(&item.expr, symtab);
            }
            if let Some(pred) = predicate {
                collect_expr_variables(pred, symtab);
            }
        }
        LogicalPlan::EmptyRow
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::CreateVectorIndex { .. }
        | LogicalPlan::CreateFulltextIndex { .. }
        | LogicalPlan::CreatePointIndex { .. }
        | LogicalPlan::CreateTextIndex { .. }
        | LogicalPlan::DropIndex { .. }
        | LogicalPlan::AnalyzeGraph { .. }
        | LogicalPlan::CreateConstraint { .. }
        | LogicalPlan::DropConstraint { .. }
        | LogicalPlan::CreateEdgeIndex { .. }
        | LogicalPlan::DropEdgeIndex { .. } => {}

        LogicalPlan::ShowIndexes | LogicalPlan::ShowConstraints => {
            symtab.add("name".to_string());
            symtab.add("type".to_string());
            symtab.add("entityType".to_string());
            symtab.add("labelsOrTypes".to_string());
            symtab.add("properties".to_string());
        }
        LogicalPlan::ShowFunctions => {
            symtab.add("name".to_string());
            symtab.add("category".to_string());
            symtab.add("description".to_string());
            symtab.add("signature".to_string());
            symtab.add("aggregating".to_string());
        }
        LogicalPlan::ShowProcedures => {
            symtab.add("name".to_string());
            symtab.add("description".to_string());
            symtab.add("signature".to_string());
            symtab.add("mode".to_string());
        }
        LogicalPlan::TransactionCommand { input, command, .. } => {
            collect_plan_variables(input, symtab);
            for column in command.columns() {
                symtab.add((*column).to_string());
            }
        }
        LogicalPlan::ShowSettings => {
            symtab.add("name".to_string());
            symtab.add("value".to_string());
            symtab.add("isDynamic".to_string());
            symtab.add("defaultValue".to_string());
            symtab.add("description".to_string());
        }

        // -- Expand variants ------------------------------------------
        LogicalPlan::ExpandOut {
            input,
            rel_var,
            to,
            range,
            ..
        }
        | LogicalPlan::ExpandIn {
            input,
            rel_var,
            to,
            range,
            ..
        }
        | LogicalPlan::ExpandBoth {
            input,
            rel_var,
            to,
            range,
            ..
        } => {
            collect_plan_variables(input, symtab);
            let rel_ty = if matches!(range, None | Some((Some(1), Some(1)))) {
                SlotType::EdgeRef
            } else {
                SlotType::List
            };
            symtab.add_typed(rel_var.clone(), rel_ty);
            symtab.add_typed(to.clone(), SlotType::NodeRef);
        }

        LogicalPlan::DistinctExpand { input, to, .. } => {
            collect_plan_variables(input, symtab);
            symtab.add_typed(to.clone(), SlotType::NodeRef);
        }

        // -- Pass-through (single input, no new variables) ------------
        LogicalPlan::Filter { input, predicate } => {
            collect_plan_variables(input, symtab);
            collect_expr_variables(predicate, symtab);
        }
        LogicalPlan::Sort { input, items } => {
            collect_plan_variables(input, symtab);
            for item in items {
                collect_expr_variables(&item.expr, symtab);
            }
        }
        LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Eager { input } => {
            collect_plan_variables(input, symtab);
        }
        LogicalPlan::SetProperty { input, .. } | LogicalPlan::RemoveOp { input, .. } => {
            collect_plan_variables(input, symtab);
        }
        LogicalPlan::Delete { input, .. } => {
            collect_plan_variables(input, symtab);
        }

        // -- Projection -----------------------------------------------
        LogicalPlan::Project {
            input,
            items,
            where_clause,
            ..
        } => {
            collect_plan_variables(input, symtab);
            for item in items {
                symtab.add(projection_name_for_symtab(item));
                collect_expr_variables(&item.expr, symtab);
            }
            if let Some(wc) = where_clause {
                collect_expr_variables(wc, symtab);
            }
        }

        // -- Aggregation ----------------------------------------------
        LogicalPlan::Aggregate { input, keys, aggs } => {
            collect_plan_variables(input, symtab);
            for item in keys {
                symtab.add(projection_name_for_symtab(item));
                collect_expr_variables(&item.expr, symtab);
            }
            for item in aggs {
                symtab.add(projection_name_for_symtab(item));
                collect_expr_variables(&item.expr, symtab);
            }
        }

        // -- Unwind ---------------------------------------------------
        LogicalPlan::Unwind {
            input, alias, expr, ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add(alias.clone());
            collect_expr_variables(expr, symtab);
        }
        LogicalPlan::ForEach {
            input,
            variable,
            list,
            body,
        } => {
            collect_plan_variables(input, symtab);
            // the loop variable and body bindings share the query's one symbol table (the executor
            // seeds the body's leaf with frames sized to it)
            symtab.add(variable.clone());
            collect_expr_variables(list, symtab);
            collect_plan_variables(body, symtab);
        }
        // runtime-only leaf with pre-built frames, already sized to the symbol table: nothing to
        // collect
        LogicalPlan::SeededRows(_) => {}

        LogicalPlan::LoadCsv {
            input,
            row_alias,
            url,
            ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add(row_alias.clone());
            // hidden slot with the index into the LOAD CSV context stack, so `file()` /
            // `linenumber()` find the current row's entry; the double underscore keeps it out of
            // projections
            symtab.add(crate::operators::load_csv::CSV_CTX_SLOT.to_string());
            collect_expr_variables(url, symtab);
        }

        // -- Create ---------------------------------------------------
        LogicalPlan::CreateNode {
            input, variable, ..
        } => {
            collect_plan_variables(input, symtab);
            if let Some(var) = variable {
                symtab.add_typed(var.clone(), SlotType::Node);
            }
        }
        LogicalPlan::CreateRel {
            input, variable, ..
        } => {
            collect_plan_variables(input, symtab);
            if let Some(var) = variable {
                symtab.add_typed(var.clone(), SlotType::Edge);
            }
        }

        // -- Two-input nodes ------------------------------------------
        LogicalPlan::OptionalMatch {
            input,
            match_plan,
            new_variables,
        } => {
            collect_plan_variables(input, symtab);
            collect_plan_variables(match_plan, symtab);
            for var in new_variables {
                symtab.add(var.clone());
            }
        }
        LogicalPlan::CrossProduct { left, right }
        | LogicalPlan::HashJoin { left, right, .. }
        | LogicalPlan::Union { left, right, .. } => {
            collect_plan_variables(left, symtab);
            collect_plan_variables(right, symtab);
        }

        // -- Path binding ---------------------------------------------
        LogicalPlan::BindPath {
            input, variable, ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add_typed(variable.clone(), SlotType::Path);
        }
        LogicalPlan::BindPathEndpoints {
            input,
            from_var,
            to_var,
            ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add_typed(from_var.clone(), SlotType::NodeRef);
            symtab.add_typed(to_var.clone(), SlotType::NodeRef);
        }
        LogicalPlan::BindEdgeEndpoints {
            input,
            from_var,
            to_var,
            ..
        } => {
            collect_plan_variables(input, symtab);
            symtab.add_typed(from_var.clone(), SlotType::NodeRef);
            symtab.add_typed(to_var.clone(), SlotType::NodeRef);
        }

        // -- Inline procedure subquery --------------------------------
        LogicalPlan::CallSubquery {
            input,
            body,
            return_aliases,
            ..
        } => {
            collect_plan_variables(input, symtab);
            // the body binds its own variables via exec_subquery; only the names its RETURN
            // promises extend the outer record
            collect_plan_variables(body, symtab);
            for a in return_aliases {
                symtab.add(a.clone());
            }
        }

        LogicalPlan::CallInTransactions {
            input,
            body,
            report_status,
            return_aliases,
            ..
        } => {
            collect_plan_variables(input, symtab);
            // the body shares the symbol table; the executor seeds its EmptyRow leaf with per-batch
            // frames (write-capable path)
            collect_plan_variables(body, symtab);
            for a in return_aliases {
                symtab.add(a.clone());
            }
            if let Some(rs) = report_status {
                symtab.add(rs.clone());
            }
        }

        // -- Procedure call -------------------------------------------
        LogicalPlan::CallProcedure {
            input,
            procedure_name,
            yield_columns,
            ..
        } => {
            collect_plan_variables(input, symtab);
            if yield_columns.is_empty() {
                // no YIELD: every procedure output, from the registry first, then the built-in
                // schema procedures (db.labels / propertyKeys / relationshipTypes), so `CALL
                // db.labels()` without YIELD gets a slot
                if let Some(proc) = crate::procedure::get_procedure(procedure_name) {
                    for out in &proc.outputs {
                        symtab.add(out.name.clone());
                    }
                } else if let Some(cols) =
                    crate::procedure::builtin_schema_proc_columns(procedure_name)
                {
                    for c in cols {
                        symtab.add((*c).to_string());
                    }
                }
            } else {
                for (output_name, bound_var) in yield_columns {
                    symtab.add(bound_var.clone());
                    if output_name != bound_var {
                        symtab.add(output_name.clone());
                    }
                }
            }
        }

        // -- Shortest path --------------------------------------------
        LogicalPlan::ShortestPath {
            input,
            path_var,
            rel_var,
            ..
        } => {
            collect_plan_variables(input, symtab);
            if let Some(pv) = path_var {
                symtab.add_typed(pv.clone(), SlotType::Path);
            }
            if let Some(rv) = rel_var {
                symtab.add_typed(rv.clone(), SlotType::List);
            }
        }

        // -- Weighted shortest path ----------------------------------
        LogicalPlan::WeightedShortestPath {
            input,
            path_var,
            rel_var,
            cost_var,
            ..
        } => {
            collect_plan_variables(input, symtab);
            if let Some(pv) = path_var {
                symtab.add_typed(pv.clone(), SlotType::Path);
            }
            if let Some(rv) = rel_var {
                symtab.add_typed(rv.clone(), SlotType::List);
            }
            if let Some(cv) = cost_var {
                symtab.add_typed(cv.clone(), SlotType::Float);
            }
        }

        // -- Merge ----------------------------------------------------
        LogicalPlan::Merge { input, pattern, .. } => {
            collect_plan_variables(input, symtab);
            for part in &pattern.parts {
                if let Some(var) = &part.element.node.variable {
                    symtab.add_typed(var.clone(), SlotType::NodeRef);
                }
                for chain in &part.element.chain {
                    if let Some(var) = &chain.rel.variable {
                        symtab.add_typed(var.clone(), SlotType::EdgeRef);
                    }
                    if let Some(var) = &chain.node.variable {
                        symtab.add_typed(var.clone(), SlotType::NodeRef);
                    }
                }
            }
        }
    }
}

/// Adds variables introduced by list comprehensions and quantifiers (loop variables needing slots)
/// in an expression.
#[recursive::recursive]
pub fn collect_expr_variables(expr: &Expr, symtab: &mut SymbolTable) {
    match expr {
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            symtab.add(variable.clone());
            collect_expr_variables(list_expr, symtab);
            if let Some(f) = filter {
                collect_expr_variables(f, symtab);
            }
            if let Some(m) = map_expr {
                collect_expr_variables(m, symtab);
            }
        }
        Expr::QuantifierExpr {
            variable,
            list_expr,
            predicate,
            ..
        } => {
            symtab.add(variable.clone());
            collect_expr_variables(list_expr, symtab);
            collect_expr_variables(predicate, symtab);
        }
        // -- Recurse into sub-expressions -----------------------------
        Expr::BinaryOp(l, _, r) | Expr::In(l, r) | Expr::StringOp(l, _, r) => {
            collect_expr_variables(l, symtab);
            collect_expr_variables(r, symtab);
        }
        Expr::UnaryOp(_, e) | Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Property(e, _) => {
            collect_expr_variables(e, symtab);
        }
        Expr::IsTyped { expr, .. } => {
            collect_expr_variables(expr, symtab);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_expr_variables(arg, symtab);
            }
        }
        Expr::ListLiteral(items) => {
            for item in items {
                collect_expr_variables(item, symtab);
            }
        }
        Expr::MapLiteral(entries) => {
            for (_, v) in entries {
                collect_expr_variables(v, symtab);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(op) = operand {
                collect_expr_variables(op, symtab);
            }
            for (w, t) in whens {
                collect_expr_variables(w, symtab);
                collect_expr_variables(t, symtab);
            }
            if let Some(e) = else_expr {
                collect_expr_variables(e, symtab);
            }
        }
        Expr::ListIndex(base, idx) => {
            collect_expr_variables(base, symtab);
            collect_expr_variables(idx, symtab);
        }
        Expr::ListSlice(base, from, to) => {
            collect_expr_variables(base, symtab);
            if let Some(f) = from {
                collect_expr_variables(f, symtab);
            }
            if let Some(t) = to {
                collect_expr_variables(t, symtab);
            }
        }
        Expr::Comparison(base, pairs) => {
            collect_expr_variables(base, symtab);
            for (_, e) in pairs {
                collect_expr_variables(e, symtab);
            }
        }
        // subqueries have their own execution context: don't descend
        Expr::ExistentialSubquery(_)
        | Expr::PatternPredicate(_)
        | Expr::FullExistentialSubquery(_)
        | Expr::CountSubquery(_)
        | Expr::CollectSubquery(_)
        | Expr::PatternComprehension { .. } => {}
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list_expr,
            update_expr,
        } => {
            symtab.add(accumulator.clone());
            symtab.add(variable.clone());
            collect_expr_variables(initial, symtab);
            collect_expr_variables(list_expr, symtab);
            collect_expr_variables(update_expr, symtab);
        }
        Expr::MapProjection {
            expr,
            literal_entries,
            ..
        } => {
            collect_expr_variables(expr, symtab);
            for (_, e) in literal_entries {
                collect_expr_variables(e, symtab);
            }
        }
        // terminals introduce nothing
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_table_basics() {
        let mut st = SymbolTable::new();
        assert_eq!(st.width(), 0);

        let s0 = st.add("a".to_string());
        assert_eq!(s0, 0);
        assert_eq!(st.width(), 1);
        // add() defaults to Dynamic
        assert_eq!(st.slot_type(s0), SlotType::Dynamic);

        let s1 = st.add("b".to_string());
        assert_eq!(s1, 1);

        // duplicate returns the existing slot
        let s0_again = st.add("a".to_string());
        assert_eq!(s0_again, 0);
        assert_eq!(st.width(), 2);

        assert_eq!(st.slot("a"), Some(0));
        assert_eq!(st.slot("b"), Some(1));
        assert_eq!(st.slot("c"), None);
        assert_eq!(st.name(0), "a");
        assert_eq!(st.name(1), "b");

        // add_typed sets the type for new slots
        let s2 = st.add_typed("c".to_string(), SlotType::NodeRef);
        assert_eq!(s2, 2);
        assert_eq!(st.slot_type(s2), SlotType::NodeRef);

        // add_typed on an existing slot keeps its type
        let s2_again = st.add_typed("c".to_string(), SlotType::Dynamic);
        assert_eq!(s2_again, 2);
        assert_eq!(st.slot_type(s2_again), SlotType::NodeRef);
    }

    #[test]
    fn slot_value_size() {
        assert!(
            std::mem::size_of::<SlotValue>() <= 48,
            "SlotValue is {} bytes, expected <= 48",
            std::mem::size_of::<SlotValue>()
        );
    }

    #[test]
    fn new_frame_is_all_null() {
        let mut st = SymbolTable::new();
        st.add("x".to_string());
        st.add("y".to_string());
        let frame = new_frame(&st);
        assert_eq!(frame.len(), 2);
        assert!(frame[0].is_null());
        assert!(frame[1].is_null());
    }

    #[test]
    fn frame_record_roundtrip() {
        let mut st = SymbolTable::new();
        st.add("a".to_string());
        st.add("b".to_string());

        let mut frame = new_frame(&st);
        frame[0] = SlotValue::Integer(42);
        frame[1] = SlotValue::from(CypherValue::String("hello".into()));

        let rec = frame_to_record(&frame, &st);
        assert_eq!(rec.get("a"), Some(&CypherValue::Integer(42)));
        assert_eq!(rec.get("b"), Some(&CypherValue::String("hello".into())));

        let frame2 = record_to_frame(&rec, &st);
        assert_eq!(frame, frame2);
    }

    #[test]
    fn slot_value_roundtrip() {
        use traverse_core::graph::types::{EdgeId, EdgeTypeId, NodeId};

        // inline variants roundtrip cheaply
        let sv = SlotValue::NodeRef(NodeId(42));
        assert_eq!(sv.to_cypher_value(), CypherValue::NodeRef(NodeId(42)));

        let sv = SlotValue::EdgeRef {
            id: EdgeId(1),
            edge_type_id: EdgeTypeId(2),
            source: NodeId(3),
            target: NodeId(4),
        };
        assert_eq!(
            sv.to_cypher_value(),
            CypherValue::EdgeRef {
                id: EdgeId(1),
                edge_type_id: EdgeTypeId(2),
                source: NodeId(3),
                target: NodeId(4),
            }
        );

        let sv = SlotValue::Integer(99);
        assert_eq!(sv.to_cypher_value(), CypherValue::Integer(99));

        // Heap roundtrips through the Box
        let cv = CypherValue::String("test".into());
        let sv = SlotValue::from(cv.clone());
        assert_eq!(sv.to_cypher_value(), cv);
    }

    #[test]
    fn typed_slot_inference() {
        use crate::planner::LogicalPlan;

        // single hop: Scan(n) -> ExpandOut(r, m)
        let plan = LogicalPlan::ExpandOut {
            input: Box::new(LogicalPlan::Scan {
                variable: "n".to_string(),
                label: None,
            }),
            from: "n".to_string(),
            rel_var: "r".to_string(),
            to: "m".to_string(),
            edge_types: vec![],
            range: None,
            prior_edge_vars: vec![],
        };
        let st = build_symbol_table(&plan);
        assert_eq!(st.slot_type(st.slot("n").unwrap()), SlotType::NodeRef);
        assert_eq!(st.slot_type(st.slot("r").unwrap()), SlotType::EdgeRef);
        assert_eq!(st.slot_type(st.slot("m").unwrap()), SlotType::NodeRef);

        // variable length: rel_var becomes List
        let vl_plan = LogicalPlan::ExpandOut {
            input: Box::new(LogicalPlan::Scan {
                variable: "n".to_string(),
                label: None,
            }),
            from: "n".to_string(),
            rel_var: "rels".to_string(),
            to: "m".to_string(),
            edge_types: vec![],
            range: Some((Some(1), Some(3))),
            prior_edge_vars: vec![],
        };
        let st2 = build_symbol_table(&vl_plan);
        assert_eq!(st2.slot_type(st2.slot("rels").unwrap()), SlotType::List);
        assert_eq!(st2.slot_type(st2.slot("m").unwrap()), SlotType::NodeRef);

        // explicit [*1..1] is still single-hop
        let explicit_single = LogicalPlan::ExpandIn {
            input: Box::new(LogicalPlan::Scan {
                variable: "a".to_string(),
                label: None,
            }),
            from: "a".to_string(),
            rel_var: "e".to_string(),
            to: "b".to_string(),
            edge_types: vec![],
            range: Some((Some(1), Some(1))),
            prior_edge_vars: vec![],
        };
        let st3 = build_symbol_table(&explicit_single);
        assert_eq!(st3.slot_type(st3.slot("e").unwrap()), SlotType::EdgeRef);
    }
}
