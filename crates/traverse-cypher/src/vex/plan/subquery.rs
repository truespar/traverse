//! Logical plans of subquery expressions: pattern-form EXISTS / COUNT, full-query EXISTS /
//! COLLECT and pattern comprehensions. Each is planned with the ordinary planner, given the
//! subset of the optimizer that keeps its bindings, and then lowered like any plan with the
//! outer variables as seeds.

use std::collections::HashSet;

use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, CmpOp, Expr, Literal, MatchClause, SingleQuery};
use crate::error::CypherError;
use crate::planner::{IndexHints, LogicalPlan};

/// Plan of `EXISTS { MATCH pattern WHERE ... }` / `COUNT { ... }` / a pattern predicate.
pub fn pattern_subquery_plan(mc: &MatchClause) -> Result<LogicalPlan, CypherError> {
    let mut plan = crate::planner::plan_match_pattern_pub(&mc.pattern)?;
    if let Some(ref where_expr) = mc.where_clause {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: where_expr.clone(),
        };
    }
    Ok(optimize_subquery_plan(plan))
}

/// Plan of a full-query `EXISTS { ... }` / `COLLECT { ... }` body.
pub fn full_subquery_plan(sq: &SingleQuery) -> Result<LogicalPlan, CypherError> {
    Ok(optimize_subquery_plan(crate::planner::plan_single_query(
        sq,
    )?))
}

/// The safe optimizer subset for subquery plans: constant folding (a folded `WHERE 1 = 0` is why
/// subqueries get an optimizer at all) and id seeks; nothing that drops bindings.
fn optimize_subquery_plan(plan: LogicalPlan) -> LogicalPlan {
    let plan = if crate::query::with_current(|c| c.is_some_and(|ctx| !ctx.params().is_empty())) {
        // literal stripping turns `WHERE 1 = 0` into `WHERE $__sp0 = $__sp1` before planning;
        // inline the known parameters so folding fires
        resolve_known_params_in_plan(plan)
    } else {
        plan
    };
    let plan = crate::optimizer::rewrite::fold_constants(plan);
    crate::optimizer::rewrite::convert_filter_to_node_by_id(plan)
}

/// Every outer variable a subquery body may read: pattern, WHERE and projection variables.
pub fn pattern_subquery_refs(mc: &MatchClause, projection: Option<&Expr>) -> HashSet<String> {
    let mut vars = HashSet::new();
    for part in &mc.pattern.parts {
        if let Some(v) = &part.variable {
            vars.insert(v.clone());
        }
        fn node(n: &crate::ast::NodePattern, vars: &mut HashSet<String>) {
            if let Some(v) = &n.variable {
                vars.insert(v.clone());
            }
            if let Some(props) = &n.properties {
                crate::planner::collect_expr_variables(props, vars);
            }
            if let Some(w) = &n.where_pred {
                crate::planner::collect_expr_variables(w, vars);
            }
        }
        node(&part.element.node, &mut vars);
        for chain in &part.element.chain {
            node(&chain.node, &mut vars);
            if let Some(v) = &chain.rel.variable {
                vars.insert(v.clone());
            }
            if let Some(props) = &chain.rel.properties {
                crate::planner::collect_expr_variables(props, &mut vars);
            }
            if let Some(w) = &chain.rel.where_pred {
                crate::planner::collect_expr_variables(w, &mut vars);
            }
        }
    }
    if let Some(w) = &mc.where_clause {
        crate::planner::collect_expr_variables(w, &mut vars);
    }
    if let Some(p) = projection {
        crate::planner::collect_expr_variables(p, &mut vars);
    }
    vars
}

/// Every outer variable a full-query body may read.
pub fn full_subquery_refs(sq: &SingleQuery) -> HashSet<String> {
    let mut vars = HashSet::new();
    fn visit_clause(c: &crate::ast::Clause, vars: &mut HashSet<String>) {
        match c {
            crate::ast::Clause::Match(mc) | crate::ast::Clause::OptionalMatch(mc) => {
                vars.extend(pattern_subquery_refs(mc, None));
            }
            crate::ast::Clause::Unwind(uw) => {
                crate::planner::collect_expr_variables(&uw.expr, vars);
            }
            _ => {}
        }
    }
    for part in &sq.parts {
        for c in &part.clauses {
            visit_clause(c, &mut vars);
        }
        if let crate::ast::ProjectionItems::Explicit(items) = &part.with.items {
            for it in items {
                crate::planner::collect_expr_variables(&it.expr, &mut vars);
            }
        }
        if let Some(w) = &part.with.where_clause {
            crate::planner::collect_expr_variables(w, &mut vars);
        }
    }
    for c in &sq.final_part.clauses {
        visit_clause(c, &mut vars);
    }
    if let Some(rc) = &sq.final_part.return_clause
        && let crate::ast::ProjectionItems::Explicit(items) = &rc.items
    {
        for it in items {
            crate::planner::collect_expr_variables(&it.expr, &mut vars);
        }
    }
    vars
}

/// Plan of a pattern comprehension: anonymous elements named so the path can be bound, the
/// pattern planned, an optional `BindPath`, the WHERE as a filter with an index seek when it pins
/// the leaf variable's indexed property to outer-only expressions, and the projection as the
/// single output column `__pc_out`.
pub fn pattern_comprehension_plan(
    path_var: Option<&str>,
    mc: &MatchClause,
    projection: &Expr,
    graph: &dyn GraphView,
) -> Result<LogicalPlan, CypherError> {
    let mut pattern = mc.pattern.clone();
    let mut node_vars = Vec::new();
    let mut edge_vars = Vec::new();
    for part in &mut pattern.parts {
        if part.element.node.variable.is_none() {
            part.element.node.variable = Some(format!("__pc_{}", next_pc_id()));
        }
        node_vars.push(part.element.node.variable.clone().unwrap());
        for chain in &mut part.element.chain {
            if chain.rel.variable.is_none() {
                chain.rel.variable = Some(format!("__pc_{}", next_pc_id()));
            }
            edge_vars.push(chain.rel.variable.clone().unwrap());
            if chain.node.variable.is_none() {
                chain.node.variable = Some(format!("__pc_{}", next_pc_id()));
            }
            node_vars.push(chain.node.variable.clone().unwrap());
        }
    }
    let mut local_vars: HashSet<String> = node_vars.iter().cloned().collect();
    local_vars.extend(edge_vars.iter().cloned());
    if let Some(pv) = path_var {
        local_vars.insert(pv.to_string());
    }

    let mut plan = crate::planner::plan_match_pattern_pub(&pattern)?;
    if let Some(pv) = path_var {
        plan = LogicalPlan::BindPath {
            input: Box::new(plan),
            variable: pv.to_string(),
            node_vars,
            edge_vars,
        };
    }
    if let Some(ref where_expr) = mc.where_clause {
        plan = comprehension_access_path(plan, where_expr, &local_vars, graph);
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: where_expr.clone(),
        };
    }
    let plan = optimize_subquery_plan(plan);
    Ok(LogicalPlan::Project {
        input: Box::new(plan),
        items: vec![crate::ast::ProjectionItem {
            expr: projection.clone(),
            alias: Some(PC_OUT.to_string()),
            source_text: None,
        }],
        distinct: false,
        where_clause: None,
    })
}

/// The output column of a pattern comprehension's plan.
pub const PC_OUT: &str = "__pc_out";

fn next_pc_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Replaces a comprehension plan's leaf label `Scan` with index seeks when `where_expr` pins the
/// leaf variable's indexed property to OUTER-only expressions: one equality conjunct (one
/// `IndexScan`), or an OR with such an equality in every branch (distinct `Union` of seeks).
fn comprehension_access_path(
    plan: LogicalPlan,
    where_expr: &Expr,
    local_vars: &HashSet<String>,
    graph: &dyn GraphView,
) -> LogicalPlan {
    #[recursive::recursive]
    fn find_leaf<'a>(p: &'a LogicalPlan) -> Option<(&'a str, &'a str)> {
        match p {
            LogicalPlan::Scan {
                variable,
                label: Some(l),
            } => Some((variable, l)),
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::ExpandOut { input, .. }
            | LogicalPlan::ExpandIn { input, .. }
            | LogicalPlan::ExpandBoth { input, .. }
            | LogicalPlan::DistinctExpand { input, .. }
            | LogicalPlan::BindPath { input, .. } => find_leaf(input),
            _ => None,
        }
    }
    let Some((leaf_var, leaf_label)) = find_leaf(&plan) else {
        return plan;
    };
    let leaf_var = leaf_var.to_string();
    let leaf_label = leaf_label.to_string();
    let hints: IndexHints = crate::build_index_hints_for(graph);

    let usable = |e: &Expr| -> Option<(String, Expr)> {
        let Expr::Comparison(l, pairs) = e else {
            return None;
        };
        if pairs.len() != 1 || pairs[0].0 != CmpOp::Eq {
            return None;
        }
        let r = &pairs[0].1;
        for (prop_side, key_side) in [(&**l, r), (r, &**l)] {
            if let Expr::Property(base, key) = prop_side
                && matches!(base.as_ref(), Expr::Variable(v) if *v == leaf_var)
                && hints.contains(&(leaf_label.clone(), key.clone()))
            {
                let mut vars = HashSet::new();
                crate::planner::collect_expr_variables(key_side, &mut vars);
                if vars.is_disjoint(local_vars) {
                    return Some((key.clone(), key_side.clone()));
                }
            }
        }
        None
    };
    #[recursive::recursive]
    fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
        if let Expr::BinaryOp(l, BinOp::And, r) = e {
            conjuncts(l, out);
            conjuncts(r, out);
        } else {
            out.push(e.clone());
        }
    }
    #[recursive::recursive]
    fn or_branches(e: &Expr, out: &mut Vec<Expr>) {
        if let Expr::BinaryOp(l, BinOp::Or, r) = e {
            or_branches(l, out);
            or_branches(r, out);
        } else {
            out.push(e.clone());
        }
    }

    let mut keys: Vec<(String, Expr)> = Vec::new();
    let mut top = Vec::new();
    conjuncts(where_expr, &mut top);
    if let Some(k) = top.iter().find_map(&usable) {
        keys.push(k);
    } else {
        let mut branches = Vec::new();
        or_branches(where_expr, &mut branches);
        if branches.len() >= 2 {
            let per_branch: Option<Vec<(String, Expr)>> = branches
                .iter()
                .map(|b| {
                    let mut cs = Vec::new();
                    conjuncts(b, &mut cs);
                    cs.iter().find_map(&usable)
                })
                .collect();
            if let Some(ks) = per_branch
                && !ks.is_empty()
                && ks.iter().all(|(p, _)| *p == ks[0].0)
            {
                keys = ks;
            }
        }
    }
    if keys.is_empty() {
        return plan;
    }

    let prop = keys[0].0.clone();
    let mut seek: Option<LogicalPlan> = None;
    for (_, key) in keys {
        let scan = LogicalPlan::IndexScan {
            variable: leaf_var.clone(),
            label: leaf_label.clone(),
            property: prop.clone(),
            value: key,
        };
        seek = Some(match seek {
            None => scan,
            Some(prev) => LogicalPlan::Union {
                left: Box::new(prev),
                right: Box::new(scan),
                all: false,
            },
        });
    }
    let seek = seek.unwrap();

    #[recursive::recursive]
    fn swap_leaf(p: LogicalPlan, seek: &LogicalPlan) -> LogicalPlan {
        match p {
            LogicalPlan::Scan { label: Some(_), .. } => seek.clone(),
            LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
                input: Box::new(swap_leaf(*input, seek)),
                predicate,
            },
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => LogicalPlan::ExpandOut {
                input: Box::new(swap_leaf(*input, seek)),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            },
            LogicalPlan::ExpandIn {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => LogicalPlan::ExpandIn {
                input: Box::new(swap_leaf(*input, seek)),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            },
            LogicalPlan::ExpandBoth {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => LogicalPlan::ExpandBoth {
                input: Box::new(swap_leaf(*input, seek)),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            },
            LogicalPlan::BindPath {
                input,
                variable,
                node_vars,
                edge_vars,
            } => LogicalPlan::BindPath {
                input: Box::new(swap_leaf(*input, seek)),
                variable,
                node_vars,
                edge_vars,
            },
            other => other,
        }
    }
    swap_leaf(plan, &seek)
}

/// Replaces `Expr::Parameter(name)` with a literal when the parameter is bound to a simple
/// literal, throughout the read operators of a plan.
#[recursive::recursive]
fn resolve_known_params_in_plan(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(resolve_known_params_in_plan(*input)),
            predicate: resolve_known_params_in_expr(predicate),
        },
        LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } => LogicalPlan::Project {
            input: Box::new(resolve_known_params_in_plan(*input)),
            items: items
                .into_iter()
                .map(|item| crate::ast::ProjectionItem {
                    expr: resolve_known_params_in_expr(item.expr),
                    alias: item.alias,
                    source_text: item.source_text,
                })
                .collect(),
            distinct,
            where_clause: where_clause.map(resolve_known_params_in_expr),
        },
        LogicalPlan::Aggregate { input, keys, aggs } => LogicalPlan::Aggregate {
            input: Box::new(resolve_known_params_in_plan(*input)),
            keys: keys
                .into_iter()
                .map(|item| crate::ast::ProjectionItem {
                    expr: resolve_known_params_in_expr(item.expr),
                    alias: item.alias,
                    source_text: item.source_text,
                })
                .collect(),
            aggs: aggs
                .into_iter()
                .map(|item| crate::ast::ProjectionItem {
                    expr: resolve_known_params_in_expr(item.expr),
                    alias: item.alias,
                    source_text: item.source_text,
                })
                .collect(),
        },
        LogicalPlan::Sort { input, items } => LogicalPlan::Sort {
            input: Box::new(resolve_known_params_in_plan(*input)),
            items,
        },
        LogicalPlan::Limit { input, count } => LogicalPlan::Limit {
            input: Box::new(resolve_known_params_in_plan(*input)),
            count,
        },
        LogicalPlan::Eager { input } => LogicalPlan::Eager {
            input: Box::new(resolve_known_params_in_plan(*input)),
        },
        LogicalPlan::Skip { input, count } => LogicalPlan::Skip {
            input: Box::new(resolve_known_params_in_plan(*input)),
            count,
        },
        LogicalPlan::Unwind {
            input,
            expr,
            alias,
            clear_source,
        } => LogicalPlan::Unwind {
            input: Box::new(resolve_known_params_in_plan(*input)),
            expr: resolve_known_params_in_expr(expr),
            alias,
            clear_source,
        },
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandOut {
            input: Box::new(resolve_known_params_in_plan(*input)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandIn {
            input: Box::new(resolve_known_params_in_plan(*input)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandBoth {
            input: Box::new(resolve_known_params_in_plan(*input)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        other => other,
    }
}

#[recursive::recursive]
fn resolve_known_params_in_expr(expr: Expr) -> Expr {
    match expr {
        Expr::Parameter(ref name) => match crate::query::param(name) {
            Some(crate::types::CypherValue::Integer(n)) => Expr::Literal(Literal::Integer(n)),
            Some(crate::types::CypherValue::Float(f)) => Expr::Literal(Literal::Float(f)),
            Some(crate::types::CypherValue::Bool(b)) => Expr::Literal(Literal::Bool(b)),
            Some(crate::types::CypherValue::String(s)) => Expr::Literal(Literal::String(s)),
            Some(crate::types::CypherValue::Null) => Expr::Literal(Literal::Null),
            _ => expr,
        },
        Expr::UnaryOp(op, inner) => {
            Expr::UnaryOp(op, Box::new(resolve_known_params_in_expr(*inner)))
        }
        Expr::BinaryOp(left, op, right) => Expr::BinaryOp(
            Box::new(resolve_known_params_in_expr(*left)),
            op,
            Box::new(resolve_known_params_in_expr(*right)),
        ),
        Expr::Comparison(first, rest) => Expr::Comparison(
            Box::new(resolve_known_params_in_expr(*first)),
            rest.into_iter()
                .map(|(op, e)| (op, resolve_known_params_in_expr(e)))
                .collect(),
        ),
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => Expr::FunctionCall {
            name,
            args: args.into_iter().map(resolve_known_params_in_expr).collect(),
            distinct,
        },
        other => other,
    }
}
