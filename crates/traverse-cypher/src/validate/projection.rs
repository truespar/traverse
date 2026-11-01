//! WITH and RETURN: projections, aliases, ORDER BY scope, and aggregation rules (grouping
//! keys, nested and non-deterministic aggregates).

use std::collections::HashSet;

use crate::ast::{
    BinOp, Expr, Literal, ProjectionItem, ProjectionItems, ReturnClause, UnaryOp, WithClause,
};
use crate::error::CypherError;

use super::clause::validate_skip_limit;
use super::expr::{is_non_entity_literal, validate_expr, validate_expr_defined};
use super::{Scope, VarKind, expr_to_name};

// -- WITH validation --------------------------------------------------

pub(super) fn validate_with(with: &WithClause, prev_scope: &Scope) -> Result<Scope, CypherError> {
    let mut new_scope = Scope {
        has_clauses: true,
        ..Scope::default()
    };

    match &with.items {
        ProjectionItems::All => {
            new_scope.vars = prev_scope.vars.clone();
        }
        ProjectionItems::Explicit(items) => {
            let mut seen_aliases = HashSet::new();
            for item in items {
                validate_expr(&item.expr, prev_scope)?;
                // like RETURN: value positions error
                if pattern_predicate_in_value_position(&item.expr) {
                    return Err(CypherError::syntax(
                        "Pattern expressions are not allowed in a WITH clause \
                         (wrap in exists() or use a boolean context)"
                            .to_string(),
                        0,
                    ));
                }
                // WITH needs aliases for anything but variables and properties
                if item.alias.is_none()
                    && !matches!(&item.expr, Expr::Variable(_) | Expr::Property(..))
                {
                    return Err(CypherError::syntax(
                        "Expression in WITH must be aliased (use AS)".to_string(),
                        0,
                    ));
                }
                let alias = item
                    .alias
                    .clone()
                    .unwrap_or_else(|| expr_to_name(&item.expr));
                // duplicates only matter for explicit aliases
                if item.alias.is_some() && !seen_aliases.insert(alias.clone()) {
                    return Err(CypherError::syntax(
                        format!(
                            "Multiple result columns with the same name are not supported: `{alias}`"
                        ),
                        0,
                    ));
                }
                let kind = match &item.expr {
                    Expr::Variable(v) => prev_scope
                        .vars
                        .get(v.as_str())
                        .copied()
                        .unwrap_or(VarKind::Scalar),
                    e if is_non_entity_literal(e) => VarKind::LiteralScalar,
                    Expr::ListLiteral(_) => VarKind::ListScalar,
                    Expr::MapLiteral(_) => VarKind::MapScalar,
                    _ => VarKind::Scalar,
                };
                new_scope.vars.insert(alias, kind);
            }
            // ambiguous aggregation across WITH items
            check_projection_aggregation(items)?;
        }
    }

    if let Some(ref order_items) = with.order_by {
        // ORDER BY in WITH sees pre- and post-projection variables
        let mut order_scope = Scope {
            vars: prev_scope.vars.clone(),
            has_clauses: true,
        };
        for (k, v) in &new_scope.vars {
            order_scope.vars.insert(k.clone(), *v);
        }
        let with_has_agg = match &with.items {
            ProjectionItems::All => false,
            ProjectionItems::Explicit(items) => items.iter().any(|pi| expr_contains_agg(&pi.expr)),
        };
        for sort_item in order_items {
            if !with_has_agg {
                validate_expr_defined(&sort_item.expr, &order_scope)?;
                if expr_contains_agg(&sort_item.expr) {
                    return Err(CypherError::syntax(
                        "In a WITH/RETURN with no aggregation, it is not possible to sort on an aggregation function"
                            .to_string(),
                        0,
                    ).with_status(crate::status::GqlStatus::InvalidAggregation));
                }
            }
            // ambiguous aggregation in ORDER BY vs WITH grouping keys
            if expr_contains_agg(&sort_item.expr) {
                let grouping_keys = if let ProjectionItems::Explicit(items) = &with.items {
                    collect_grouping_keys(items)
                } else {
                    HashSet::new()
                };
                let mut non_agg_refs = Vec::new();
                collect_non_agg_refs(&sort_item.expr, &mut non_agg_refs);
                for ref_key in &non_agg_refs {
                    if !grouping_keys.contains(ref_key) {
                        return Err(CypherError::syntax(
                            "Ambiguous aggregation expression in ORDER BY".to_string(),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidAggregation));
                    }
                }
                // aggregating WITH: ORDER BY aggregates must match projected ones (no new
                // aggregates)
                if with_has_agg && let ProjectionItems::Explicit(items) = &with.items {
                    let projected_agg_keys: HashSet<String> = items
                        .iter()
                        .filter(|pi| expr_contains_agg(&pi.expr))
                        .map(|pi| simple_expr_key(&pi.expr))
                        .collect();
                    let mut order_agg_keys = Vec::new();
                    collect_agg_expr_keys(&sort_item.expr, &mut order_agg_keys);
                    for key in &order_agg_keys {
                        if !projected_agg_keys.contains(key) {
                            return Err(CypherError::syntax(
                                format!("Variable `{key}` not defined"),
                                0,
                            )
                            .with_status(crate::status::GqlStatus::VariableNotDefined));
                        }
                    }
                }
            }
        }
    }

    if let Some(ref skip_expr) = with.skip {
        validate_skip_limit(skip_expr, "SKIP")?;
    }
    if let Some(ref limit_expr) = with.limit {
        validate_skip_limit(limit_expr, "LIMIT")?;
    }

    if let Some(ref where_expr) = with.where_clause {
        // non-aggregating WITH applies WHERE before projection, so it sees the previous scope
        let with_has_agg = match &with.items {
            ProjectionItems::All => false,
            ProjectionItems::Explicit(items) => items.iter().any(|pi| expr_contains_agg(&pi.expr)),
        };
        if with_has_agg {
            validate_expr(where_expr, &new_scope)?;
        } else {
            // WHERE validates over prev_scope + new_scope
            let mut where_scope = Scope {
                vars: prev_scope.vars.clone(),
                has_clauses: true,
            };
            for (k, v) in &new_scope.vars {
                where_scope.vars.insert(k.clone(), *v);
            }
            validate_expr(where_expr, &where_scope)?;
        }
    }

    Ok(new_scope)
}

// -- RETURN validation ------------------------------------------------

pub(super) fn validate_return(rc: &ReturnClause, scope: &Scope) -> Result<(), CypherError> {
    match &rc.items {
        ProjectionItems::All => {
            if scope.vars.is_empty() {
                return Err(CypherError::syntax(
                    "RETURN * is not allowed when there are no variables in scope".to_string(),
                    0,
                ));
            }
        }
        ProjectionItems::Explicit(items) => {
            let mut seen_aliases = HashSet::new();
            for item in items {
                validate_expr(&item.expr, scope)?;
                // Neo4j 5: pattern expressions are boolean predicates, legal in BOOLEAN positions
                // (not(...), and/or/xor, searched-CASE conditions, comprehension filters), illegal
                // in VALUE positions
                if pattern_predicate_in_value_position(&item.expr) {
                    return Err(CypherError::syntax(
                        "Pattern expressions are not allowed in a RETURN clause \
                         (wrap in exists() or use a boolean context)"
                            .to_string(),
                        0,
                    ));
                }
                // nested aggregation (count(count(*)))
                if expr_contains_nested_agg(&item.expr) {
                    return Err(CypherError::syntax(
                        "Can't use aggregate functions inside of aggregate functions".to_string(),
                        0,
                    )
                    .with_status(crate::status::GqlStatus::InvalidAggregation));
                }
                // duplicates only matter for explicit aliases
                if let Some(ref alias) = item.alias
                    && !seen_aliases.insert(alias.clone())
                {
                    return Err(CypherError::syntax(
                        format!(
                            "Multiple result columns with the same name are not supported: `{alias}`"
                        ),
                        0,
                    ));
                }
            }
            // ambiguous aggregation across projection items
            check_projection_aggregation(items)?;
        }
    }

    if let Some(ref skip_expr) = rc.skip {
        validate_skip_limit(skip_expr, "SKIP")?;
    }
    if let Some(ref limit_expr) = rc.limit {
        validate_skip_limit(limit_expr, "LIMIT")?;
    }

    if let Some(ref order_items) = rc.order_by {
        let ret_has_agg = match &rc.items {
            ProjectionItems::All => false,
            ProjectionItems::Explicit(items) => items.iter().any(|pi| expr_contains_agg(&pi.expr)),
        };
        // with DISTINCT, ORDER BY may only use projected columns or properties of projected entity
        // variables
        if rc.distinct
            && let ProjectionItems::Explicit(items) = &rc.items
        {
            let projected = collect_grouping_keys(items);
            // base names of entity projections (bare variables)
            let mut projected_entity_vars = HashSet::new();
            for pi in items {
                if let Expr::Variable(v) = &pi.expr {
                    projected_entity_vars.insert(v.clone());
                }
                // an alias of a bare variable is an entity too
                if matches!(&pi.expr, Expr::Variable(_))
                    && let Some(ref alias) = pi.alias
                {
                    projected_entity_vars.insert(alias.clone());
                }
            }
            for sort_item in order_items {
                let mut refs = Vec::new();
                collect_non_agg_refs(&sort_item.expr, &mut refs);
                for ref_key in &refs {
                    if projected.contains(ref_key) {
                        continue;
                    }
                    // property access on projected entities is fine
                    let base = ref_key.split('.').next().unwrap_or(ref_key);
                    if ref_key.contains('.') && projected_entity_vars.contains(base) {
                        continue;
                    }
                    return Err(CypherError::syntax(
                        format!(
                            "In a RETURN with DISTINCT, ORDER BY expression `{ref_key}` \
                                 is not part of the RETURN projection"
                        ),
                        0,
                    ));
                }
            }
        }
        for sort_item in order_items {
            if !ret_has_agg && expr_contains_agg(&sort_item.expr) {
                return Err(CypherError::syntax(
                    "In a WITH/RETURN with no aggregation, it is not possible to sort on an aggregation function"
                        .to_string(),
                    0,
                ).with_status(crate::status::GqlStatus::InvalidAggregation));
            }
            // ambiguous aggregation in ORDER BY vs projection grouping keys
            if expr_contains_agg(&sort_item.expr) {
                let grouping_keys = if let ProjectionItems::Explicit(items) = &rc.items {
                    collect_grouping_keys(items)
                } else {
                    HashSet::new()
                };
                let mut non_agg_refs = Vec::new();
                collect_non_agg_refs(&sort_item.expr, &mut non_agg_refs);
                for ref_key in &non_agg_refs {
                    if !grouping_keys.contains(ref_key) {
                        return Err(CypherError::syntax(
                            "Ambiguous aggregation expression in ORDER BY".to_string(),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidAggregation));
                    }
                }
            }
        }
    }

    Ok(())
}

// -- Aggregation detection --------------------------------------------

#[recursive::recursive]
pub(super) fn expr_contains_agg(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, args, .. } => {
            if is_agg_function(name) {
                return true;
            }
            args.iter().any(expr_contains_agg)
        }
        Expr::BinaryOp(l, _, r) => expr_contains_agg(l) || expr_contains_agg(r),
        Expr::UnaryOp(_, e) => expr_contains_agg(e),
        Expr::Property(base, _) => expr_contains_agg(base),
        Expr::Comparison(base, pairs) => {
            expr_contains_agg(base) || pairs.iter().any(|(_, e)| expr_contains_agg(e))
        }
        _ => false,
    }
}

/// Non-aggregated variable/property references, not descending into aggregate calls.
#[recursive::recursive]
fn collect_non_agg_refs(expr: &Expr, refs: &mut Vec<String>) {
    match expr {
        Expr::FunctionCall { name, .. } if is_agg_function(name) => {}
        Expr::Variable(v) => refs.push(v.clone()),
        Expr::Property(base, key) => {
            // "base.key" form, for grouping key comparison
            if let Expr::Variable(v) = base.as_ref() {
                refs.push(format!("{v}.{key}"));
            } else {
                collect_non_agg_refs(base, refs);
            }
        }
        Expr::BinaryOp(l, _, r) => {
            collect_non_agg_refs(l, refs);
            collect_non_agg_refs(r, refs);
        }
        Expr::UnaryOp(_, e) => collect_non_agg_refs(e, refs),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_non_agg_refs(a, refs);
            }
        }
        Expr::Comparison(base, pairs) => {
            collect_non_agg_refs(base, refs);
            for (_, e) in pairs {
                collect_non_agg_refs(e, refs);
            }
        }
        Expr::ListIndex(base, idx) => {
            collect_non_agg_refs(base, refs);
            collect_non_agg_refs(idx, refs);
        }
        Expr::ListSlice(base, from, to) => {
            collect_non_agg_refs(base, refs);
            if let Some(e) = from {
                collect_non_agg_refs(e, refs);
            }
            if let Some(e) = to {
                collect_non_agg_refs(e, refs);
            }
        }
        _ => {}
    }
}

/// Simple string key of an expression, to match ORDER BY aggregates with projected ones.
#[recursive::recursive]
fn simple_expr_key(expr: &Expr) -> String {
    match expr {
        Expr::Variable(v) => v.clone(),
        Expr::Property(base, key) => format!("{}.{key}", simple_expr_key(base)),
        Expr::FunctionCall { name, args, .. } => {
            let arg_keys: Vec<String> = args.iter().map(simple_expr_key).collect();
            format!("{name}({})", arg_keys.join(","))
        }
        Expr::BinaryOp(l, op, r) => {
            format!("({}{:?}{})", simple_expr_key(l), op, simple_expr_key(r))
        }
        Expr::UnaryOp(op, e) => format!("{op:?}{}", simple_expr_key(e)),
        Expr::Literal(lit) => match lit {
            Literal::Null => "null".to_string(),
            Literal::Bool(b) => b.to_string(),
            Literal::Integer(i) => i.to_string(),
            Literal::Float(f) => f.to_string(),
            Literal::String(s) => format!("'{s}'"),
        },
        _ => format!("{expr:?}"),
    }
}

/// String keys of the aggregate calls in an expression.
#[recursive::recursive]
fn collect_agg_expr_keys(expr: &Expr, keys: &mut Vec<String>) {
    match expr {
        Expr::FunctionCall { name, args, .. } if is_agg_function(name) => {
            keys.push(simple_expr_key(expr));
            // aggregate args are part of the key: don't recurse
        }
        Expr::BinaryOp(l, _, r) => {
            collect_agg_expr_keys(l, keys);
            collect_agg_expr_keys(r, keys);
        }
        Expr::UnaryOp(_, e) => collect_agg_expr_keys(e, keys),
        Expr::Property(base, _) => collect_agg_expr_keys(base, keys),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_agg_expr_keys(a, keys);
            }
        }
        Expr::ListIndex(base, idx) => {
            collect_agg_expr_keys(base, keys);
            collect_agg_expr_keys(idx, keys);
        }
        Expr::ListSlice(base, from, to) => {
            collect_agg_expr_keys(base, keys);
            if let Some(e) = from {
                collect_agg_expr_keys(e, keys);
            }
            if let Some(e) = to {
                collect_agg_expr_keys(e, keys);
            }
        }
        _ => {}
    }
}

/// String key of an expression for grouping key comparison.
fn expr_grouping_key(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Variable(v) => Some(v.clone()),
        Expr::Property(base, key) => {
            if let Expr::Variable(v) = base.as_ref() {
                Some(format!("{v}.{key}"))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Grouping keys of projection items (non-aggregates), expressions and aliases.
fn collect_grouping_keys(items: &[ProjectionItem]) -> HashSet<String> {
    let mut keys = HashSet::new();
    for item in items {
        if !expr_contains_agg(&item.expr) {
            if let Some(k) = expr_grouping_key(&item.expr) {
                keys.insert(k);
            }
            // the alias counts too
            if let Some(ref alias) = item.alias {
                keys.insert(alias.clone());
            }
        }
    }
    keys
}

/// Errors on ambiguous aggregation: an item mixing aggregation with non-grouped references.
fn check_projection_aggregation(items: &[ProjectionItem]) -> Result<(), CypherError> {
    let has_any_agg = items.iter().any(|pi| expr_contains_agg(&pi.expr));
    if !has_any_agg {
        return Ok(());
    }

    let grouping_keys = collect_grouping_keys(items);

    // non-agg refs of each aggregating item must be grouping keys
    for item in items {
        if !expr_contains_agg(&item.expr) {
            continue;
        }
        let mut non_agg_refs = Vec::new();
        collect_non_agg_refs(&item.expr, &mut non_agg_refs);
        if non_agg_refs.is_empty() {
            continue;
        }
        for ref_key in &non_agg_refs {
            if !grouping_keys.contains(ref_key) {
                return Err(CypherError::syntax(
                    "Ambiguous aggregation expression: contains both aggregation and non-aggregated variables"
                        .to_string(),
                    0,
                ).with_status(crate::status::GqlStatus::InvalidAggregation));
            }
        }
    }

    Ok(())
}

/// Contains an `Expr::PatternPredicate`? Neo4j 5 allows pattern expressions outside WHERE only in
/// BOOLEAN positions (not(), and/or/xor, searched-CASE conditions, comprehension filters); VALUE
/// positions (projections, function args, arithmetic, CASE branches) are syntax errors.
pub(super) fn pattern_predicate_in_value_position(expr: &Expr) -> bool {
    #[recursive::recursive]
    fn walk(e: &Expr, boolean_ok: bool) -> bool {
        match e {
            Expr::PatternPredicate(_) => !boolean_ok,
            Expr::UnaryOp(UnaryOp::Not, inner) => walk(inner, true),
            Expr::BinaryOp(l, BinOp::And | BinOp::Or | BinOp::Xor, r) => {
                walk(l, true) || walk(r, true)
            }
            Expr::BinaryOp(l, _, r) => walk(l, false) || walk(r, false),
            Expr::UnaryOp(_, inner)
            | Expr::Property(inner, _)
            | Expr::IsNull(inner)
            | Expr::IsNotNull(inner) => walk(inner, false),
            Expr::FunctionCall { args, .. } => args.iter().any(|a| walk(a, false)),
            Expr::Comparison(base, pairs) => {
                walk(base, false) || pairs.iter().any(|(_, e)| walk(e, false))
            }
            Expr::ListLiteral(items) => items.iter().any(|i| walk(i, false)),
            Expr::Case {
                operand,
                whens,
                else_expr,
            } => {
                let searched = operand.is_none();
                operand.as_ref().is_some_and(|o| walk(o, false))
                    || whens
                        .iter()
                        .any(|(w, t)| walk(w, searched) || walk(t, false))
                    || else_expr.as_ref().is_some_and(|e| walk(e, false))
            }
            Expr::ListComprehension {
                list_expr,
                filter,
                map_expr,
                ..
            } => {
                walk(list_expr, false)
                    || filter.as_ref().is_some_and(|f| walk(f, true))
                    || map_expr.as_ref().is_some_and(|m| walk(m, false))
            }
            _ => false,
        }
    }
    walk(expr, false)
}

#[recursive::recursive]
fn expr_contains_pattern_predicate(expr: &Expr) -> bool {
    match expr {
        Expr::PatternPredicate(_) => true,
        Expr::BinaryOp(l, _, r) => {
            expr_contains_pattern_predicate(l) || expr_contains_pattern_predicate(r)
        }
        Expr::UnaryOp(_, e) | Expr::Property(e, _) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
            expr_contains_pattern_predicate(e)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_pattern_predicate),
        Expr::Comparison(base, pairs) => {
            expr_contains_pattern_predicate(base)
                || pairs
                    .iter()
                    .any(|(_, e)| expr_contains_pattern_predicate(e))
        }
        Expr::ListLiteral(items) => items.iter().any(expr_contains_pattern_predicate),
        _ => false,
    }
}

/// Nested aggregation (`count(count(*))`)?
#[recursive::recursive]
fn expr_contains_nested_agg(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, args, .. } if is_agg_function(name) => {
            // any argument aggregating?
            args.iter().any(expr_contains_agg)
        }
        Expr::BinaryOp(l, _, r) => expr_contains_nested_agg(l) || expr_contains_nested_agg(r),
        Expr::UnaryOp(_, e) | Expr::Property(e, _) => expr_contains_nested_agg(e),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_nested_agg),
        Expr::Comparison(base, pairs) => {
            expr_contains_nested_agg(base) || pairs.iter().any(|(_, e)| expr_contains_nested_agg(e))
        }
        _ => false,
    }
}

#[recursive::recursive]
pub(super) fn expr_contains_nondeterministic(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, .. } if name == "rand" || name == "timestamp" => true,
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_nondeterministic),
        Expr::BinaryOp(l, _, r) => {
            expr_contains_nondeterministic(l) || expr_contains_nondeterministic(r)
        }
        Expr::UnaryOp(_, e) | Expr::Property(e, _) => expr_contains_nondeterministic(e),
        _ => false,
    }
}

pub(super) fn is_agg_function(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "collect"
            | "stdev"
            | "stdevp"
            | "percentilecont"
            | "percentiledisc"
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

/// Label check (`__has_label(n, "Person")`)?
pub(super) fn is_label_check(expr: &Expr) -> bool {
    matches!(expr, Expr::FunctionCall { name, .. } if name == "__has_label")
}

/// Pure arithmetic on literals (`1 + 1`), which can't be a node/relationship/path?
#[recursive::recursive]
pub(super) fn is_pure_arithmetic(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(Literal::Integer(_) | Literal::Float(_)) => true,
        Expr::BinaryOp(l, op, r) => {
            matches!(
                op,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow
            ) && is_pure_arithmetic(l)
                && is_pure_arithmetic(r)
        }
        Expr::UnaryOp(UnaryOp::Neg, inner) => is_pure_arithmetic(inner),
        _ => false,
    }
}
