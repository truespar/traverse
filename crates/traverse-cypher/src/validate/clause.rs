//! Clauses: the per-clause checks, CALL (procedure signatures and YIELD), SET, SKIP and LIMIT.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    CallClause, Clause, Expr, Literal, ProjectionItems, SetItem, UnaryOp, YieldItems,
};
use crate::error::CypherError;

use super::expr::validate_expr;
use super::pattern::{
    validate_create_pattern, validate_match_hint, validate_match_pattern, validate_merge_pattern,
};
use super::projection::{
    expr_contains_agg, is_label_check, is_pure_arithmetic, pattern_predicate_in_value_position,
};
use super::{Scope, VarKind};

// -- Clause validation ------------------------------------------------

#[recursive::recursive]
pub(super) fn validate_clause(
    clause: &Clause,
    scope: &mut Scope,
    is_standalone: bool,
) -> Result<(), CypherError> {
    scope.has_clauses = true;
    match clause {
        Clause::Match(mc) | Clause::OptionalMatch(mc) => {
            validate_match_pattern(&mc.pattern, scope)?;
            // USING INDEX / USING SCAN: the variable must be bound by the pattern with the hint's
            // label (index existence is checked at plan time, with the graph)
            for h in &mc.hints {
                validate_match_hint(h, &mc.pattern)?;
            }
            if let Some(ref search) = mc.search {
                // the SEARCH binding must be a variable of this pattern (shape checked at plan
                // time)
                if !scope.vars.contains_key(search.binding.as_str()) {
                    return Err(CypherError::syntax(
                        format!("Variable {} not defined", search.binding),
                        0,
                    )
                    .with_status(crate::status::GqlStatus::VariableNotDefined));
                }
                validate_expr(&search.query_vector, scope)?;
                validate_expr(&search.limit, scope)?;
                if let Some(ref w) = search.index_where {
                    validate_expr(w, scope)?;
                }
                // `SCORE AS alias` adds a float variable
                if let Some(ref alias) = search.score_alias {
                    scope.vars.insert(alias.clone(), VarKind::Scalar);
                }
            }
            if let Some(ref where_expr) = mc.where_clause {
                // no aggregation in WHERE
                if expr_contains_agg(where_expr) {
                    return Err(CypherError::syntax(
                        "Aggregation functions are not allowed in WHERE".to_string(),
                        0,
                    )
                    .with_status(crate::status::GqlStatus::InvalidAggregation));
                }
                // a bare node/relationship variable isn't a boolean predicate
                if let Expr::Variable(v) = where_expr
                    && let Some(&kind) = scope.vars.get(v.as_str())
                    && matches!(kind, VarKind::Node | VarKind::Relationship)
                {
                    return Err(CypherError::syntax(
                        format!("Type mismatch: expected Boolean but got {kind:?}"),
                        0,
                    ));
                }
                validate_expr(where_expr, scope)?;
            }
        }
        Clause::Create(cc) => validate_create_pattern(&cc.pattern, scope)?,
        Clause::Merge(mc) => {
            validate_merge_pattern(&mc.pattern, scope)?;
            // ON CREATE SET / ON MATCH SET expressions
            if let Some(ref on_create) = mc.on_create {
                validate_set_items(&on_create.items, scope)?;
            }
            if let Some(ref on_match) = mc.on_match {
                validate_set_items(&on_match.items, scope)?;
            }
        }
        Clause::Delete(dc) => {
            for expr in &dc.exprs {
                validate_expr(expr, scope)?;
                // no DELETE of literal arithmetic (DELETE 1 + 1)
                if is_pure_arithmetic(expr) {
                    return Err(CypherError::syntax(
                        "DELETE requires a node, relationship, or path expression".to_string(),
                        0,
                    ));
                }
                // no DELETE of label checks (DELETE n:Person)
                if is_label_check(expr) {
                    return Err(CypherError::syntax(
                        "Invalid delete of label expression".to_string(),
                        0,
                    ));
                }
            }
        }
        Clause::Set(sc) => {
            validate_set_items(&sc.items, scope)?;
        }
        Clause::Remove(_) => {}
        Clause::Unwind(uc) => {
            validate_expr(&uc.expr, scope)?;
            scope.vars.insert(uc.alias.clone(), VarKind::Scalar);
        }
        Clause::ForEach(fc) => {
            // the list is evaluated in the outer scope; the loop variable and body bindings stay in
            // an inner scope (FOREACH exports nothing)
            validate_expr(&fc.list, scope)?;
            let mut inner = Scope {
                vars: scope.vars.clone(),
                has_clauses: scope.has_clauses,
            };
            inner.vars.insert(fc.variable.clone(), VarKind::Scalar);
            for c in &fc.updates {
                validate_clause(c, &mut inner, false)?;
            }
        }
        Clause::LoadCsv(lc) => {
            validate_expr(&lc.url, scope)?;
            scope.vars.insert(lc.row_alias.clone(), VarKind::Scalar);
        }
        Clause::Call(cc) => {
            validate_call_clause(cc, scope, is_standalone)?;
        }
        Clause::CallSubquery(cs) => {
            // the body is a full SingleQuery, validated in an inner scope: imports / outer bindings
            // visible, body-local ones don't leak. Its final RETURN's variables join the outer
            // scope afterwards (ISO section 9.4)
            let mut inner_vars: HashMap<String, VarKind> = match &cs.imports {
                Some(imports) => scope
                    .vars
                    .iter()
                    .filter(|(name, _)| imports.iter().any(|i| i == *name))
                    .map(|(n, k)| (n.clone(), *k))
                    .collect(),
                None => scope.vars.clone(),
            };
            let mut inner_scope = Scope {
                vars: std::mem::take(&mut inner_vars),
                has_clauses: false,
            };
            for part in &cs.body.single.parts {
                for c in &part.clauses {
                    validate_clause(c, &mut inner_scope, false)?;
                }
            }
            for c in &cs.body.single.final_part.clauses {
                validate_clause(c, &mut inner_scope, false)?;
            }
            // UNION branches validate in their OWN scope (imports visible, sibling bindings not)
            for u in &cs.body.unions {
                let mut branch_scope = Scope {
                    vars: inner_scope.vars.clone(),
                    has_clauses: false,
                };
                for part in &u.query.parts {
                    for c in &part.clauses {
                        validate_clause(c, &mut branch_scope, false)?;
                    }
                }
                for c in &u.query.final_part.clauses {
                    validate_clause(c, &mut branch_scope, false)?;
                }
            }
            // RETURN columns of the body enter the outer scope: aliased items (`RETURN x*2 AS d`)
            // and bare variables (`RETURN x`), keeping the source kind of a returned bound variable
            // so `n.prop` on a returned node validates
            if let Some(ref ret) = cs.body.single.final_part.return_clause
                && let ProjectionItems::Explicit(items) = &ret.items
            {
                for item in items {
                    if let Some(ref alias) = item.alias {
                        let kind = if let Expr::Variable(v) = &item.expr {
                            inner_scope.vars.get(v).copied().unwrap_or(VarKind::Scalar)
                        } else {
                            VarKind::Scalar
                        };
                        scope.vars.insert(alias.clone(), kind);
                    } else if let Expr::Variable(v) = &item.expr {
                        let kind = inner_scope.vars.get(v).copied().unwrap_or(VarKind::Scalar);
                        scope.vars.insert(v.clone(), kind);
                    }
                }
            }
            // `... IN TRANSACTIONS REPORT STATUS AS s` binds `s` (status map) outside
            if let Some(it) = &cs.in_transactions
                && let Some(rs) = &it.report_status
            {
                scope.vars.insert(rs.clone(), VarKind::Scalar);
            }
        }
    }
    Ok(())
}

// -- CALL clause validation -------------------------------------------

fn validate_call_clause(
    cc: &CallClause,
    scope: &mut Scope,
    is_standalone: bool,
) -> Result<(), CypherError> {
    use crate::procedure;

    let is_in_query = !is_standalone;

    // algorithm procedures (`traverse.<algo>.<mode>`) check their args at runtime against the
    // algorithm's config schema; skip the data-driven checks (CallProcedureOp errors clearly on
    // unknown algorithms)
    if cc.procedure_name.starts_with("traverse.")
        || cc.procedure_name.starts_with("gds.")
        || cc.procedure_name.starts_with("db.index.vector.")
        || cc.procedure_name.starts_with("db.index.fulltext.")
        || procedure::builtin_schema_proc_columns(&cc.procedure_name).is_some()
    {
        // runtime-dispatched procedures (algorithms, vector-index queries): validate arg
        // expressions, not arity/type (CallProcedureOp errors clearly)
        if let Some(ref args) = cc.args {
            for arg in args {
                if expr_contains_agg(arg) {
                    return Err(CypherError::syntax(
                        "InvalidAggregation: aggregation functions not allowed in CALL arguments",
                        0,
                    )
                    .with_status(crate::status::GqlStatus::InvalidAggregation));
                }
                validate_expr(arg, scope)?;
            }
        }
        // bind YIELD aliases into the scope
        if let Some(YieldItems::Explicit(items)) = &cc.yield_items {
            for it in items {
                let bound = it.alias.clone().unwrap_or_else(|| it.name.clone());
                scope.bind(&bound, VarKind::Scalar)?;
            }
        }
        return Ok(());
    }

    // 1. procedure must exist
    let proc_def = match procedure::get_procedure(&cc.procedure_name) {
        Some(p) => p,
        None => {
            return Err(CypherError::syntax(
                format!("ProcedureNotFound: {}", cc.procedure_name),
                0,
            )
            .with_status(crate::status::GqlStatus::UnknownProcedure));
        }
    };

    // 2. in-query + implicit args + params -> InvalidArgumentPassingMode
    if is_in_query && cc.args.is_none() && !proc_def.params.is_empty() {
        return Err(CypherError::syntax(
            "InvalidArgumentPassingMode: implicit argument passing not allowed in in-query CALL",
            0,
        ));
    }

    // 3. in-query YIELD * -> UnexpectedSyntax
    if is_in_query && let Some(YieldItems::All) = &cc.yield_items {
        return Err(CypherError::syntax(
            "YIELD * is not allowed in in-query CALL".to_string(),
            0,
        ));
    }

    // 4. explicit arg count = param count
    if let Some(ref args) = cc.args {
        if args.len() != proc_def.params.len() {
            return Err(CypherError::syntax(
                format!(
                    "InvalidNumberOfArguments: expected {}, got {}",
                    proc_def.params.len(),
                    args.len()
                ),
                0,
            ));
        }
        // 5. type-check literal arguments
        for (arg, param) in args.iter().zip(proc_def.params.iter()) {
            // no aggregates in args
            if expr_contains_agg(arg) {
                return Err(CypherError::syntax(
                    "InvalidAggregation: aggregation functions not allowed in CALL arguments",
                    0,
                )
                .with_status(crate::status::GqlStatus::InvalidAggregation));
            }
            match arg {
                Expr::Literal(Literal::Integer(_)) => {
                    if !matches!(
                        param.typ,
                        procedure::ProcType::Integer
                            | procedure::ProcType::Number
                            | procedure::ProcType::Float
                    ) {
                        return Err(CypherError::syntax(
                            format!(
                                "InvalidArgumentType: expected {} for parameter `{}`",
                                proc_type_name(&param.typ),
                                param.name
                            ),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidValueType));
                    }
                }
                Expr::Literal(Literal::Float(_)) => {
                    if !matches!(
                        param.typ,
                        procedure::ProcType::Float | procedure::ProcType::Number
                    ) {
                        return Err(CypherError::syntax(
                            format!(
                                "InvalidArgumentType: expected {} for parameter `{}`",
                                proc_type_name(&param.typ),
                                param.name
                            ),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidValueType));
                    }
                }
                Expr::Literal(Literal::String(_)) => {
                    if !matches!(param.typ, procedure::ProcType::String) {
                        return Err(CypherError::syntax(
                            format!(
                                "InvalidArgumentType: expected {} for parameter `{}`",
                                proc_type_name(&param.typ),
                                param.name
                            ),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidValueType));
                    }
                }
                Expr::Literal(Literal::Null) => {} // null is always valid
                Expr::Literal(Literal::Bool(_)) => {
                    // Bool matches no procedure type
                    return Err(CypherError::syntax(
                        format!(
                            "InvalidArgumentType: expected {} for parameter `{}`",
                            proc_type_name(&param.typ),
                            param.name
                        ),
                        0,
                    )
                    .with_status(crate::status::GqlStatus::InvalidValueType));
                }
                _ => {
                    validate_expr(arg, scope)?;
                }
            }
        }
    }

    // 7. implicit args: a query param per proc param
    if cc.args.is_none() {
        let param_keys = crate::executor::get_query_param_keys();
        for param in &proc_def.params {
            if !param_keys.contains(&param.name) {
                return Err(CypherError::syntax(
                    format!("MissingParameter: missing query parameter `{}`", param.name),
                    0,
                )
                .with_status(crate::status::GqlStatus::InvalidParameter));
            }
        }
    }

    // 8. YIELD bindings must not clash with the scope
    if let Some(ref yield_items) = cc.yield_items {
        match yield_items {
            YieldItems::All => {
                // every output column into scope
                for out in &proc_def.outputs {
                    if scope.vars.contains_key(&out.name) {
                        return Err(CypherError::syntax(
                            format!("VariableAlreadyBound: `{}`", out.name),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::VariableAlreadyBound));
                    }
                    scope.vars.insert(out.name.clone(), VarKind::Scalar);
                }
            }
            YieldItems::Explicit(items) => {
                let mut seen = HashSet::new();
                for item in items {
                    let bound_name = item.alias.as_deref().unwrap_or(&item.name);
                    if !seen.insert(bound_name.to_string()) {
                        return Err(CypherError::syntax(
                            format!("VariableAlreadyBound: `{bound_name}`"),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::VariableAlreadyBound));
                    }
                    if scope.vars.contains_key(bound_name) {
                        return Err(CypherError::syntax(
                            format!("VariableAlreadyBound: `{bound_name}`"),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::VariableAlreadyBound));
                    }
                    scope.vars.insert(bound_name.to_string(), VarKind::Scalar);
                }
            }
        }
    } else if !proc_def.outputs.is_empty() {
        // no YIELD: standalone CALL yields all outputs
        if is_standalone {
            for out in &proc_def.outputs {
                scope.vars.insert(out.name.clone(), VarKind::Scalar);
            }
        }
    }

    Ok(())
}

fn proc_type_name(t: &crate::procedure::ProcType) -> &'static str {
    match t {
        crate::procedure::ProcType::String => "STRING",
        crate::procedure::ProcType::Integer => "INTEGER",
        crate::procedure::ProcType::Float => "FLOAT",
        crate::procedure::ProcType::Number => "NUMBER",
    }
}

// -- SET item validation ----------------------------------------------

fn validate_set_items(items: &[SetItem], scope: &Scope) -> Result<(), CypherError> {
    for item in items {
        match item {
            SetItem::Property(pa, expr) => {
                // property target variable must be defined
                if scope.has_clauses && !scope.vars.contains_key(pa.expr_or_var.as_str()) {
                    return Err(CypherError::syntax(
                        format!("Variable `{}` not defined", pa.expr_or_var),
                        0,
                    )
                    .with_status(crate::status::GqlStatus::VariableNotDefined));
                }
                validate_expr(expr, scope)?;
                // no pattern predicates in SET values
                if pattern_predicate_in_value_position(expr) {
                    return Err(CypherError::syntax(
                        "Pattern expressions are not allowed in a SET clause \
                         (wrap in exists())"
                            .to_string(),
                        0,
                    ));
                }
            }
            SetItem::AllProperties(var, expr) | SetItem::MergeProperties(var, expr) => {
                if scope.has_clauses && !scope.vars.contains_key(var.as_str()) {
                    return Err(
                        CypherError::syntax(format!("Variable `{var}` not defined"), 0)
                            .with_status(crate::status::GqlStatus::VariableNotDefined),
                    );
                }
                validate_expr(expr, scope)?;
                if pattern_predicate_in_value_position(expr) {
                    return Err(CypherError::syntax(
                        "Pattern expressions are not allowed in a SET clause \
                         (wrap in exists())"
                            .to_string(),
                        0,
                    ));
                }
            }
            SetItem::Label(var, _, _) => {
                if scope.has_clauses && !scope.vars.contains_key(var.as_str()) {
                    return Err(
                        CypherError::syntax(format!("Variable `{var}` not defined"), 0)
                            .with_status(crate::status::GqlStatus::VariableNotDefined),
                    );
                }
            }
            SetItem::DynamicProperty {
                variable,
                key,
                value,
            } => {
                if scope.has_clauses && !scope.vars.contains_key(variable.as_str()) {
                    return Err(CypherError::syntax(
                        format!("Variable `{variable}` not defined"),
                        0,
                    )
                    .with_status(crate::status::GqlStatus::VariableNotDefined));
                }
                validate_expr(key, scope)?;
                validate_expr(value, scope)?;
            }
        }
    }
    Ok(())
}

// -- SKIP / LIMIT validation ------------------------------------------

pub(super) fn validate_skip_limit(expr: &Expr, label: &str) -> Result<(), CypherError> {
    // no float literals in SKIP/LIMIT
    if matches!(expr, Expr::Literal(Literal::Float(_))) {
        return Err(
            CypherError::syntax(format!("{label} must be a non-negative integer"), 0)
                .with_status(crate::status::GqlStatus::NegativeValue),
        );
    }
    // no negative integer literals
    if let Expr::UnaryOp(UnaryOp::Neg, inner) = expr
        && matches!(inner.as_ref(), Expr::Literal(Literal::Integer(_)))
    {
        return Err(
            CypherError::syntax(format!("{label} must be a non-negative integer"), 0)
                .with_status(crate::status::GqlStatus::NegativeValue),
        );
    }
    // no variable-dependent expressions
    if expr_references_variable(expr) {
        return Err(CypherError::syntax(
            format!("{label} must be a non-negative integer constant"),
            0,
        )
        .with_status(crate::status::GqlStatus::NegativeValue));
    }
    Ok(())
}

#[recursive::recursive]
fn expr_references_variable(expr: &Expr) -> bool {
    match expr {
        Expr::Variable(_) => true,
        Expr::Literal(_) => false,
        Expr::BinaryOp(l, _, r) => expr_references_variable(l) || expr_references_variable(r),
        Expr::UnaryOp(_, e) => expr_references_variable(e),
        Expr::Property(base, _) => expr_references_variable(base),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_references_variable),
        _ => false,
    }
}
