//! Semantic validation of the Cypher AST between parsing and planning: errors the parser can't see
//! (variable kind conflicts, bad boolean operands, illegal CREATE patterns, ...).

mod clause;
mod expr;
mod pattern;
mod projection;

use std::collections::HashMap;

use crate::ast::{Clause, Expr, ProjectionItems, Query, SingleQuery, Statement};
use crate::error::CypherError;

use clause::validate_clause;
use projection::{validate_return, validate_with};

// -- Variable kinds ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Relationship,
    Path,
    /// From a computed expression; any type at runtime.
    Scalar,
    /// From a pure literal; never a graph entity.
    LiteralScalar,
    /// From a list literal; not usable as a node.
    ListScalar,
    /// From a map literal; property access yes, graph entity positions no.
    MapScalar,
}

// -- Scope ------------------------------------------------------------

#[derive(Debug, Default)]
struct Scope {
    vars: HashMap<String, VarKind>,
    /// Set once any clause ran; enables undefined-variable checks.
    has_clauses: bool,
}

impl Scope {
    fn bind(&mut self, name: &str, kind: VarKind) -> Result<(), CypherError> {
        if let Some(&existing) = self.vars.get(name) {
            if existing == kind {
                return Ok(());
            }
            // LiteralScalar / MapScalar can't be graph entities
            if matches!(existing, VarKind::LiteralScalar | VarKind::MapScalar)
                && matches!(kind, VarKind::Node | VarKind::Relationship | VarKind::Path)
            {
                return Err(CypherError::syntax(
                    format!(
                        "Variable `{name}` already declared as {existing:?}, cannot use as {kind:?}"
                    ),
                    0,
                )
                .with_status(crate::status::GqlStatus::VariableAlreadyDeclared));
            }
            // ListScalar can't be a node (var-length rel patterns are fine)
            if existing == VarKind::ListScalar && kind == VarKind::Node {
                return Err(CypherError::syntax(
                    format!("Variable `{name}` already declared as List, cannot use as {kind:?}"),
                    0,
                )
                .with_status(crate::status::GqlStatus::VariableAlreadyDeclared));
            }
            // Scalar (from expressions) can rebind to anything
            if existing == VarKind::Scalar
                || kind == VarKind::Scalar
                || existing == VarKind::LiteralScalar
                || kind == VarKind::LiteralScalar
                || existing == VarKind::ListScalar
                || kind == VarKind::ListScalar
                || existing == VarKind::MapScalar
                || kind == VarKind::MapScalar
            {
                return Ok(());
            }
            // node vs relationship vs path conflicts always error
            return Err(CypherError::syntax(
                format!(
                    "Variable `{name}` already declared as {existing:?}, cannot use as {kind:?}"
                ),
                0,
            )
            .with_status(crate::status::GqlStatus::VariableAlreadyDeclared));
        }
        self.vars.insert(name.to_string(), kind);
        Ok(())
    }
}

// -- Entry point ------------------------------------------------------

/// Validates a parsed statement: variable kind conflicts, bad boolean operands, illegal CREATE
/// patterns, ....
pub fn validate(stmt: &Statement) -> Result<(), CypherError> {
    match &stmt.query {
        Query::CreateIndex { .. } | Query::CreateVectorIndex { .. } | Query::CreateFulltextIndex { .. } | Query::CreatePointIndex { .. } | Query::CreateTextIndex { .. } | Query::DropIndex { .. } | Query::AnalyzeGraph { .. }
        | Query::CreateConstraint { .. } | Query::DropConstraint { .. }
        | Query::CreateEdgeIndex { .. } | Query::DropEdgeIndex { .. }
        | Query::ShowIndexes
        | Query::ShowConstraints
        | Query::ShowFunctions
        | Query::ShowProcedures
        | Query::TransactionCommands { .. }
        | Query::ShowSettings
        // composable SHOW references the leaf's catalog columns (name, type, ...), not MATCH
        // variables, so it's validated at execution
        | Query::ShowComposable { .. } => return Ok(()),
        Query::Regular(_) => {}
    }
    let Query::Regular(ref rq) = stmt.query else {
        unreachable!()
    };
    validate_single_query(&rq.single)?;
    for u in &rq.unions {
        validate_single_query(&u.query)?;
    }
    // UNION column consistency and mixing
    if !rq.unions.is_empty() {
        let base_cols = projection_column_names(&rq.single);
        let first_all = rq.unions[0].all;
        for u in &rq.unions {
            let cols = projection_column_names(&u.query);
            if let (Some(base), Some(other)) = (&base_cols, &cols)
                && base != other
            {
                return Err(CypherError::syntax(
                    "All sub queries in an UNION must have the same return column names"
                        .to_string(),
                    0,
                ));
            }
            if u.all != first_all {
                return Err(CypherError::syntax(
                    "Invalid combination of UNION and UNION ALL".to_string(),
                    0,
                ));
            }
        }
    }
    Ok(())
}

fn projection_column_names(sq: &SingleQuery) -> Option<Vec<String>> {
    if let Some(ref rc) = sq.final_part.return_clause {
        match &rc.items {
            ProjectionItems::All => None,
            ProjectionItems::Explicit(items) => Some(
                items
                    .iter()
                    .map(|pi| pi.alias.clone().unwrap_or_else(|| expr_to_name(&pi.expr)))
                    .collect(),
            ),
        }
    } else {
        None
    }
}

#[recursive::recursive]
fn expr_to_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(v) => v.clone(),
        Expr::Property(base, key) => format!("{}.{key}", expr_to_name(base)),
        Expr::FunctionCall { name, args, .. } => {
            if args.is_empty() {
                format!("{name}()")
            } else {
                format!("{name}(...)")
            }
        }
        _ => "expr".to_string(),
    }
}

// -- Single query validation ------------------------------------------

fn validate_single_query(sq: &SingleQuery) -> Result<(), CypherError> {
    let mut scope = Scope::default();

    // standalone CALL: one clause in final_part, no RETURN, no prior parts
    let is_standalone_call = sq.parts.is_empty()
        && sq.final_part.return_clause.is_none()
        && sq.final_part.clauses.len() == 1
        && matches!(sq.final_part.clauses.first(), Some(Clause::Call(_)));

    for part in &sq.parts {
        for clause in &part.clauses {
            validate_clause(clause, &mut scope, false)?;
        }
        let new_scope = validate_with(&part.with, &scope)?;
        scope = new_scope;
    }

    for clause in &sq.final_part.clauses {
        let standalone = is_standalone_call && matches!(clause, Clause::Call(_));
        validate_clause(clause, &mut scope, standalone)?;
    }
    if let Some(ref rc) = sq.final_part.return_clause {
        // RETURN may only use variables defined by earlier clauses
        scope.has_clauses = true;
        validate_return(rc, &scope)?;
    }

    Ok(())
}
