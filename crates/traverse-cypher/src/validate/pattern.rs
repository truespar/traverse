//! Patterns in MATCH, CREATE and MERGE.

use std::collections::HashSet;

use crate::ast::{NodePattern, Pattern, PatternElement, RelDirection, RelPattern};
use crate::error::CypherError;

use super::expr::validate_expr;
use super::{Scope, VarKind};

// -- MATCH pattern validation -----------------------------------------

/// Validates a `USING INDEX` / `USING SCAN` hint: the variable must be a node of the pattern
/// carrying the hint's label. Index existence is checked at plan time.
pub(super) fn validate_match_hint(
    hint: &crate::ast::MatchHint,
    pattern: &crate::ast::Pattern,
) -> Result<(), CypherError> {
    let (var, label) = match hint {
        crate::ast::MatchHint::Index {
            variable, label, ..
        }
        | crate::ast::MatchHint::Scan { variable, label } => (variable, label),
    };
    let mut found = false;
    let mut check_node = |np: &crate::ast::NodePattern| -> Result<(), CypherError> {
        if np.variable.as_deref() == Some(var.as_str()) {
            found = true;
            if !np.labels.iter().any(|l| l == label) {
                return Err(CypherError::syntax(
                    format!("USING hint variable `{var}` is not labelled `{label}` in the pattern",),
                    0,
                ));
            }
        }
        Ok(())
    };
    for part in &pattern.parts {
        check_node(&part.element.node)?;
        for chain in &part.element.chain {
            check_node(&chain.node)?;
        }
    }
    if !found {
        return Err(CypherError::syntax(
            format!("USING hint refers to unbound variable `{var}`"),
            0,
        ));
    }
    Ok(())
}

pub(super) fn validate_match_pattern(
    pattern: &Pattern,
    scope: &mut Scope,
) -> Result<(), CypherError> {
    let mut rel_vars = HashSet::new();
    for part in &pattern.parts {
        if let Some(ref var) = part.variable {
            scope.bind(var, VarKind::Path)?;
        }
        // wShortest cost variable into scope
        if let Some(ref cv) = part.total_cost_var {
            scope.bind(cv, VarKind::Scalar)?;
        }
        validate_pattern_element(&part.element, scope, &mut rel_vars)?;
        // ISO section 16.11 multi-step group variables, bound as node/edge LISTS
        for (v, _) in &part.qpp_list_groups {
            scope.bind(v, VarKind::ListScalar)?;
        }
    }
    // pattern-internal WHERE validates against the FULLY bound pattern (it may use variables bound
    // later in the same pattern)
    for part in &pattern.parts {
        let elem = &part.element;
        if let Some(ref p) = elem.node.where_pred {
            validate_expr(p, scope)?;
        }
        for chain in &elem.chain {
            if let Some(ref p) = chain.rel.where_pred {
                validate_expr(p, scope)?;
            }
            if let Some(ref qpp) = chain.rel.qpp_group {
                for np in [&qpp.start, &qpp.end] {
                    if let Some(ref p) = np.where_pred {
                        validate_expr(p, scope)?;
                    }
                }
            }
            if let Some(ref p) = chain.node.where_pred {
                validate_expr(p, scope)?;
            }
        }
    }
    Ok(())
}

fn validate_pattern_element(
    elem: &PatternElement,
    scope: &mut Scope,
    rel_vars: &mut HashSet<String>,
) -> Result<(), CypherError> {
    validate_node_pattern(&elem.node, scope)?;
    for chain in &elem.chain {
        if let Some(ref var) = chain.rel.variable
            && !rel_vars.insert(var.clone())
        {
            return Err(CypherError::syntax(
                format!(
                    "Cannot use the same relationship variable `{var}` for multiple relationships"
                ),
                0,
            ));
        }
        validate_rel_pattern(&chain.rel, scope)?;
        // QPP group variables `((x)-[r]->(y)){m,n}`, bound as node LISTS
        if let Some(ref qpp) = chain.rel.qpp_group {
            for np in [&qpp.start, &qpp.end] {
                if let Some(ref var) = np.variable {
                    scope.bind(var, VarKind::ListScalar)?;
                }
                if let Some(ref props) = np.properties {
                    validate_expr(props, scope)?;
                }
            }
        }
        validate_node_pattern(&chain.node, scope)?;
    }
    Ok(())
}

fn validate_node_pattern(np: &NodePattern, scope: &mut Scope) -> Result<(), CypherError> {
    if let Some(ref var) = np.variable {
        scope.bind(var, VarKind::Node)?;
    }
    if let Some(ref props) = np.properties {
        validate_expr(props, scope)?;
    }
    Ok(())
}

fn validate_rel_pattern(rp: &RelPattern, scope: &mut Scope) -> Result<(), CypherError> {
    if let Some(ref var) = rp.variable {
        scope.bind(var, VarKind::Relationship)?;
    }
    if let Some(ref props) = rp.properties {
        validate_expr(props, scope)?;
    }
    Ok(())
}

// -- CREATE pattern validation ----------------------------------------

pub(super) fn validate_create_pattern(
    pattern: &Pattern,
    scope: &mut Scope,
) -> Result<(), CypherError> {
    for part in &pattern.parts {
        validate_create_element(&part.element, scope)?;
    }
    Ok(())
}

fn validate_create_element(elem: &PatternElement, scope: &mut Scope) -> Result<(), CypherError> {
    // pattern-internal WHERE belongs to read patterns; Neo4j rejects it in CREATE / MERGE
    reject_pattern_where(elem, "CREATE")?;
    let is_standalone = elem.chain.is_empty();
    validate_create_node(&elem.node, scope, is_standalone)?;
    for chain in &elem.chain {
        validate_create_rel(&chain.rel, scope)?;
        validate_create_node(&chain.node, scope, false)?;
    }
    Ok(())
}

/// Rejects `(n WHERE ...)` / `[r WHERE ...]` in write patterns.
fn reject_pattern_where(elem: &PatternElement, clause: &str) -> Result<(), CypherError> {
    let has = elem.node.where_pred.is_some()
        || elem
            .chain
            .iter()
            .any(|c| c.rel.where_pred.is_some() || c.node.where_pred.is_some());
    if has {
        // complex label expressions (`|`, `!`, `%`) lower to pattern predicates at parse time and
        // land here too: name both
        return Err(CypherError::syntax(
            format!(
                "WHERE and label expressions with |, !, % are not allowed inside {clause} patterns"
            ),
            0,
        ));
    }
    Ok(())
}

fn validate_create_node(
    np: &NodePattern,
    scope: &mut Scope,
    is_standalone: bool,
) -> Result<(), CypherError> {
    if let Some(ref var) = np.variable {
        if let Some(&kind) = scope.vars.get(var.as_str()) {
            // LiteralScalar/MapScalar can't be a CREATE node
            if matches!(kind, VarKind::LiteralScalar | VarKind::MapScalar) {
                return Err(CypherError::syntax(
                    format!(
                        "Variable `{var}` already declared as Scalar, cannot use as Node in CREATE"
                    ),
                    0,
                )
                .with_status(crate::status::GqlStatus::VariableAlreadyDeclared));
            }
            // relationship/path can't be a node
            if kind == VarKind::Relationship || kind == VarKind::Path {
                return Err(CypherError::syntax(
                    format!(
                        "Variable `{var}` already declared as {kind:?}, cannot use as Node in CREATE"
                    ),
                    0,
                ).with_status(crate::status::GqlStatus::VariableAlreadyDeclared));
            }
            // standalone node already bound: can't create an existing entity
            if is_standalone && kind == VarKind::Node {
                return Err(CypherError::syntax(
                    format!(
                        "Variable `{var}` already bound, cannot create a standalone node in CREATE"
                    ),
                    0,
                )
                .with_status(crate::status::GqlStatus::VariableAlreadyBound));
            }
            // already bound as Node or Scalar: no new labels/props
            if !np.labels.is_empty() || np.properties.is_some() {
                return Err(CypherError::syntax(
                    format!(
                        "Variable `{var}` already bound, cannot rebind with new labels or properties in CREATE"
                    ),
                    0,
                ).with_status(crate::status::GqlStatus::VariableAlreadyBound));
            }
        } else {
            scope.vars.insert(var.clone(), VarKind::Node);
        }
    }
    if let Some(ref props) = np.properties {
        validate_expr(props, scope)?;
    }
    Ok(())
}

fn validate_create_rel(rp: &RelPattern, scope: &mut Scope) -> Result<(), CypherError> {
    // CREATE needs exactly one type, static or a single dynamic expression; more is a syntax error
    // (as in Neo4j 5)
    let total = rp.types.len() + rp.dynamic_types.len();
    if total == 0 {
        return Err(CypherError::syntax(
            "Relationships must have exactly one type in CREATE".to_string(),
            0,
        ));
    }
    if total > 1 {
        return Err(CypherError::syntax(
            "Relationships must have exactly one type in CREATE, got multiple".to_string(),
            0,
        ));
    }
    if rp.direction == RelDirection::Both {
        // name the offending type: the AST has no spans, so it's the only way to locate the clause
        // in a big multi-clause statement (else a 4 KB CREATE script fails at "position 0")
        let ty = rp.types.first().map(String::as_str).unwrap_or("REL");
        return Err(CypherError::syntax(
            format!(
                "Only directed relationships are supported in CREATE - write -[:{ty}]-> or <-[:{ty}]-, not the undirected -[:{ty}]-"
            ),
            0,
        ));
    }
    if rp.range.is_some() {
        return Err(CypherError::syntax(
            "Variable length relationships cannot be used in CREATE".to_string(),
            0,
        ));
    }
    if let Some(ref var) = rp.variable {
        if scope.vars.contains_key(var.as_str()) {
            return Err(CypherError::syntax(
                format!("Variable `{var}` already bound, cannot rebind in CREATE"),
                0,
            )
            .with_status(crate::status::GqlStatus::VariableAlreadyBound));
        }
        scope.vars.insert(var.clone(), VarKind::Relationship);
    }
    if let Some(ref props) = rp.properties {
        validate_expr(props, scope)?;
    }
    Ok(())
}

// -- MERGE pattern validation -----------------------------------------

pub(super) fn validate_merge_pattern(
    pattern: &Pattern,
    scope: &mut Scope,
) -> Result<(), CypherError> {
    for part in &pattern.parts {
        if let Some(ref var) = part.variable {
            scope.bind(var, VarKind::Path)?;
        }
        validate_merge_element(&part.element, scope)?;
    }
    Ok(())
}

fn validate_merge_element(elem: &PatternElement, scope: &mut Scope) -> Result<(), CypherError> {
    reject_pattern_where(elem, "MERGE")?;
    // standalone node already bound?
    let is_standalone = elem.chain.is_empty();
    if is_standalone
        && let Some(ref var) = elem.node.variable
        && let Some(&kind) = scope.vars.get(var.as_str())
        && kind == VarKind::Node
    {
        return Err(CypherError::syntax(
            format!("Variable `{var}` already bound, cannot use standalone in MERGE"),
            0,
        )
        .with_status(crate::status::GqlStatus::VariableAlreadyBound));
    }
    validate_merge_node(&elem.node, scope)?;
    for chain in &elem.chain {
        validate_merge_rel(&chain.rel, scope)?;
        validate_merge_node(&chain.node, scope)?;
    }
    Ok(())
}

fn validate_merge_node(np: &NodePattern, scope: &mut Scope) -> Result<(), CypherError> {
    if let Some(ref var) = np.variable
        && let Some(&kind) = scope.vars.get(var.as_str())
    {
        // bound Node with new MERGE labels: reject
        if kind == VarKind::Node && !np.labels.is_empty() {
            return Err(CypherError::syntax(
                format!("Variable `{var}` already bound; cannot add labels in MERGE"),
                0,
            )
            .with_status(crate::status::GqlStatus::VariableAlreadyBound));
        }
    }
    validate_node_pattern(np, scope)?;
    Ok(())
}

fn validate_merge_rel(rp: &RelPattern, scope: &mut Scope) -> Result<(), CypherError> {
    if rp.types.is_empty() {
        return Err(CypherError::syntax(
            "Relationships must have exactly one type in MERGE".to_string(),
            0,
        ));
    }
    if rp.types.len() > 1 {
        return Err(CypherError::syntax(
            "Relationships must have exactly one type in MERGE, got multiple".to_string(),
            0,
        ));
    }
    if rp.range.is_some() {
        return Err(CypherError::syntax(
            "Variable length relationships cannot be used in MERGE".to_string(),
            0,
        ));
    }
    if let Some(ref var) = rp.variable {
        if scope.vars.contains_key(var.as_str()) {
            return Err(CypherError::syntax(
                format!("Variable `{var}` already bound, cannot rebind in MERGE"),
                0,
            )
            .with_status(crate::status::GqlStatus::VariableAlreadyBound));
        }
        scope.vars.insert(var.clone(), VarKind::Relationship);
    }
    if let Some(ref props) = rp.properties {
        validate_expr(props, scope)?;
    }
    Ok(())
}
