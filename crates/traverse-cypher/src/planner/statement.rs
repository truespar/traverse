//! Statements and queries: UNION, SHOW, and the clause-by-clause plan of a single query.

use crate::ast::{
    Clause, Expr, NodePattern, Pattern, ProjectionItem, ProjectionItems, Query, RegularQuery,
    ReturnClause, SingleQuery, Statement, WithClause, YieldItems,
};
use crate::error::CypherError;

use super::IndexHints;
use super::clauses::{plan_create, plan_return, plan_with};
use super::part::rand_id;
use super::pattern::{
    plan_match_pattern, plan_match_pattern_with_hints, plan_match_search,
    rewrite_pattern_for_seed_binding,
};
use super::plan::LogicalPlan;
use super::vars::{collect_expr_variables, collect_pattern_variables};

/// Plans a statement with index hints for IndexScan.
pub fn plan_statement_with_hints(
    stmt: &Statement,
    hints: &IndexHints,
) -> Result<LogicalPlan, CypherError> {
    match &stmt.query {
        Query::Regular(rq) => plan_regular_query_with_hints(rq, hints),
        Query::CreateIndex {
            name,
            label,
            property,
            extra_properties,
        } => Ok(LogicalPlan::CreateIndex {
            name: name.clone(),
            extra_properties: extra_properties.clone(),
            label: label.clone(),
            property: property.clone(),
        }),
        Query::CreateVectorIndex {
            name,
            label,
            property,
            dimensions,
            similarity,
            quantization,
        } => Ok(LogicalPlan::CreateVectorIndex {
            name: name.clone(),
            label: label.clone(),
            property: property.clone(),
            dimensions: *dimensions,
            similarity: similarity.clone(),
            quantization: *quantization,
        }),
        Query::CreateFulltextIndex {
            name,
            labels,
            properties,
            analyzer,
        } => Ok(LogicalPlan::CreateFulltextIndex {
            name: name.clone(),
            labels: labels.clone(),
            properties: properties.clone(),
            analyzer: analyzer.clone(),
        }),
        Query::CreatePointIndex {
            name,
            label,
            property,
        } => Ok(LogicalPlan::CreatePointIndex {
            name: name.clone(),
            label: label.clone(),
            property: property.clone(),
        }),
        Query::CreateTextIndex {
            name,
            label,
            property,
        } => Ok(LogicalPlan::CreateTextIndex {
            name: name.clone(),
            label: label.clone(),
            property: property.clone(),
        }),
        Query::DropIndex { name } => Ok(LogicalPlan::DropIndex { name: name.clone() }),
        Query::AnalyzeGraph { labels } => Ok(LogicalPlan::AnalyzeGraph {
            labels: labels.clone(),
        }),
        Query::CreateConstraint {
            name,
            label,
            property,
            extra_properties,
            constraint_type,
            is_relationship,
            if_not_exists,
        } => Ok(LogicalPlan::CreateConstraint {
            name: name.clone(),
            label: label.clone(),
            property: property.clone(),
            extra_properties: extra_properties.clone(),
            constraint_type: constraint_type.clone(),
            is_relationship: *is_relationship,
            if_not_exists: *if_not_exists,
        }),
        Query::DropConstraint { name } => Ok(LogicalPlan::DropConstraint { name: name.clone() }),
        Query::CreateEdgeIndex {
            name,
            edge_type,
            property,
            extra_properties,
        } => Ok(LogicalPlan::CreateEdgeIndex {
            name: name.clone(),
            edge_type: edge_type.clone(),
            property: property.clone(),
            extra_properties: extra_properties.clone(),
        }),
        Query::DropEdgeIndex { name } => Ok(LogicalPlan::DropEdgeIndex { name: name.clone() }),
        Query::ShowIndexes => Ok(LogicalPlan::ShowIndexes),
        Query::ShowConstraints => Ok(LogicalPlan::ShowConstraints),
        Query::ShowFunctions => Ok(LogicalPlan::ShowFunctions),
        Query::ShowProcedures => Ok(LogicalPlan::ShowProcedures),
        Query::TransactionCommands {
            commands,
            return_clause,
        } => plan_transaction_commands(commands, return_clause.as_ref()),
        Query::ShowSettings => Ok(LogicalPlan::ShowSettings),
        Query::ShowComposable {
            kind,
            yield_clause,
            where_only,
            return_clause,
            pre_filter,
        } => {
            // the SHOW leaf feeds the usual projection/filter/sort/page pipeline (WITH and RETURN
            // lowering reused)
            let mut plan = show_kind_leaf(*kind);
            // typed-SHOW filter (e.g. type = 'FULLTEXT') applies to the leaf columns BEFORE user
            // YIELD/WHERE
            if let Some(pf) = pre_filter {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: pf.clone(),
                };
            }
            if let Some(w) = yield_clause {
                plan = plan_with(w, plan)?;
            } else if let Some(pred) = where_only {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: pred.clone(),
                };
            }
            if let Some(ret) = return_clause {
                plan = plan_return(ret, plan)?;
            }
            Ok(plan)
        }
    }
}

/// Chained `SHOW` / `TERMINATE TRANSACTIONS`. A command runs once per row of the one before it;
/// its `YIELD` keeps the earlier commands' variables, like `CALL ... YIELD`. Without `YIELD` or
/// `RETURN` the last command returns its default columns.
fn plan_transaction_commands(
    commands: &[crate::ast::TransactionCommand],
    return_clause: Option<&ReturnClause>,
) -> Result<LogicalPlan, CypherError> {
    let mut plan = LogicalPlan::EmptyRow;
    // variables in scope after each command's YIELD
    let mut scope: Vec<String> = Vec::new();
    for command in commands {
        let columns = command.kind.columns();
        if let Some(clash) = columns.iter().find(|c| scope.iter().any(|s| s == *c)) {
            return Err(CypherError::semantic(format!(
                "Variable `{clash}` already declared: rename the earlier one with YIELD {clash} AS ..."
            )).with_status(crate::status::GqlStatus::VariableAlreadyDeclared));
        }
        plan = LogicalPlan::TransactionCommand {
            input: Box::new(plan),
            command: command.kind,
            ids: command.ids.clone(),
        };
        if let Some(yield_clause) = &command.yield_clause {
            let carried = scope.iter().map(|name| variable_item(name));
            let (items, names): (Vec<ProjectionItem>, Vec<String>) = match &yield_clause.items {
                ProjectionItems::All => {
                    let items: Vec<ProjectionItem> = carried
                        .chain(columns.iter().map(|c| variable_item(c)))
                        .collect();
                    let names = items.iter().map(crate::executor::projection_name).collect();
                    (items, names)
                }
                ProjectionItems::Explicit(own) => {
                    let items: Vec<ProjectionItem> = carried.chain(own.iter().cloned()).collect();
                    let names = items.iter().map(crate::executor::projection_name).collect();
                    (items, names)
                }
            };
            let with = WithClause {
                items: ProjectionItems::Explicit(items),
                ..yield_clause.clone()
            };
            plan = plan_with(&with, plan)?;
            scope = names;
        } else if let Some(predicate) = &command.where_only {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: predicate.clone(),
            };
        }
    }
    if let Some(ret) = return_clause {
        return plan_return(ret, plan);
    }
    match commands.last() {
        Some(last) if last.yield_clause.is_none() => {
            let ret = ReturnClause {
                distinct: false,
                items: ProjectionItems::Explicit(
                    last.kind
                        .default_columns()
                        .iter()
                        .map(|c| variable_item(c))
                        .collect(),
                ),
                order_by: None,
                skip: None,
                limit: None,
            };
            plan_return(&ret, plan)
        }
        _ => Ok(plan),
    }
}

/// `name` as a projection item returning the variable under its own name.
fn variable_item(name: &str) -> ProjectionItem {
    ProjectionItem {
        expr: Expr::Variable(name.to_string()),
        alias: None,
        source_text: Some(name.to_string()),
    }
}

/// SHOW kind -> leaf plan.
fn show_kind_leaf(kind: crate::ast::ShowKind) -> LogicalPlan {
    use crate::ast::ShowKind;
    match kind {
        ShowKind::Indexes => LogicalPlan::ShowIndexes,
        ShowKind::Constraints => LogicalPlan::ShowConstraints,
        ShowKind::Functions => LogicalPlan::ShowFunctions,
        ShowKind::Procedures => LogicalPlan::ShowProcedures,
        ShowKind::Settings => LogicalPlan::ShowSettings,
    }
}

fn plan_regular_query_with_hints(
    rq: &RegularQuery,
    hints: &IndexHints,
) -> Result<LogicalPlan, CypherError> {
    let mut plan = plan_single_query_with_hints(&rq.single, hints)?;
    for union in &rq.unions {
        let right = plan_single_query_with_hints(&union.query, hints)?;
        plan = LogicalPlan::Union {
            left: Box::new(plan),
            right: Box::new(right),
            all: union.all,
        };
    }
    Ok(plan)
}

/// Plans a single query (full existential subqueries).
pub fn plan_single_query(sq: &SingleQuery) -> Result<LogicalPlan, CypherError> {
    plan_single_query_with_hints(sq, &IndexHints::new())
}

#[recursive::recursive]
fn plan_single_query_with_hints(
    sq: &SingleQuery,
    hints: &IndexHints,
) -> Result<LogicalPlan, CypherError> {
    // start with EmptyRow or the first clause
    let mut plan = LogicalPlan::EmptyRow;
    let mut has_match = false;
    let mut bound_vars = std::collections::HashSet::new();

    // intermediate parts (WITH)
    for part in &sq.parts {
        plan = plan_clauses_with_hints(
            &part.clauses,
            plan,
            &mut has_match,
            &mut bound_vars,
            hints,
            &TailScope::With(&part.with),
        )?;
        plan = plan_with(&part.with, plan)?;
        // after WITH only projected variables are in scope
        match &part.with.items {
            ProjectionItems::All => {
                // WITH * keeps everything bound
            }
            ProjectionItems::Explicit(items) => {
                bound_vars.clear();
                for item in items {
                    if let Some(alias) = &item.alias {
                        bound_vars.insert(alias.clone());
                    } else if let Expr::Variable(v) = &item.expr {
                        bound_vars.insert(v.clone());
                    }
                }
            }
        }
    }

    // final part
    plan = plan_clauses_with_hints(
        &sq.final_part.clauses,
        plan,
        &mut has_match,
        &mut bound_vars,
        hints,
        &TailScope::Return(sq.final_part.return_clause.as_ref()),
    )?;

    if let Some(ret) = &sq.final_part.return_clause {
        plan = plan_return(ret, plan)?;
    }

    Ok(plan)
}

/// What follows a clause slice, for liveness lookahead. `Unknown` is conservative: every variable
/// counts as referenced.
enum TailScope<'a> {
    /// Closing WITH of the part: a scope barrier.
    With(&'a WithClause),
    /// Final part's RETURN, if any.
    Return(Option<&'a ReturnClause>),
    /// Unknown continuation (subquery bodies, ...).
    Unknown,
}

impl TailScope<'_> {
    fn may_reference_var(&self, v: &str) -> bool {
        #[recursive::recursive]
        fn expr_refs(e: &Expr, v: &str) -> bool {
            let mut vars = std::collections::HashSet::new();
            collect_expr_variables(e, &mut vars);
            vars.contains(v)
        }
        fn items_ref(items: &ProjectionItems, v: &str) -> bool {
            match items {
                ProjectionItems::All => true,
                ProjectionItems::Explicit(items) => items.iter().any(|i| expr_refs(&i.expr, v)),
            }
        }
        match self {
            TailScope::With(wc) => {
                items_ref(&wc.items, v)
                    || wc.where_clause.as_ref().is_some_and(|w| expr_refs(w, v))
                    || wc
                        .order_by
                        .as_ref()
                        .is_some_and(|obs| obs.iter().any(|o| expr_refs(&o.expr, v)))
                    || wc.skip.as_ref().is_some_and(|e| expr_refs(e, v))
                    || wc.limit.as_ref().is_some_and(|e| expr_refs(e, v))
            }
            TailScope::Return(Some(rc)) => {
                items_ref(&rc.items, v)
                    || rc
                        .order_by
                        .as_ref()
                        .is_some_and(|obs| obs.iter().any(|o| expr_refs(&o.expr, v)))
                    || rc.skip.as_ref().is_some_and(|e| expr_refs(e, v))
                    || rc.limit.as_ref().is_some_and(|e| expr_refs(e, v))
            }
            TailScope::Return(None) => false,
            TailScope::Unknown => true,
        }
    }
}

fn plan_clauses_with_hints(
    clauses: &[Clause],
    mut plan: LogicalPlan,
    has_match: &mut bool,
    bound_vars: &mut std::collections::HashSet<String>,
    hints: &IndexHints,
    tail: &TailScope<'_>,
) -> Result<LogicalPlan, CypherError> {
    for (clause_idx, clause) in clauses.iter().enumerate() {
        match clause {
            Clause::Match(mc) => {
                // `USING INDEX var:Label(prop)`: check the index exists on the live graph. `USING
                // SCAN` is accepted silently (a label scan always exists). Otherwise advisory:
                // indexed lookups are preferred anyway when a matching filter exists
                for h in &mc.hints {
                    if let crate::ast::MatchHint::Index {
                        label, property, ..
                    } = h
                        && !hints.contains(&(label.clone(), property.clone()))
                    {
                        return Err(CypherError::syntax(
                            format!(
                                "USING INDEX hint refers to a non-existent index `{label}({property})`",
                            ),
                            0,
                        ));
                    }
                }
                if let Some(ref search) = mc.search {
                    // SEARCH: the pattern variable comes from the vector index, not a scan
                    plan = plan_match_search(mc, search, plan.clone())?;
                    if let Some(ref where_expr) = mc.where_clause {
                        plan = LogicalPlan::Filter {
                            input: Box::new(plan),
                            predicate: where_expr.clone(),
                        };
                    }
                    for v in collect_pattern_variables(&mc.pattern) {
                        bound_vars.insert(v);
                    }
                    if let Some(ref alias) = search.score_alias {
                        bound_vars.insert(alias.clone());
                    }
                    *has_match = true;
                    continue;
                }
                // re-root parts at a bound endpoint (from prior clauses or earlier parts), so an
                // unbound first node isn't scanned when a later one is bound
                let rewritten = rewrite_pattern_for_seed_binding(&mc.pattern, bound_vars);
                plan = plan_match_pattern_with_hints(&rewritten, plan.clone(), bound_vars, hints)?;
                if let Some(ref where_expr) = mc.where_clause {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: where_expr.clone(),
                    };
                }
                for v in collect_pattern_variables(&mc.pattern) {
                    bound_vars.insert(v);
                }
                *has_match = true;
            }
            Clause::OptionalMatch(mc) => {
                // EmptyRow, OptionalMatch joins with the input itself. Parts are re-rooted at bound
                // endpoints, else `(prev)-[r]->(seller)` with `seller` bound would scan all nodes
                // for `prev` per outer row
                let mut optional = if let Some(ref search) = mc.search {
                    // OPTIONAL MATCH ... SEARCH: match_plan is the vector search per outer row; a
                    // null query vector gives no rows, so OptionalMatch emits a null row
                    plan_match_search(mc, search, LogicalPlan::EmptyRow)?
                } else {
                    let rewritten = rewrite_pattern_for_seed_binding(&mc.pattern, bound_vars);
                    // pass `bound_vars` (not empty) so plan_single_pattern_part sees bound nodes
                    // (skips scans via first_node_bound) and bound edges (BindEdgeEndpoints);
                    // re-rooting already put bound nodes at position 0, so renames don't fire on
                    // them
                    plan_match_pattern(&rewritten, LogicalPlan::EmptyRow, bound_vars)?
                };
                if let Some(ref where_expr) = mc.where_clause {
                    optional = LogicalPlan::Filter {
                        input: Box::new(optional),
                        predicate: where_expr.clone(),
                    };
                }
                let mut new_vars = collect_pattern_variables(&mc.pattern);
                if let Some(ref search) = mc.search
                    && let Some(ref alias) = search.score_alias
                {
                    new_vars.push(alias.clone());
                }
                plan = LogicalPlan::OptionalMatch {
                    input: Box::new(plan),
                    match_plan: Box::new(optional),
                    new_variables: new_vars.clone(),
                };
                for v in new_vars {
                    bound_vars.insert(v);
                }
                *has_match = true;
            }
            Clause::Create(cc) => {
                plan = plan_create(&cc.pattern, plan, bound_vars)?;
            }
            Clause::Merge(mc) => {
                // clone the pattern, naming anonymous elements for path binding
                let mut pattern = mc.pattern.clone();
                let has_path_var = pattern.parts.iter().any(|p| p.variable.is_some());
                if has_path_var {
                    for part in &mut pattern.parts {
                        if part.element.node.variable.is_none() {
                            part.element.node.variable = Some(format!("__merge_{}", rand_id()));
                        }
                        for chain in &mut part.element.chain {
                            if chain.rel.variable.is_none() {
                                chain.rel.variable = Some(format!("__merge_{}", rand_id()));
                            }
                            if chain.node.variable.is_none() {
                                chain.node.variable = Some(format!("__merge_{}", rand_id()));
                            }
                        }
                    }
                }
                plan = LogicalPlan::Merge {
                    input: Box::new(plan),
                    pattern: pattern.clone(),
                    on_create: mc.on_create.clone(),
                    on_match: mc.on_match.clone(),
                };
                for v in collect_pattern_variables(&pattern) {
                    bound_vars.insert(v);
                }
                // bind named path variables of MERGE patterns
                for part in &pattern.parts {
                    if let Some(ref path_var) = part.variable {
                        let mut node_vars = Vec::new();
                        let mut edge_vars = Vec::new();
                        if let Some(ref v) = part.element.node.variable {
                            node_vars.push(v.clone());
                        }
                        for chain in &part.element.chain {
                            if let Some(ref v) = chain.rel.variable {
                                edge_vars.push(v.clone());
                            }
                            if let Some(ref v) = chain.node.variable {
                                node_vars.push(v.clone());
                            }
                        }
                        plan = LogicalPlan::BindPath {
                            input: Box::new(plan),
                            variable: path_var.clone(),
                            node_vars,
                            edge_vars,
                        };
                        bound_vars.insert(path_var.clone());
                    }
                }
            }
            Clause::Delete(dc) => {
                plan = LogicalPlan::Delete {
                    input: Box::new(plan),
                    exprs: dc.exprs.clone(),
                    detach: dc.detach,
                };
            }
            Clause::Set(sc) => {
                plan = LogicalPlan::SetProperty {
                    input: Box::new(plan),
                    items: sc.items.clone(),
                };
            }
            Clause::Remove(rc) => {
                plan = LogicalPlan::RemoveOp {
                    input: Box::new(plan),
                    items: rc.items.clone(),
                };
            }
            Clause::Unwind(uc) => {
                // dead source: `UNWIND xs AS x` with no LATER use of `xs` lets the operator move
                // the list out instead of cloning it per element
                let clear_source = match &uc.expr {
                    Expr::Variable(v) if *v != uc.alias => {
                        !clauses[clause_idx + 1..]
                            .iter()
                            .any(|c| clause_may_reference_var(c, v))
                            && !tail.may_reference_var(v)
                    }
                    _ => false,
                };
                plan = LogicalPlan::Unwind {
                    input: Box::new(plan),
                    expr: uc.expr.clone(),
                    alias: uc.alias.clone(),
                    clear_source,
                };
                bound_vars.insert(uc.alias.clone());
            }
            Clause::LoadCsv(lc) => {
                plan = LogicalPlan::LoadCsv {
                    input: Box::new(plan),
                    url: lc.url.clone(),
                    with_headers: lc.with_headers,
                    row_alias: lc.row_alias.clone(),
                    field_terminator: lc.field_terminator,
                };
                bound_vars.insert(lc.row_alias.clone());
            }
            Clause::ForEach(fc) => {
                // body sees the loop variable plus every bound outer variable; update clauses are
                // planned over a fresh EmptyRow the executor seeds per element
                let mut body_bound = bound_vars.clone();
                body_bound.insert(fc.variable.clone());
                let body = plan_clauses_with_hints(
                    &fc.updates,
                    LogicalPlan::EmptyRow,
                    has_match,
                    &mut body_bound,
                    hints,
                    &TailScope::Unknown,
                )?;
                plan = LogicalPlan::ForEach {
                    input: Box::new(plan),
                    variable: fc.variable.clone(),
                    list: fc.list.clone(),
                    body: Box::new(body),
                };
                // FOREACH exports nothing (loop variable and body bindings stay inside)
            }
            Clause::Call(cc) => {
                let yield_columns = match &cc.yield_items {
                    Some(YieldItems::Explicit(items)) => items
                        .iter()
                        .map(|yi| {
                            let bound = yi.alias.clone().unwrap_or_else(|| yi.name.clone());
                            (yi.name.clone(), bound.clone())
                        })
                        .collect(),
                    _ => Vec::new(), // All or None: resolved at execution
                };
                for (_, bound) in &yield_columns {
                    bound_vars.insert(bound.clone());
                }
                plan = LogicalPlan::CallProcedure {
                    input: Box::new(plan),
                    procedure_name: cc.procedure_name.clone(),
                    args: cc.args.clone(),
                    yield_columns,
                };
            }
            Clause::CallSubquery(cs) => {
                // plan the body on its own scope (EmptyRow leaf); the executor seeds each run with
                // the outer record (narrowed to imports if explicit) via exec_subquery
                let body_plan = plan_regular_query_with_hints(&cs.body, hints)?;
                // body's RETURN aliases, for downstream clauses. No RETURN (FINISH / write-only):
                // nothing new in scope, outer row passes through
                let return_aliases: Vec<String> = match cs
                    .body
                    .single
                    .final_part
                    .return_clause
                    .as_ref()
                    .map(|r| &r.items)
                {
                    Some(ProjectionItems::Explicit(items)) => items
                        .iter()
                        .filter_map(|it| {
                            if let Some(a) = &it.alias {
                                Some(a.clone())
                            } else if let Expr::Variable(v) = &it.expr {
                                Some(v.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                for a in &return_aliases {
                    bound_vars.insert(a.clone());
                }
                if let Some(it) = &cs.in_transactions {
                    if let Some(rs) = &it.report_status {
                        bound_vars.insert(rs.clone());
                    }
                    plan = LogicalPlan::CallInTransactions {
                        input: Box::new(plan),
                        body: Box::new(body_plan),
                        batch_size: it.batch_size.clone(),
                        on_error: it.on_error,
                        report_status: it.report_status.clone(),
                        return_aliases,
                    };
                } else {
                    plan = LogicalPlan::CallSubquery {
                        input: Box::new(plan),
                        imports: cs.imports.clone(),
                        body: Box::new(body_plan),
                        return_aliases,
                    };
                }
            }
        }
    }
    Ok(plan)
}

/// Conservative: could `clause` reference `v`? `true` for kinds not inspected; a wrong answer only
/// costs the Unwind take-instead-of-clone, never correctness.
fn clause_may_reference_var(clause: &Clause, v: &str) -> bool {
    #[recursive::recursive]
    fn expr_refs(e: &Expr, v: &str) -> bool {
        let mut vars = std::collections::HashSet::new();
        collect_expr_variables(e, &mut vars);
        vars.contains(v)
    }
    fn pattern_refs(p: &Pattern, v: &str) -> bool {
        p.parts.iter().any(|part| {
            let elem = &part.element;
            let node_ref = |n: &NodePattern| {
                n.variable.as_deref() == Some(v)
                    || n.properties.as_ref().is_some_and(|e| expr_refs(e, v))
            };
            if node_ref(&elem.node) {
                return true;
            }
            part.variable.as_deref() == Some(v)
                || elem.chain.iter().any(|c| {
                    node_ref(&c.node)
                        || c.rel.variable.as_deref() == Some(v)
                        || c.rel.properties.as_ref().is_some_and(|e| expr_refs(e, v))
                })
        })
    }
    match clause {
        Clause::Match(mc) | Clause::OptionalMatch(mc) => {
            pattern_refs(&mc.pattern, v)
                || mc.where_clause.as_ref().is_some_and(|w| expr_refs(w, v))
        }
        Clause::Unwind(uc) => expr_refs(&uc.expr, v),
        // anything else (CREATE/MERGE/SET/DELETE/CALL/FOREACH/subqueries/...) is assumed to use `v`
        _ => true,
    }
}
