//! The variables patterns, subqueries and expressions use.

use crate::ast::{Clause, Expr, Pattern, ProjectionItems, SingleQuery};

pub(super) fn collect_pattern_variables(pattern: &Pattern) -> Vec<String> {
    let mut vars = Vec::new();
    for part in &pattern.parts {
        if let Some(ref v) = part.element.node.variable {
            vars.push(v.clone());
        }
        for chain in &part.element.chain {
            if let Some(ref v) = chain.rel.variable {
                vars.push(v.clone());
            }
            if let Some(ref v) = chain.node.variable {
                vars.push(v.clone());
            }
            // QPP group variables (`((x)-[r]->(y)){m,n}`), bound as lists
            if let Some(ref qpp) = chain.rel.qpp_group {
                if let Some(ref v) = qpp.start.variable {
                    vars.push(v.clone());
                }
                if let Some(ref v) = qpp.end.variable {
                    vars.push(v.clone());
                }
            }
        }
        // multi-step group-variable lists (GQL unrolling)
        for (v, _) in &part.qpp_list_groups {
            vars.push(v.clone());
        }
    }
    vars
}

/// Every variable used in a full existential subquery (`EXISTS { ... }`). Conservative (all pattern
/// and expression variables) so filters on them aren't pushed down wrongly.
fn collect_full_subquery_variables(sq: &SingleQuery, out: &mut std::collections::HashSet<String>) {
    #[recursive::recursive]
    fn collect_clause_variables(clause: &Clause, out: &mut std::collections::HashSet<String>) {
        match clause {
            Clause::Match(mc) | Clause::OptionalMatch(mc) => {
                for v in collect_pattern_variables(&mc.pattern) {
                    out.insert(v);
                }
                if let Some(ref w) = mc.where_clause {
                    collect_expr_variables(w, out);
                }
            }
            Clause::Unwind(uw) => {
                collect_expr_variables(&uw.expr, out);
            }
            _ => {}
        }
    }
    fn collect_projection_variables(
        items: &ProjectionItems,
        out: &mut std::collections::HashSet<String>,
    ) {
        if let ProjectionItems::Explicit(items) = items {
            for item in items {
                collect_expr_variables(&item.expr, out);
            }
        }
    }
    for part in &sq.parts {
        for clause in &part.clauses {
            collect_clause_variables(clause, out);
        }
        collect_projection_variables(&part.with.items, out);
        if let Some(ref w) = part.with.where_clause {
            collect_expr_variables(w, out);
        }
    }
    for clause in &sq.final_part.clauses {
        collect_clause_variables(clause, out);
    }
    if let Some(ref rc) = sq.final_part.return_clause {
        collect_projection_variables(&rc.items, out);
    }
}

/// Every variable an expression references.
#[recursive::recursive]
pub(crate) fn collect_expr_variables(expr: &Expr, out: &mut std::collections::HashSet<String>) {
    match expr {
        Expr::Variable(name) => {
            out.insert(name.clone());
        }
        Expr::Property(base, _) => collect_expr_variables(base, out),
        Expr::Literal(_) | Expr::Parameter(_) => {}
        Expr::BinaryOp(l, _, r) => {
            collect_expr_variables(l, out);
            collect_expr_variables(r, out);
        }
        Expr::UnaryOp(_, e) => collect_expr_variables(e, out),
        Expr::Comparison(base, pairs) => {
            collect_expr_variables(base, out);
            for (_, e) in pairs {
                collect_expr_variables(e, out);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_expr_variables(a, out);
            }
        }
        Expr::ListLiteral(items) => {
            for item in items {
                collect_expr_variables(item, out);
            }
        }
        Expr::MapLiteral(entries) => {
            for (_, v) in entries {
                collect_expr_variables(v, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                collect_expr_variables(o, out);
            }
            for (w, t) in whens {
                collect_expr_variables(w, out);
                collect_expr_variables(t, out);
            }
            if let Some(e) = else_expr {
                collect_expr_variables(e, out);
            }
        }
        Expr::In(l, r) => {
            collect_expr_variables(l, out);
            collect_expr_variables(r, out);
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) => collect_expr_variables(e, out),
        Expr::IsTyped { expr, .. } => collect_expr_variables(expr, out),
        Expr::StringOp(l, _, r) => {
            collect_expr_variables(l, out);
            collect_expr_variables(r, out);
        }
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            collect_expr_variables(list_expr, out);
            if let Some(f) = filter {
                collect_expr_variables(f, out);
            }
            if let Some(m) = map_expr {
                collect_expr_variables(m, out);
            }
            out.insert(variable.clone());
        }
        Expr::ListIndex(base, idx) => {
            collect_expr_variables(base, out);
            collect_expr_variables(idx, out);
        }
        Expr::ListSlice(base, from, to) => {
            collect_expr_variables(base, out);
            if let Some(f) = from {
                collect_expr_variables(f, out);
            }
            if let Some(t) = to {
                collect_expr_variables(t, out);
            }
        }
        Expr::QuantifierExpr {
            list_expr,
            predicate,
            ..
        } => {
            collect_expr_variables(list_expr, out);
            collect_expr_variables(predicate, out);
        }
        Expr::ExistentialSubquery(mc) | Expr::PatternPredicate(mc) | Expr::CountSubquery(mc) => {
            for v in collect_pattern_variables(&mc.pattern) {
                out.insert(v);
            }
            if let Some(ref w) = mc.where_clause {
                collect_expr_variables(w, out);
            }
        }
        Expr::FullExistentialSubquery(sq) | Expr::CollectSubquery(sq) => {
            collect_full_subquery_variables(sq, out);
        }
        Expr::PatternComprehension {
            match_clause,
            projection,
            ..
        } => {
            for v in collect_pattern_variables(&match_clause.pattern) {
                out.insert(v);
            }
            if let Some(ref w) = match_clause.where_clause {
                collect_expr_variables(w, out);
            }
            collect_expr_variables(projection, out);
        }
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list_expr,
            update_expr,
        } => {
            out.insert(accumulator.clone());
            out.insert(variable.clone());
            collect_expr_variables(initial, out);
            collect_expr_variables(list_expr, out);
            collect_expr_variables(update_expr, out);
        }
        Expr::MapProjection {
            expr,
            literal_entries,
            ..
        } => {
            collect_expr_variables(expr, out);
            for (_, e) in literal_entries {
                collect_expr_variables(e, out);
            }
        }
    }
}

/// Index-lookup eligible: a literal or parameter, not a variable needing resolution.
pub(crate) fn is_indexable_value(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(_) | Expr::Parameter(_))
}
