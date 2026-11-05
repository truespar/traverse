//! Compiled expressions: an [`Expr`] resolved against a [`Layout`] into a tree the evaluator runs
//! over chunks.
//!
//! Variables become positions, comprehension variables become local bindings, function names
//! become kinds, and subquery bodies become compiled sub-plans with seed maps. The tree is
//! immutable and shared; evaluation state (locals, subquery operator instances) lives in an
//! [`eval::EvalScratch`] owned by the operator.

pub mod compile;
pub mod eval;
pub mod kernels;
pub mod scalar;

use std::sync::Arc;

use crate::ast::{BinOp, CmpOp, QuantifierKind, StringOp};
use crate::types::CypherValue;

use super::chunk::DataPos;
use super::plan::SubPlan;

pub use compile::{SubqueryLowerer, compile_expr, compile_expr_with};
pub use eval::{EvalScratch, Operand};

/// A compiled expression.
#[derive(Debug)]
pub struct CompiledExpr {
    /// The tree.
    pub root: ExprNode,
    /// The original, for naming and error messages.
    pub source: crate::ast::Expr,
}

/// Which built-in a function call resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuncKind {
    /// `id(entity)`.
    Id,
    /// `elementId(entity)`.
    ElementId,
    /// `type(rel)`.
    Type,
    /// `labels(node)`.
    Labels,
    /// `startNode(rel)`.
    StartNode,
    /// `endNode(rel)`.
    EndNode,
    /// `coalesce(...)`.
    Coalesce,
    /// `__has_label(entity, label)` with a static label.
    HasLabel(String),
    /// `__has_label(entity, expr)` with a dynamic label.
    HasLabelDynamic,
    /// `__composite_key(...)`.
    CompositeKey,
    /// `file()` of LOAD CSV, reading the hidden context column.
    File(Option<DataPos>),
    /// `linenumber()` of LOAD CSV.
    LineNumber(Option<DataPos>),
    /// An `apoc.*` function.
    Apoc,
    /// A `gds.*` function.
    Gds,
    /// `gds.util.asNode(id)`: the node with that id, null if there is none. A kernel of its own:
    /// one existence check per chunk, and the node column it makes gathers properties in batch.
    AsNode,
    /// An aggregate name outside an aggregation (an error when evaluated).
    Aggregate,
    /// Any other built-in, through the scalar function table.
    Generic,
}

/// A node of the compiled tree.
#[derive(Debug)]
pub enum ExprNode {
    /// A literal.
    Const(CypherValue),
    /// A query parameter.
    Param(String),
    /// A bound variable.
    Var(DataPos),
    /// A variable the layout doesn't bind: null.
    Unbound(String),
    /// A local binding (comprehension / reduce variable), by depth.
    Local(u32),
    /// `base.key`.
    Prop {
        /// The base.
        base: Box<ExprNode>,
        /// The key.
        key: String,
    },
    /// `base[idx]`.
    Index {
        /// The base.
        base: Box<ExprNode>,
        /// The index.
        idx: Box<ExprNode>,
    },
    /// `base[from..to]`.
    Slice {
        /// The base.
        base: Box<ExprNode>,
        /// From.
        from: Option<Box<ExprNode>>,
        /// To.
        to: Option<Box<ExprNode>>,
    },
    /// A single comparison.
    Cmp {
        /// The operator.
        op: CmpOp,
        /// Left.
        l: Box<ExprNode>,
        /// Right.
        r: Box<ExprNode>,
    },
    /// A comparison chain `a < b <= c`.
    CmpChain {
        /// First operand.
        first: Box<ExprNode>,
        /// `(op, operand)` pairs.
        rest: Vec<(CmpOp, ExprNode)>,
    },
    /// Arithmetic or concatenation.
    Arith {
        /// The operator.
        op: BinOp,
        /// Left.
        l: Box<ExprNode>,
        /// Right.
        r: Box<ExprNode>,
    },
    /// Three-valued AND with short-circuit.
    And(Box<ExprNode>, Box<ExprNode>),
    /// Three-valued OR with short-circuit.
    Or(Box<ExprNode>, Box<ExprNode>),
    /// XOR.
    Xor(Box<ExprNode>, Box<ExprNode>),
    /// NOT.
    Not(Box<ExprNode>),
    /// Unary minus.
    Neg(Box<ExprNode>),
    /// `IS NULL`.
    IsNull(Box<ExprNode>),
    /// `IS NOT NULL`.
    IsNotNull(Box<ExprNode>),
    /// `IS :: TYPE`.
    IsTyped {
        /// The value.
        e: Box<ExprNode>,
        /// The type.
        type_name: String,
        /// Negated.
        negated: bool,
        /// `NOT NULL`.
        not_null: bool,
    },
    /// `item IN list`.
    In {
        /// The item.
        item: Box<ExprNode>,
        /// The list.
        list: Box<ExprNode>,
    },
    /// A string predicate.
    StringOp {
        /// The operator.
        op: StringOp,
        /// Left.
        l: Box<ExprNode>,
        /// Right.
        r: Box<ExprNode>,
    },
    /// `CASE`.
    Case {
        /// Simple-form operand.
        operand: Option<Box<ExprNode>>,
        /// `WHEN ... THEN ...`.
        whens: Vec<(ExprNode, ExprNode)>,
        /// `ELSE`.
        else_: Option<Box<ExprNode>>,
    },
    /// A list literal.
    ListLit(Vec<ExprNode>),
    /// A map literal.
    MapLit(Vec<(String, ExprNode)>),
    /// A function call.
    Func {
        /// Lowercased name.
        name: String,
        /// What it resolved to.
        kind: FuncKind,
        /// Arguments.
        args: Vec<ExprNode>,
    },
    /// A list comprehension; the variable is `Local(depth)`.
    ListComp {
        /// The list.
        list: Box<ExprNode>,
        /// The filter.
        filter: Option<Box<ExprNode>>,
        /// The map.
        map: Option<Box<ExprNode>>,
    },
    /// A quantifier; the variable is `Local(depth)`.
    Quantifier {
        /// Which.
        kind: QuantifierKind,
        /// The list.
        list: Box<ExprNode>,
        /// The predicate.
        pred: Box<ExprNode>,
    },
    /// `reduce`; the accumulator is `Local(depth)`, the variable `Local(depth + 1)`.
    Reduce {
        /// Initial value.
        init: Box<ExprNode>,
        /// The list.
        list: Box<ExprNode>,
        /// The update.
        update: Box<ExprNode>,
    },
    /// A map projection.
    MapProj {
        /// The base.
        base: Box<ExprNode>,
        /// `.prop` selectors.
        props: Vec<String>,
        /// `.*`.
        all: bool,
        /// Literal entries.
        literal: Vec<(String, ExprNode)>,
    },
    /// `EXISTS { ... }` / a pattern predicate.
    Exists(Arc<SubPlan>),
    /// `COUNT { ... }`.
    CountSub(Arc<SubPlan>),
    /// `COLLECT { ... }`: the body's single column as a list.
    CollectSub(Arc<SubPlan>),
    /// A pattern comprehension: the body's projection column as a list.
    PatternComp(Arc<SubPlan>),
}

impl CompiledExpr {
    /// Is the tree a plain variable reference?
    pub fn as_var(&self) -> Option<DataPos> {
        match &self.root {
            ExprNode::Var(p) => Some(*p),
            _ => None,
        }
    }

    /// Is the tree a literal?
    pub fn as_const(&self) -> Option<&CypherValue> {
        match &self.root {
            ExprNode::Const(v) => Some(v),
            _ => None,
        }
    }

    /// Every position the tree reads, including through subqueries' seeds.
    pub fn positions(&self) -> Vec<DataPos> {
        let mut out = Vec::new();
        collect_positions(&self.root, &mut out);
        out.sort_unstable_by_key(|p| (p.group, p.col));
        out.dedup();
        out
    }
}

fn collect_positions(node: &ExprNode, out: &mut Vec<DataPos>) {
    match node {
        ExprNode::Var(p) => out.push(*p),
        ExprNode::Const(_) | ExprNode::Param(_) | ExprNode::Unbound(_) | ExprNode::Local(_) => {}
        ExprNode::Prop { base, .. } => collect_positions(base, out),
        ExprNode::Index { base, idx } => {
            collect_positions(base, out);
            collect_positions(idx, out);
        }
        ExprNode::Slice { base, from, to } => {
            collect_positions(base, out);
            if let Some(f) = from {
                collect_positions(f, out);
            }
            if let Some(t) = to {
                collect_positions(t, out);
            }
        }
        ExprNode::Cmp { l, r, .. }
        | ExprNode::Arith { l, r, .. }
        | ExprNode::And(l, r)
        | ExprNode::Or(l, r)
        | ExprNode::Xor(l, r)
        | ExprNode::StringOp { l, r, .. } => {
            collect_positions(l, out);
            collect_positions(r, out);
        }
        ExprNode::In { item, list } => {
            collect_positions(item, out);
            collect_positions(list, out);
        }
        ExprNode::CmpChain { first, rest } => {
            collect_positions(first, out);
            for (_, e) in rest {
                collect_positions(e, out);
            }
        }
        ExprNode::Not(e)
        | ExprNode::Neg(e)
        | ExprNode::IsNull(e)
        | ExprNode::IsNotNull(e)
        | ExprNode::IsTyped { e, .. } => collect_positions(e, out),
        ExprNode::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_positions(o, out);
            }
            for (w, t) in whens {
                collect_positions(w, out);
                collect_positions(t, out);
            }
            if let Some(e) = else_ {
                collect_positions(e, out);
            }
        }
        ExprNode::ListLit(items) => {
            for i in items {
                collect_positions(i, out);
            }
        }
        ExprNode::MapLit(entries) => {
            for (_, e) in entries {
                collect_positions(e, out);
            }
        }
        ExprNode::Func { args, kind, .. } => {
            if let FuncKind::File(Some(p)) | FuncKind::LineNumber(Some(p)) = kind {
                out.push(*p);
            }
            for a in args {
                collect_positions(a, out);
            }
        }
        ExprNode::ListComp { list, filter, map } => {
            collect_positions(list, out);
            if let Some(f) = filter {
                collect_positions(f, out);
            }
            if let Some(m) = map {
                collect_positions(m, out);
            }
        }
        ExprNode::Quantifier { list, pred, .. } => {
            collect_positions(list, out);
            collect_positions(pred, out);
        }
        ExprNode::Reduce { init, list, update } => {
            collect_positions(init, out);
            collect_positions(list, out);
            collect_positions(update, out);
        }
        ExprNode::MapProj { base, literal, .. } => {
            collect_positions(base, out);
            for (_, e) in literal {
                collect_positions(e, out);
            }
        }
        ExprNode::Exists(sp)
        | ExprNode::CountSub(sp)
        | ExprNode::CollectSub(sp)
        | ExprNode::PatternComp(sp) => {
            for (src, _) in &sp.seeds {
                if let super::plan::SeedSource::Outer(p) = src {
                    out.push(*p);
                }
            }
        }
    }
}
