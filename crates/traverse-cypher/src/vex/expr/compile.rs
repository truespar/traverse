//! `Expr` -> [`ExprNode`]: variables to positions, comprehension variables to locals, function
//! names to kinds, subqueries to compiled sub-plans.

use std::sync::Arc;

use crate::ast::{Expr, Literal, MatchClause, SingleQuery};
use crate::error::CypherError;
use crate::planner::is_aggregate_function;

use super::super::layout::Layout;
use super::super::plan::SubPlan;
use super::{CompiledExpr, ExprNode, FuncKind};

/// Lowers subquery bodies for the expression compiler. The lowering module implements it; the
/// compiler only needs the resulting sub-plans.
pub trait SubqueryLowerer {
    /// `EXISTS { MATCH ... }` / a pattern predicate / `COUNT { MATCH ... }`.
    fn lower_pattern_subquery(
        &mut self,
        mc: &MatchClause,
        outer: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError>;

    /// `EXISTS { full query }` / `COLLECT { full query }`.
    fn lower_full_subquery(
        &mut self,
        sq: &SingleQuery,
        outer: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError>;

    /// A pattern comprehension; the sub-plan's single output column is the projection.
    fn lower_pattern_comprehension(
        &mut self,
        path_var: Option<&str>,
        mc: &MatchClause,
        projection: &Expr,
        outer: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError>;
}

/// A lowerer that rejects every subquery; for expressions that are known not to contain any.
pub struct NoSubqueries;

impl SubqueryLowerer for NoSubqueries {
    fn lower_pattern_subquery(
        &mut self,
        _mc: &MatchClause,
        _outer: &Layout,
        _locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        Err(CypherError::semantic(
            "subquery expressions are not allowed here".to_string(),
        ))
    }

    fn lower_full_subquery(
        &mut self,
        _sq: &SingleQuery,
        _outer: &Layout,
        _locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        Err(CypherError::semantic(
            "subquery expressions are not allowed here".to_string(),
        ))
    }

    fn lower_pattern_comprehension(
        &mut self,
        _path_var: Option<&str>,
        _mc: &MatchClause,
        _projection: &Expr,
        _outer: &Layout,
        _locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        Err(CypherError::semantic(
            "pattern comprehensions are not allowed here".to_string(),
        ))
    }
}

/// Compiles `expr` against `layout`; subqueries fail.
pub fn compile_expr(expr: &Expr, layout: &Layout) -> Result<CompiledExpr, CypherError> {
    compile_expr_with(expr, layout, &mut NoSubqueries)
}

/// Compiles `expr` against `layout`, lowering subqueries through `sub`.
pub fn compile_expr_with(
    expr: &Expr,
    layout: &Layout,
    sub: &mut dyn SubqueryLowerer,
) -> Result<CompiledExpr, CypherError> {
    let mut c = Compiler {
        layout,
        locals: Vec::new(),
        sub,
    };
    let root = c.node(expr)?;
    Ok(CompiledExpr {
        root,
        source: expr.clone(),
    })
}

struct Compiler<'a> {
    layout: &'a Layout,
    /// Local bindings in scope, innermost last.
    locals: Vec<String>,
    sub: &'a mut dyn SubqueryLowerer,
}

impl Compiler<'_> {
    fn var(&self, name: &str) -> ExprNode {
        // innermost local wins over an outer variable of the same name
        if let Some(i) = self.locals.iter().rposition(|l| l == name) {
            return ExprNode::Local(i as u32);
        }
        match self.layout.pos(name) {
            Some(p) => ExprNode::Var(p),
            None => ExprNode::Unbound(name.to_string()),
        }
    }

    #[recursive::recursive]
    fn node(&mut self, expr: &Expr) -> Result<ExprNode, CypherError> {
        Ok(match expr {
            Expr::Literal(lit) => ExprNode::Const(match lit {
                Literal::Null => crate::types::CypherValue::Null,
                Literal::Bool(b) => crate::types::CypherValue::Bool(*b),
                Literal::Integer(i) => crate::types::CypherValue::Integer(*i),
                Literal::Float(f) => crate::types::CypherValue::Float(*f),
                Literal::String(s) => crate::types::CypherValue::String(s.clone()),
            }),
            Expr::Variable(name) => self.var(name),
            Expr::Parameter(name) => ExprNode::Param(name.clone()),
            Expr::Property(base, key) => ExprNode::Prop {
                base: Box::new(self.node(base)?),
                key: key.clone(),
            },
            Expr::BinaryOp(l, op, r) => {
                let l = Box::new(self.node(l)?);
                let r = Box::new(self.node(r)?);
                match op {
                    crate::ast::BinOp::And => ExprNode::And(l, r),
                    crate::ast::BinOp::Or => ExprNode::Or(l, r),
                    crate::ast::BinOp::Xor => ExprNode::Xor(l, r),
                    other => ExprNode::Arith { op: *other, l, r },
                }
            }
            Expr::UnaryOp(op, e) => {
                let e = Box::new(self.node(e)?);
                match op {
                    crate::ast::UnaryOp::Not => ExprNode::Not(e),
                    crate::ast::UnaryOp::Neg => ExprNode::Neg(e),
                }
            }
            Expr::Comparison(first, rest) => {
                let f = self.node(first)?;
                if rest.len() == 1 {
                    ExprNode::Cmp {
                        op: rest[0].0,
                        l: Box::new(f),
                        r: Box::new(self.node(&rest[0].1)?),
                    }
                } else {
                    let mut pairs = Vec::with_capacity(rest.len());
                    for (op, e) in rest {
                        pairs.push((*op, self.node(e)?));
                    }
                    ExprNode::CmpChain {
                        first: Box::new(f),
                        rest: pairs,
                    }
                }
            }
            Expr::FunctionCall { name, args, .. } => self.func(name, args)?,
            Expr::ListLiteral(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(self.node(i)?);
                }
                ExprNode::ListLit(out)
            }
            Expr::MapLiteral(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, e) in entries {
                    out.push((k.clone(), self.node(e)?));
                }
                ExprNode::MapLit(out)
            }
            Expr::Case {
                operand,
                whens,
                else_expr,
            } => {
                let operand = match operand {
                    Some(o) => Some(Box::new(self.node(o)?)),
                    None => None,
                };
                let mut ws = Vec::with_capacity(whens.len());
                for (w, t) in whens {
                    ws.push((self.node(w)?, self.node(t)?));
                }
                let else_ = match else_expr {
                    Some(e) => Some(Box::new(self.node(e)?)),
                    None => None,
                };
                ExprNode::Case {
                    operand,
                    whens: ws,
                    else_,
                }
            }
            Expr::In(item, list) => ExprNode::In {
                item: Box::new(self.node(item)?),
                list: Box::new(self.node(list)?),
            },
            Expr::IsNull(e) => ExprNode::IsNull(Box::new(self.node(e)?)),
            Expr::IsNotNull(e) => ExprNode::IsNotNull(Box::new(self.node(e)?)),
            Expr::IsTyped {
                expr,
                type_name,
                negated,
                not_null,
            } => ExprNode::IsTyped {
                e: Box::new(self.node(expr)?),
                type_name: type_name.clone(),
                negated: *negated,
                not_null: *not_null,
            },
            Expr::StringOp(l, op, r) => ExprNode::StringOp {
                op: *op,
                l: Box::new(self.node(l)?),
                r: Box::new(self.node(r)?),
            },
            Expr::ListIndex(base, idx) => ExprNode::Index {
                base: Box::new(self.node(base)?),
                idx: Box::new(self.node(idx)?),
            },
            Expr::ListSlice(base, from, to) => ExprNode::Slice {
                base: Box::new(self.node(base)?),
                from: match from {
                    Some(f) => Some(Box::new(self.node(f)?)),
                    None => None,
                },
                to: match to {
                    Some(t) => Some(Box::new(self.node(t)?)),
                    None => None,
                },
            },
            Expr::ListComprehension {
                variable,
                list_expr,
                filter,
                map_expr,
            } => {
                let list = Box::new(self.node(list_expr)?);
                self.locals.push(variable.clone());
                let filter = match filter {
                    Some(f) => Some(Box::new(self.node(f)?)),
                    None => None,
                };
                let map = match map_expr {
                    Some(m) => Some(Box::new(self.node(m)?)),
                    None => None,
                };
                self.locals.pop();
                ExprNode::ListComp { list, filter, map }
            }
            Expr::QuantifierExpr {
                kind,
                variable,
                list_expr,
                predicate,
            } => {
                let list = Box::new(self.node(list_expr)?);
                self.locals.push(variable.clone());
                let pred = Box::new(self.node(predicate)?);
                self.locals.pop();
                ExprNode::Quantifier {
                    kind: *kind,
                    list,
                    pred,
                }
            }
            Expr::Reduce {
                accumulator,
                initial,
                variable,
                list_expr,
                update_expr,
            } => {
                let init = Box::new(self.node(initial)?);
                let list = Box::new(self.node(list_expr)?);
                self.locals.push(accumulator.clone());
                self.locals.push(variable.clone());
                let update = Box::new(self.node(update_expr)?);
                self.locals.pop();
                self.locals.pop();
                ExprNode::Reduce { init, list, update }
            }
            Expr::MapProjection {
                expr,
                properties,
                all_properties,
                literal_entries,
            } => {
                let base = Box::new(self.node(expr)?);
                let mut literal = Vec::with_capacity(literal_entries.len());
                for (k, e) in literal_entries {
                    literal.push((k.clone(), self.node(e)?));
                }
                ExprNode::MapProj {
                    base,
                    props: properties.clone(),
                    all: *all_properties,
                    literal,
                }
            }
            Expr::ExistentialSubquery(mc) | Expr::PatternPredicate(mc) => {
                let sp = self
                    .sub
                    .lower_pattern_subquery(mc, self.layout, &self.locals)?;
                ExprNode::Exists(sp)
            }
            Expr::CountSubquery(mc) => {
                let sp = self
                    .sub
                    .lower_pattern_subquery(mc, self.layout, &self.locals)?;
                ExprNode::CountSub(sp)
            }
            Expr::FullExistentialSubquery(sq) => {
                let sp = self
                    .sub
                    .lower_full_subquery(sq, self.layout, &self.locals)?;
                ExprNode::Exists(sp)
            }
            Expr::CollectSubquery(sq) => {
                let sp = self
                    .sub
                    .lower_full_subquery(sq, self.layout, &self.locals)?;
                ExprNode::CollectSub(sp)
            }
            Expr::PatternComprehension {
                variable,
                match_clause,
                projection,
            } => {
                let sp = self.sub.lower_pattern_comprehension(
                    variable.as_deref(),
                    match_clause,
                    projection,
                    self.layout,
                    &self.locals,
                )?;
                ExprNode::PatternComp(sp)
            }
        })
    }

    fn func(&mut self, name: &str, args: &[Expr]) -> Result<ExprNode, CypherError> {
        let kind = match name {
            "__has_label" => match args.get(1) {
                Some(Expr::Literal(Literal::String(s))) => FuncKind::HasLabel(s.clone()),
                _ => FuncKind::HasLabelDynamic,
            },
            "__composite_key" => FuncKind::CompositeKey,
            "file" => FuncKind::File(self.layout.pos(crate::vex::ops::load_csv::CSV_CTX_SLOT)),
            "linenumber" => {
                FuncKind::LineNumber(self.layout.pos(crate::vex::ops::load_csv::CSV_CTX_SLOT))
            }
            "id" => FuncKind::Id,
            "elementid" => FuncKind::ElementId,
            "type" => FuncKind::Type,
            "labels" => FuncKind::Labels,
            "startnode" => FuncKind::StartNode,
            "endnode" => FuncKind::EndNode,
            "coalesce" => FuncKind::Coalesce,
            _ if name.starts_with("apoc.") && !is_aggregate_function(name) => FuncKind::Apoc,
            "gds.util.asnode" if args.len() == 1 => FuncKind::AsNode,
            _ if name.starts_with("gds.") => FuncKind::Gds,
            _ if is_aggregate_function(name) => FuncKind::Aggregate,
            _ => FuncKind::Generic,
        };
        let mut compiled = Vec::with_capacity(args.len());
        for a in args {
            compiled.push(self.node(a)?);
        }
        Ok(ExprNode::Func {
            name: name.to_string(),
            kind,
            args: compiled,
        })
    }
}
