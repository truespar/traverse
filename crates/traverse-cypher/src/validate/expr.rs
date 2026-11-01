//! Expressions: variable scope and kinds, operand types, known functions and quantifier
//! predicates.

use crate::ast::{BinOp, Expr, Literal, UnaryOp};
use crate::error::CypherError;

use super::projection::{expr_contains_agg, expr_contains_nondeterministic, is_agg_function};
use super::{Scope, VarKind};

// -- Expression validation --------------------------------------------

#[allow(clippy::only_used_in_recursion)]
#[recursive::recursive]
pub(super) fn validate_expr(expr: &Expr, scope: &Scope) -> Result<(), CypherError> {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) => Ok(()),
        Expr::Variable(v) => {
            // variable must be in scope (once clauses started binding)
            if scope.has_clauses && !scope.vars.contains_key(v.as_str()) {
                return Err(
                    CypherError::syntax(format!("Variable `{v}` not defined"), 0)
                        .with_status(crate::status::GqlStatus::VariableNotDefined),
                );
            }
            Ok(())
        }
        Expr::Property(base, _) => {
            validate_expr(base, scope)?;
            // property access on a non-map literal is a type error
            match base.as_ref() {
                Expr::Literal(Literal::Integer(_))
                | Expr::Literal(Literal::Float(_))
                | Expr::Literal(Literal::Bool(_))
                | Expr::Literal(Literal::String(_))
                | Expr::ListLiteral(_) => {
                    return Err(CypherError::syntax(
                        "Type mismatch: property access requires a node, relationship, or map"
                            .to_string(),
                        0,
                    ));
                }
                Expr::Variable(v)
                    if scope.vars.get(v.as_str()) == Some(&VarKind::LiteralScalar) =>
                {
                    return Err(CypherError::syntax(
                        "Type mismatch: property access requires a node, relationship, or map"
                            .to_string(),
                        0,
                    ));
                }
                Expr::Variable(v) if scope.vars.get(v.as_str()) == Some(&VarKind::Path) => {
                    return Err(CypherError::syntax(
                        "Type mismatch: property access on a path is not allowed".to_string(),
                        0,
                    ));
                }
                _ => {}
            }
            Ok(())
        }
        Expr::BinaryOp(left, op, right) => {
            validate_expr(left, scope)?;
            validate_expr(right, scope)?;
            match op {
                BinOp::And | BinOp::Or | BinOp::Xor => {
                    check_boolean_operand(left)?;
                    check_boolean_operand(right)?;
                }
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                    check_arithmetic_operands(left, right, *op)?;
                }
                BinOp::Concat => {
                    // ISO 39075 section 20.23 `||`: operand kinds (strings or lists) are checked at run
                    // time, since expressions may only produce them then
                }
            }
            Ok(())
        }
        Expr::UnaryOp(op, operand) => {
            validate_expr(operand, scope)?;
            if *op == UnaryOp::Not {
                check_boolean_operand(operand)?;
            }
            Ok(())
        }
        Expr::Comparison(base, pairs) => {
            validate_expr(base, scope)?;
            for (_, e) in pairs {
                validate_expr(e, scope)?;
            }
            Ok(())
        }
        Expr::FunctionCall { name, args, .. } => {
            for arg in args {
                validate_expr(arg, scope)?;
            }
            // compile-time type checks for functions with known argument types
            if args.len() == 1
                && let Expr::Variable(v) = &args[0]
                && let Some(&kind) = scope.vars.get(v.as_str())
            {
                match name.as_str() {
                    "type" if kind == VarKind::Node => {
                        return Err(CypherError::syntax(
                            "Type mismatch: expected Relationship but got Node".to_string(),
                            0,
                        ));
                    }
                    "labels" if kind == VarKind::Path => {
                        return Err(CypherError::syntax(
                            "Type mismatch: expected Node but got Path".to_string(),
                            0,
                        ));
                    }
                    "length" if kind == VarKind::Node || kind == VarKind::Relationship => {
                        return Err(CypherError::syntax(
                            format!(
                                "Type mismatch: expected String, Path, or List but got {kind:?}",
                            ),
                            0,
                        ));
                    }
                    "size" if kind == VarKind::Path => {
                        return Err(CypherError::syntax(
                            "Type mismatch: expected String or List but got Path".to_string(),
                            0,
                        ));
                    }
                    _ => {}
                }
            }
            // properties() on a literal non-entity argument
            if name == "properties" && args.len() == 1 {
                let arg_type = match &args[0] {
                    Expr::Literal(Literal::Integer(_)) => Some("INTEGER"),
                    Expr::Literal(Literal::Float(_)) => Some("FLOAT"),
                    Expr::Literal(Literal::String(_)) => Some("STRING"),
                    Expr::Literal(Literal::Bool(_)) => Some("BOOLEAN"),
                    Expr::ListLiteral(_) => Some("LIST"),
                    _ => None,
                };
                if let Some(t) = arg_type {
                    return Err(CypherError::syntax(
                        format!("Type mismatch: expected Node, Relationship, or Map but got {t}"),
                        0,
                    ));
                }
            }
            // size() on a pattern predicate is rejected
            if name == "size" && args.len() == 1 && matches!(&args[0], Expr::PatternPredicate(_)) {
                return Err(CypherError::syntax(
                    "Type mismatch: expected String or List but got pattern predicate".to_string(),
                    0,
                ));
            }
            // nested aggregation: count(count(*))
            if is_agg_function(name) {
                for arg in args {
                    if expr_contains_agg(arg) {
                        return Err(CypherError::syntax(
                            "Can't use aggregate functions inside of aggregate functions."
                                .to_string(),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidAggregation));
                    }
                    // no non-deterministic functions inside aggregations
                    if expr_contains_nondeterministic(arg) {
                        return Err(CypherError::syntax(
                            "Can't use non-constant expression with an aggregation function"
                                .to_string(),
                            0,
                        )
                        .with_status(crate::status::GqlStatus::InvalidAggregation));
                    }
                }
            }
            // unknown functions (neither aggregate nor known scalar); apoc.* resolves at runtime
            // (clear error there outside the compatibility subset)
            if !name.starts_with("apoc.")
                && !name.starts_with("gds.")
                && !is_known_function(name)
                && !is_agg_function(name)
            {
                return Err(CypherError::syntax(format!("Unknown function: {name}"), 0)
                    .with_status(crate::status::GqlStatus::UnknownFunction));
            }
            Ok(())
        }
        Expr::ListLiteral(items) => {
            for item in items {
                validate_expr(item, scope)?;
            }
            Ok(())
        }
        Expr::MapLiteral(pairs) => {
            for (_, e) in pairs {
                validate_expr(e, scope)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(op) = operand {
                validate_expr(op, scope)?;
            }
            for (when, then) in whens {
                validate_expr(when, scope)?;
                validate_expr(then, scope)?;
            }
            if let Some(el) = else_expr {
                validate_expr(el, scope)?;
            }
            Ok(())
        }
        Expr::In(left, right) => {
            validate_expr(left, scope)?;
            validate_expr(right, scope)?;
            // right side must not be a non-list literal
            match right.as_ref() {
                Expr::Literal(Literal::Bool(_))
                | Expr::Literal(Literal::Integer(_))
                | Expr::Literal(Literal::Float(_))
                | Expr::Literal(Literal::String(_))
                | Expr::MapLiteral(_) => {
                    return Err(CypherError::syntax(
                        "IN requires a list on the right side".to_string(),
                        0,
                    ));
                }
                _ => {}
            }
            Ok(())
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) => validate_expr(e, scope),
        Expr::IsTyped { expr, .. } => validate_expr(expr, scope),
        Expr::StringOp(left, _, right) => {
            validate_expr(left, scope)?;
            validate_expr(right, scope)
        }
        Expr::ListIndex(list, idx) => {
            validate_expr(list, scope)?;
            validate_expr(idx, scope)
        }
        Expr::ListSlice(list, from, to) => {
            validate_expr(list, scope)?;
            if let Some(f) = from {
                validate_expr(f, scope)?;
            }
            if let Some(t) = to {
                validate_expr(t, scope)?;
            }
            Ok(())
        }
        Expr::ListComprehension {
            variable,
            list_expr,
            filter,
            map_expr,
        } => {
            validate_expr(list_expr, scope)?;
            // no aggregation in list comprehensions
            if let Some(m) = map_expr
                && expr_contains_agg(m)
            {
                return Err(CypherError::syntax(
                    "Can't use aggregate functions inside of list comprehensions".to_string(),
                    0,
                )
                .with_status(crate::status::GqlStatus::InvalidAggregation));
            }
            // local scope with the comprehension variable
            let mut local = Scope {
                vars: scope.vars.clone(),
                has_clauses: scope.has_clauses,
            };
            local.vars.insert(variable.clone(), VarKind::Scalar);
            if let Some(f) = filter {
                validate_expr(f, &local)?;
            }
            if let Some(m) = map_expr {
                validate_expr(m, &local)?;
            }
            Ok(())
        }
        Expr::ExistentialSubquery(_)
        | Expr::CountSubquery(_)
        | Expr::FullExistentialSubquery(_)
        | Expr::CollectSubquery(_) => Ok(()),
        Expr::PatternPredicate(mc) => {
            // every pattern-predicate variable must already be bound
            for part in &mc.pattern.parts {
                let elem = &part.element;
                if let Some(ref v) = elem.node.variable
                    && !scope.vars.contains_key(v.as_str())
                {
                    return Err(
                        CypherError::syntax(format!("Variable `{v}` not defined"), 0)
                            .with_status(crate::status::GqlStatus::VariableNotDefined),
                    );
                }
                for chain in &elem.chain {
                    if let Some(ref v) = chain.rel.variable
                        && !scope.vars.contains_key(v.as_str())
                    {
                        return Err(
                            CypherError::syntax(format!("Variable `{v}` not defined"), 0)
                                .with_status(crate::status::GqlStatus::VariableNotDefined),
                        );
                    }
                    if let Some(ref v) = chain.node.variable
                        && !scope.vars.contains_key(v.as_str())
                    {
                        return Err(
                            CypherError::syntax(format!("Variable `{v}` not defined"), 0)
                                .with_status(crate::status::GqlStatus::VariableNotDefined),
                        );
                    }
                }
            }
            Ok(())
        }
        Expr::QuantifierExpr {
            list_expr,
            predicate,
            variable,
            ..
        } => {
            validate_expr(list_expr, scope)?;
            // type mismatch: non-numeric literals used in arithmetic (none(x IN ['a'] WHERE x % 2 =
            // 0))
            if let Some(elem_type) = list_literal_element_type(list_expr)
                && predicate_uses_var_in_arithmetic(predicate, variable)
            {
                return Err(CypherError::syntax(
                    format!(
                        "Type mismatch: expected numeric operands but got {elem_type} and INTEGER"
                    ),
                    0,
                ));
            }
            // local scope with the quantifier variable
            let mut local = Scope {
                vars: scope.vars.clone(),
                has_clauses: scope.has_clauses,
            };
            local.vars.insert(variable.clone(), VarKind::Scalar);
            validate_expr(predicate, &local)
        }
        Expr::PatternComprehension {
            variable,
            match_clause,
            projection,
        } => {
            // local scope with the pattern's variables
            let mut local = Scope {
                vars: scope.vars.clone(),
                has_clauses: scope.has_clauses,
            };
            // path variable
            if let Some(pv) = variable {
                local.vars.insert(pv.clone(), VarKind::Path);
            }
            // pattern node and edge variables
            for part in &match_clause.pattern.parts {
                if let Some(ref v) = part.element.node.variable {
                    local.vars.entry(v.clone()).or_insert(VarKind::Node);
                }
                for chain in &part.element.chain {
                    if let Some(ref v) = chain.rel.variable {
                        local.vars.entry(v.clone()).or_insert(VarKind::Relationship);
                    }
                    if let Some(ref v) = chain.node.variable {
                        local.vars.entry(v.clone()).or_insert(VarKind::Node);
                    }
                }
            }
            if let Some(ref w) = match_clause.where_clause {
                validate_expr(w, &local)?;
            }
            validate_expr(projection, &local)
        }
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            list_expr,
            update_expr,
        } => {
            validate_expr(initial, scope)?;
            validate_expr(list_expr, scope)?;
            let mut local = Scope {
                vars: scope.vars.clone(),
                has_clauses: scope.has_clauses,
            };
            local.vars.insert(accumulator.clone(), VarKind::Node);
            local.vars.insert(variable.clone(), VarKind::Node);
            validate_expr(update_expr, &local)
        }
        Expr::MapProjection {
            expr,
            literal_entries,
            ..
        } => {
            validate_expr(expr, scope)?;
            for (_, e) in literal_entries {
                validate_expr(e, scope)?;
            }
            Ok(())
        }
    }
}

// -- Check expression defined in scope (for ORDER BY) -----------------

#[recursive::recursive]
pub(super) fn validate_expr_defined(expr: &Expr, scope: &Scope) -> Result<(), CypherError> {
    match expr {
        Expr::Variable(v) => {
            if !scope.vars.contains_key(v.as_str()) {
                return Err(
                    CypherError::syntax(format!("Variable `{v}` not defined"), 0)
                        .with_status(crate::status::GqlStatus::VariableNotDefined),
                );
            }
            Ok(())
        }
        Expr::Property(base, _) => validate_expr_defined(base, scope),
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                validate_expr_defined(arg, scope)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// -- Pure literal check -----------------------------------------------

/// Literal that can't have properties (not a map literal, not null).
#[recursive::recursive]
pub(super) fn is_non_entity_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(Literal::Null) => false,
        Expr::Literal(_) => true, // int, float, bool, string
        Expr::ListLiteral(items) => items.iter().all(is_non_entity_literal),
        Expr::UnaryOp(UnaryOp::Neg, inner) => is_non_entity_literal(inner),
        _ => false,
    }
}

// -- Boolean operand checking -----------------------------------------

fn check_boolean_operand(expr: &Expr) -> Result<(), CypherError> {
    match expr {
        Expr::Literal(lit) => match lit {
            Literal::Bool(_) | Literal::Null => Ok(()),
            Literal::Integer(_) => Err(CypherError::syntax(
                "Type mismatch: expected Boolean but was Integer".to_string(),
                0,
            )),
            Literal::Float(_) => Err(CypherError::syntax(
                "Type mismatch: expected Boolean but was Float".to_string(),
                0,
            )),
            Literal::String(_) => Err(CypherError::syntax(
                "Type mismatch: expected Boolean but was String".to_string(),
                0,
            )),
        },
        Expr::ListLiteral(_) => Err(CypherError::syntax(
            "Type mismatch: expected Boolean but was List".to_string(),
            0,
        )),
        Expr::MapLiteral(_) => Err(CypherError::syntax(
            "Type mismatch: expected Boolean but was Map".to_string(),
            0,
        )),
        _ => Ok(()),
    }
}

/// Static type name of a non-numeric literal.
fn non_numeric_literal_type(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::Literal(Literal::String(_)) => Some("STRING"),
        Expr::Literal(Literal::Bool(_)) => Some("BOOLEAN"),
        Expr::ListLiteral(_) => Some("LIST"),
        Expr::MapLiteral(_) => Some("MAP"),
        _ => None,
    }
}

/// Static type name of a numeric literal.
fn numeric_literal_type(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::Literal(Literal::Integer(_)) => Some("INTEGER"),
        Expr::Literal(Literal::Float(_)) => Some("FLOAT"),
        _ => None,
    }
}

fn check_arithmetic_operands(left: &Expr, right: &Expr, op: BinOp) -> Result<(), CypherError> {
    // `+`: string + string and list + anything concatenate
    if op == BinOp::Add {
        // `+` with a STRING on either side concatenates; Neo4j coerces numbers/booleans ('epoch:
        // ' + 123 -> 'epoch: 123')
        if matches!(left, Expr::Literal(Literal::String(_)))
            || matches!(right, Expr::Literal(Literal::String(_)))
        {
            return Ok(());
        }
        // list + anything / anything + list concatenates
        if matches!(left, Expr::ListLiteral(_)) || matches!(right, Expr::ListLiteral(_)) {
            return Ok(());
        }
    }
    // a non-numeric literal with any literal: error
    if let Some(ltype) = non_numeric_literal_type(left) {
        if let Some(rtype) = numeric_literal_type(right) {
            return Err(CypherError::syntax(
                format!("Type mismatch: expected numeric operands but got {ltype} and {rtype}"),
                0,
            ));
        }
        if let Some(rtype) = non_numeric_literal_type(right) {
            return Err(CypherError::syntax(
                format!("Type mismatch: expected numeric operands but got {ltype} and {rtype}"),
                0,
            ));
        }
    }
    if let Some(rtype) = non_numeric_literal_type(right)
        && let Some(ltype) = numeric_literal_type(left)
    {
        return Err(CypherError::syntax(
            format!("Type mismatch: expected numeric operands but got {ltype} and {rtype}"),
            0,
        ));
    }
    Ok(())
}

fn is_known_function(name: &str) -> bool {
    matches!(
        name,
        "id" | "elementid"
            | "type"
            | "labels"
            | "keys"
            | "properties"
            | "tostring"
            | "tointeger"
            | "toint"
            | "tofloat"
            | "toboolean"
            | "tobooleanlist"
            | "tofloatlist"
            | "tointegerlist"
            | "tostringlist"
            | "tobooleanornull"
            | "tofloatornull"
            | "tointegerornull"
            | "tostringornull"
            | "toupper"
            | "touppercase"
            | "tolower"
            | "tolowercase"
            | "upper"
            | "lower"
            | "trim"
            | "btrim"
            | "gql_trim"
            | "ltrim"
            | "rtrim"
            | "replace"
            | "substring"
            | "left"
            | "right"
            | "split"
            | "reverse"
            | "path_length"
            | "duration_between"
            | "format"
            | "coll.distinct"
            | "coll.flatten"
            | "coll.indexof"
            | "coll.min"
            | "coll.max"
            | "coll.sort"
            | "coll.insert"
            | "coll.remove"
            | "string.join"
            | "string.indexof"
            | "string.regexreplace"
            | "char_length"
            | "character_length"
            | "normalize"
            | "gql_is_normalized"
            | "abs"
            | "ceil"
            | "ceiling"
            | "floor"
            | "round"
            | "sign"
            | "rand"
            | "randomuuid"
            | "isnan"
            | "isempty"
            | "file"
            | "linenumber"
            | "point"
            | "point.distance"
            | "point.withinbbox"
            | "vector.similarity.cosine"
            | "vector.similarity.euclidean"
            | "vector"
            | "vector_distance"
            | "vector_norm"
            | "vector_dimension_count"
            | "sqrt"
            | "log"
            | "ln"
            | "log10"
            | "exp"
            | "e"
            | "pi"
            | "mod"
            | "power"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "sinh"
            | "cosh"
            | "tanh"
            | "cot"
            | "degrees"
            | "radians"
            | "haversin"
            | "startnode"
            | "endnode"
            | "size"
            | "length"
            | "head"
            | "tail"
            | "last"
            | "range"
            | "coalesce"
            | "nullif"
            | "nodes"
            | "relationships"
            | "rels"
            | "exists"
            | "property_exists"
            | "current_date"
            | "current_time"
            | "current_timestamp"
            | "local_timestamp"
            | "session_user"
            | "typename"
            | "valuetype"
            | "date"
            | "localtime"
            | "time"
            | "localdatetime"
            | "datetime"
            | "duration"
            | "local_time"
            | "local_datetime"
            | "zoned_time"
            | "zoned_datetime"
            | "date.truncate"
            | "localtime.truncate"
            | "time.truncate"
            | "localdatetime.truncate"
            | "datetime.truncate"
            | "duration.between"
            | "duration.inmonths"
            | "duration.indays"
            | "duration.inseconds"
            | "datetime.fromepoch"
            | "datetime.fromepochmillis"
            | "date.transaction"
            | "date.statement"
            | "date.realtime"
            | "localtime.transaction"
            | "localtime.statement"
            | "localtime.realtime"
            | "time.transaction"
            | "time.statement"
            | "time.realtime"
            | "localdatetime.transaction"
            | "localdatetime.statement"
            | "localdatetime.realtime"
            | "datetime.transaction"
            | "datetime.statement"
            | "datetime.realtime"
            | "__has_label"
            | "timestamp"
    )
}

// -- Quantifier type-mismatch helpers ---------------------------------

/// Common non-numeric element type of a list literal ("STRING", "BOOLEAN"), if all elements share
/// one.
fn list_literal_element_type(expr: &Expr) -> Option<&'static str> {
    if let Expr::ListLiteral(items) = expr {
        if items.is_empty() {
            return None;
        }
        let first_type = non_numeric_literal_type(&items[0])?;
        if items
            .iter()
            .all(|i| non_numeric_literal_type(i) == Some(first_type))
        {
            return Some(first_type);
        }
    }
    None
}

/// Does the predicate use `var` as an arithmetic operand (%, /, *, -, ^, +)?
#[recursive::recursive]
fn predicate_uses_var_in_arithmetic(expr: &Expr, var: &str) -> bool {
    match expr {
        Expr::BinaryOp(left, op, right) => {
            let is_arith = matches!(
                op,
                BinOp::Mod | BinOp::Div | BinOp::Mul | BinOp::Sub | BinOp::Pow | BinOp::Add
            );
            if is_arith {
                let left_is_var = matches!(left.as_ref(), Expr::Variable(v) if v == var);
                let right_is_var = matches!(right.as_ref(), Expr::Variable(v) if v == var);
                if left_is_var || right_is_var {
                    return true;
                }
            }
            predicate_uses_var_in_arithmetic(left, var)
                || predicate_uses_var_in_arithmetic(right, var)
        }
        Expr::Comparison(base, pairs) => {
            predicate_uses_var_in_arithmetic(base, var)
                || pairs
                    .iter()
                    .any(|(_, e)| predicate_uses_var_in_arithmetic(e, var))
        }
        Expr::UnaryOp(_, e) => predicate_uses_var_in_arithmetic(e, var),
        _ => false,
    }
}
