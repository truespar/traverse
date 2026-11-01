//! CREATE, RETURN and WITH: projections, aggregation, aliases and ORDER BY resolution.

use crate::ast::{
    BinOp, Expr, Literal, Pattern, ProjectionItem, ProjectionItems, RelDirection, ReturnClause,
    SortItem, WithClause,
};
use crate::error::CypherError;

use super::part::rand_id;
use super::plan::LogicalPlan;

pub(super) fn plan_create(
    pattern: &Pattern,
    mut plan: LogicalPlan,
    bound_vars: &mut std::collections::HashSet<String>,
) -> Result<LogicalPlan, CypherError> {
    for part in &pattern.parts {
        let elem = &part.element;
        let node = &elem.node;

        let from_var = node
            .variable
            .clone()
            .unwrap_or_else(|| format!("__anon_create_{}", rand_id()));

        // create the node only if its variable isn't bound
        if !bound_vars.contains(&from_var) {
            plan = LogicalPlan::CreateNode {
                input: Box::new(plan),
                variable: Some(from_var.clone()),
                labels: node.labels.clone(),
                dynamic_labels: node.dynamic_labels.clone(),
                properties: node.properties.clone(),
            };
            bound_vars.insert(from_var.clone());
        }

        let mut current_from = from_var;

        for chain in &elem.chain {
            let target = &chain.node;
            let rel = &chain.rel;

            let to_var = target
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_create_{}", rand_id()));

            // create the target only if its variable isn't bound
            if !bound_vars.contains(&to_var) {
                plan = LogicalPlan::CreateNode {
                    input: Box::new(plan),
                    variable: Some(to_var.clone()),
                    labels: target.labels.clone(),
                    dynamic_labels: target.dynamic_labels.clone(),
                    properties: target.properties.clone(),
                };
                bound_vars.insert(to_var.clone());
            }

            let edge_type = rel.types.first().cloned().unwrap_or_default();
            // dynamic edge type wins over static `edge_type`; CREATE takes one dynamic expression
            // (it creates one type)
            let dynamic_edge_type = rel.dynamic_types.first().cloned();

            // create the relationship given a static type OR a dynamic expression
            if !edge_type.is_empty() || dynamic_edge_type.is_some() {
                let (actual_from, actual_to) = match rel.direction {
                    RelDirection::Outgoing | RelDirection::Both => {
                        (current_from.clone(), to_var.clone())
                    }
                    RelDirection::Incoming => (to_var.clone(), current_from.clone()),
                };
                plan = LogicalPlan::CreateRel {
                    input: Box::new(plan),
                    variable: rel.variable.clone(),
                    from: actual_from,
                    to: actual_to,
                    dynamic_edge_type,
                    edge_type,
                    properties: rel.properties.clone(),
                };
            }

            current_from = to_var;
        }
    }
    Ok(plan)
}

pub(super) fn plan_return(
    ret: &ReturnClause,
    mut plan: LogicalPlan,
) -> Result<LogicalPlan, CypherError> {
    // aggregation?
    let has_agg = match &ret.items {
        ProjectionItems::All => false,
        ProjectionItems::Explicit(items) => items.iter().any(|i| expr_has_aggregation(&i.expr)),
    };

    // the key/agg split emits keys before aggregates; if RETURN listed an aggregate first, a slim
    // Project restores the user's column order at the end (Neo4j keeps it)
    let mut agg_reordered = false;
    if has_agg && let ProjectionItems::Explicit(items) = &ret.items {
        let mut keys = Vec::new();
        let mut aggs = Vec::new();
        let mut seen_agg = false;
        for item in items {
            if expr_has_aggregation(&item.expr) {
                seen_agg = true;
                aggs.push(item.clone());
            } else {
                if seen_agg {
                    agg_reordered = true;
                }
                keys.push(item.clone());
            }
        }
        plan = LogicalPlan::Aggregate {
            input: Box::new(plan),
            keys,
            aggs,
        };
    }

    // DISTINCT dedups BEFORE SKIP/LIMIT (project, dedup, order, paginate), else `RETURN DISTINCT x
    // LIMIT n` limits pre-distinct rows. Project first; ORDER BY then uses the projected columns
    // (all Neo4j allows after DISTINCT)
    let distinct_first = ret.distinct && !has_agg;
    if distinct_first {
        plan = LogicalPlan::Project {
            input: Box::new(plan),
            items: match &ret.items {
                ProjectionItems::All => Vec::new(),
                ProjectionItems::Explicit(items) => items.clone(),
            },
            distinct: true,
            where_clause: None,
        };
    }

    // sort BEFORE projection so ORDER BY can use pre-projection variables; a Variable naming a
    // projection alias is replaced by that projection's expression
    if let Some(ref order) = ret.order_by {
        let resolved_order: Vec<SortItem> = order
            .iter()
            .map(|item| {
                if let ProjectionItems::Explicit(ref proj_items) = ret.items {
                    let mut expr = resolve_aliases_in_expr(&item.expr, proj_items);
                    // after aggregation (or an early DISTINCT projection) ORDER BY expressions
                    // matching a projection item resolve to its output column
                    if has_agg || distinct_first {
                        expr = resolve_order_expr_to_column(&expr, proj_items);
                    }
                    SortItem {
                        expr,
                        ascending: item.ascending,
                    }
                } else {
                    item.clone()
                }
            })
            .collect();
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            items: resolved_order,
        };
    }
    if let Some(ref skip) = ret.skip {
        plan = LogicalPlan::Skip {
            input: Box::new(plan),
            count: skip.clone(),
        };
    }
    if let Some(ref limit) = ret.limit {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            count: limit.clone(),
        };
    }

    // project after sort/skip/limit, unless aggregation or DISTINCT already did
    if !has_agg && !distinct_first {
        plan = LogicalPlan::Project {
            input: Box::new(plan),
            items: match &ret.items {
                ProjectionItems::All => Vec::new(),
                ProjectionItems::Explicit(items) => items.clone(),
            },
            distinct: ret.distinct,
            where_clause: None,
        };
    } else if agg_reordered {
        // restore the RETURN's column order over the aggregate's keys-then-aggs layout, projecting
        // each column by name
        if let ProjectionItems::Explicit(items) = &ret.items {
            let reorder: Vec<ProjectionItem> = items
                .iter()
                .map(|item| {
                    let name = crate::executor::projection_name(item);
                    ProjectionItem {
                        expr: Expr::Variable(name.clone()),
                        alias: Some(name),
                        source_text: item.source_text.clone(),
                    }
                })
                .collect();
            plan = LogicalPlan::Project {
                input: Box::new(plan),
                items: reorder,
                distinct: false,
                where_clause: None,
            };
        }
    }

    Ok(plan)
}

pub(super) fn plan_with(
    with: &WithClause,
    mut plan: LogicalPlan,
) -> Result<LogicalPlan, CypherError> {
    let has_agg = match &with.items {
        ProjectionItems::All => false,
        ProjectionItems::Explicit(items) => items.iter().any(|i| expr_has_aggregation(&i.expr)),
    };

    if has_agg && let ProjectionItems::Explicit(items) = &with.items {
        let mut keys = Vec::new();
        let mut aggs = Vec::new();
        for item in items {
            if expr_has_aggregation(&item.expr) {
                aggs.push(item.clone());
            } else {
                keys.push(item.clone());
            }
        }
        plan = LogicalPlan::Aggregate {
            input: Box::new(plan),
            keys,
            aggs,
        };
    }

    // DISTINCT dedups BEFORE SKIP/LIMIT (see plan_return); otherwise `WITH DISTINCT x LIMIT n`
    // limits pre-distinct rows
    let distinct_first = with.distinct && !has_agg;
    if distinct_first {
        plan = LogicalPlan::Project {
            input: Box::new(plan),
            items: match &with.items {
                ProjectionItems::All => Vec::new(),
                ProjectionItems::Explicit(items) => items.clone(),
            },
            distinct: true,
            where_clause: with.where_clause.clone(),
        };
    }

    // sort BEFORE projection so ORDER BY can use pre-projection variables; a Variable naming a
    // projection alias is replaced by that projection's expression
    if let Some(ref order) = with.order_by {
        let resolved_order: Vec<SortItem> = order
            .iter()
            .map(|item| {
                if let ProjectionItems::Explicit(ref proj_items) = with.items {
                    let mut expr = resolve_aliases_in_expr(&item.expr, proj_items);
                    if has_agg || distinct_first {
                        expr = resolve_order_expr_to_column(&expr, proj_items);
                    }
                    SortItem {
                        expr,
                        ascending: item.ascending,
                    }
                } else {
                    item.clone()
                }
            })
            .collect();
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            items: resolved_order,
        };
    }
    if let Some(ref skip) = with.skip {
        plan = LogicalPlan::Skip {
            input: Box::new(plan),
            count: skip.clone(),
        };
    }
    if let Some(ref limit) = with.limit {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            count: limit.clone(),
        };
    }

    // project AFTER sort/skip/limit, unless aggregation or DISTINCT already did
    if !has_agg && !distinct_first {
        plan = LogicalPlan::Project {
            input: Box::new(plan),
            items: match &with.items {
                ProjectionItems::All => Vec::new(),
                ProjectionItems::Explicit(items) => items.clone(),
            },
            distinct: with.distinct,
            where_clause: with.where_clause.clone(),
        };
    }

    // aggregated WITH: WHERE goes after (it sees only post-aggregation scope)
    if has_agg && let Some(ref where_expr) = with.where_clause {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: where_expr.clone(),
        };
    }
    Ok(plan)
}

/// Contains an aggregate function?
#[recursive::recursive]
fn expr_has_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, args, .. } => {
            is_aggregate_function(name) || args.iter().any(expr_has_aggregation)
        }
        Expr::BinaryOp(l, _, r) => expr_has_aggregation(l) || expr_has_aggregation(r),
        Expr::UnaryOp(_, e) => expr_has_aggregation(e),
        Expr::Property(e, _) => expr_has_aggregation(e),
        Expr::Comparison(e, cmps) => {
            expr_has_aggregation(e) || cmps.iter().any(|(_, e)| expr_has_aggregation(e))
        }
        Expr::In(a, b) => expr_has_aggregation(a) || expr_has_aggregation(b),
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, e)| expr_has_aggregation(e)),
        Expr::ListLiteral(items) => items.iter().any(expr_has_aggregation),
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_ref().is_some_and(|o| expr_has_aggregation(o))
                || whens
                    .iter()
                    .any(|(w, t)| expr_has_aggregation(w) || expr_has_aggregation(t))
                || else_expr.as_ref().is_some_and(|e| expr_has_aggregation(e))
        }
        Expr::QuantifierExpr {
            list_expr,
            predicate,
            ..
        } => expr_has_aggregation(list_expr) || expr_has_aggregation(predicate),
        Expr::ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            expr_has_aggregation(list_expr)
                || filter.as_ref().is_some_and(|f| expr_has_aggregation(f))
                || map_expr.as_ref().is_some_and(|m| expr_has_aggregation(m))
        }
        Expr::ListIndex(base, idx) => expr_has_aggregation(base) || expr_has_aggregation(idx),
        Expr::ListSlice(base, from, to) => {
            expr_has_aggregation(base)
                || from.as_ref().is_some_and(|e| expr_has_aggregation(e))
                || to.as_ref().is_some_and(|e| expr_has_aggregation(e))
        }
        _ => false,
    }
}

/// Resolves alias references in ORDER BY expressions, recursively.
#[recursive::recursive]
fn resolve_aliases_in_expr(expr: &Expr, proj_items: &[ProjectionItem]) -> Expr {
    match expr {
        Expr::Variable(name) => {
            for pi in proj_items {
                if pi.alias.as_deref() == Some(name.as_str()) {
                    return pi.expr.clone();
                }
            }
            expr.clone()
        }
        Expr::BinaryOp(l, op, r) => Expr::BinaryOp(
            Box::new(resolve_aliases_in_expr(l, proj_items)),
            *op,
            Box::new(resolve_aliases_in_expr(r, proj_items)),
        ),
        Expr::UnaryOp(op, e) => {
            Expr::UnaryOp(*op, Box::new(resolve_aliases_in_expr(e, proj_items)))
        }
        Expr::Property(base, key) => Expr::Property(
            Box::new(resolve_aliases_in_expr(base, proj_items)),
            key.clone(),
        ),
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| resolve_aliases_in_expr(a, proj_items))
                .collect(),
            distinct: *distinct,
        },
        Expr::Comparison(base, pairs) => Expr::Comparison(
            Box::new(resolve_aliases_in_expr(base, proj_items)),
            pairs
                .iter()
                .map(|(op, e)| (*op, resolve_aliases_in_expr(e, proj_items)))
                .collect(),
        ),
        _ => expr.clone(),
    }
}

/// Replaces `expr` with a Variable for the column name when it structurally matches a projection
/// item, so ORDER BY works on aggregate expressions after an Aggregate.
#[recursive::recursive]
fn resolve_order_expr_to_column(expr: &Expr, proj_items: &[ProjectionItem]) -> Expr {
    // whole expression matches an item?
    let expr_key = simple_expr_key(expr);
    for pi in proj_items {
        if simple_expr_key(&pi.expr) == expr_key {
            let col_name = pi
                .alias
                .clone()
                .unwrap_or_else(|| simple_expr_key(&pi.expr));
            return Expr::Variable(col_name);
        }
    }
    // recurse into sub-expressions (a.name inside a.name + 'C')
    match expr {
        Expr::BinaryOp(l, op, r) => Expr::BinaryOp(
            Box::new(resolve_order_expr_to_column(l, proj_items)),
            *op,
            Box::new(resolve_order_expr_to_column(r, proj_items)),
        ),
        Expr::UnaryOp(op, e) => {
            Expr::UnaryOp(*op, Box::new(resolve_order_expr_to_column(e, proj_items)))
        }
        Expr::Property(base, key) => {
            // property expression matches an item?
            let prop_key = simple_expr_key(expr);
            for pi in proj_items {
                if simple_expr_key(&pi.expr) == prop_key {
                    let col_name = pi
                        .alias
                        .clone()
                        .unwrap_or_else(|| simple_expr_key(&pi.expr));
                    return Expr::Variable(col_name);
                }
            }
            Expr::Property(
                Box::new(resolve_order_expr_to_column(base, proj_items)),
                key.clone(),
            )
        }
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| resolve_order_expr_to_column(a, proj_items))
                .collect(),
            distinct: *distinct,
        },
        _ => expr.clone(),
    }
}

/// Simple canonical key for expression matching.
#[recursive::recursive]
pub(crate) fn simple_expr_key(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(base, key) => format!("{}.{key}", simple_expr_key(base)),
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
                let arg_keys: Vec<String> = args.iter().map(simple_expr_key).collect();
                if *distinct {
                    format!("{name}(DISTINCT {})", arg_keys.join(", "))
                } else {
                    format!("{name}({})", arg_keys.join(", "))
                }
            }
        }
        Expr::BinaryOp(l, op, r) => {
            let op_str = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Mod => "%",
                BinOp::And => "AND",
                BinOp::Or => "OR",
                BinOp::Xor => "XOR",
                BinOp::Pow => "^",
                BinOp::Concat => "||",
            };
            format!("{} {op_str} {}", simple_expr_key(l), simple_expr_key(r))
        }
        Expr::Literal(Literal::Integer(n)) => n.to_string(),
        Expr::Literal(Literal::Float(f)) => f.to_string(),
        Expr::Literal(Literal::String(s)) => format!("\"{s}\""),
        Expr::Literal(Literal::Bool(b)) => b.to_string(),
        Expr::Literal(Literal::Null) => "null".to_string(),
        _ => format!("{expr:?}"),
    }
}

/// Is `name` an aggregate function?
pub fn is_aggregate_function(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "collect"
            | "percentiledisc"
            | "percentilecont"
            | "stdev"
            | "stdevp"
            | "apoc.agg.first"
            | "apoc.agg.last"
            | "apoc.agg.median"
            | "apoc.agg.product"
            | "apoc.agg.statistics"
            | "apoc.agg.percentiles"
            | "apoc.agg.nth"
            | "apoc.agg.slice"
            | "apoc.agg.maxitems"
            | "apoc.agg.minitems"
            | "apoc.agg.graph"
    )
}
