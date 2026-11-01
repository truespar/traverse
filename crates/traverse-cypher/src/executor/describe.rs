//! What a plan is: its query type for Bolt metadata and its result columns.

use crate::ast::{BinOp, CmpOp, Expr, Literal, ProjectionItem, StringOp, UnaryOp};
use crate::planner::LogicalPlan;

use super::context::{QueryStats, QueryType};

/// Query type of a plan without executing it (empty stats). The internal `classify_query` also
/// accounts for runtime mutations.
pub fn classify_plan(plan: &LogicalPlan) -> QueryType {
    classify_query(plan, &QueryStats::default())
}

pub(super) fn classify_query(plan: &LogicalPlan, stats: &QueryStats) -> QueryType {
    if plan_is_schema(plan) {
        return QueryType::Schema;
    }
    let mutating = plan_is_mutating(plan) || !stats.is_empty();
    let has_return = plan_has_projection(plan);
    match (mutating, has_return) {
        (true, true) => QueryType::ReadWrite,
        (true, false) => QueryType::Write,
        _ => QueryType::Read,
    }
}

/// Refines a pre-classified query type with runtime stats (streaming): `Read` with observed
/// mutations becomes `ReadWrite` (a UDF or procedure that writes).
pub(super) fn classify_query_from_hint(hint: QueryType, stats: &QueryStats) -> QueryType {
    if hint == QueryType::Read && !stats.is_empty() {
        QueryType::ReadWrite
    } else {
        hint
    }
}

/// Does `pred` hold for any operator of `plan`? Iterative over every child, so no operator hides
/// what's under it and deep plans can't overflow the stack.
fn any_operator(plan: &LogicalPlan, pred: impl Fn(&LogicalPlan) -> bool) -> bool {
    let mut stack = vec![plan];
    while let Some(node) = stack.pop() {
        if pred(node) {
            return true;
        }
        stack.extend(crate::stack::children(node));
    }
    false
}

/// Plan contains schema operations?
fn plan_is_schema(plan: &LogicalPlan) -> bool {
    any_operator(plan, |node| {
        matches!(
            node,
            LogicalPlan::CreateIndex { .. }
                | LogicalPlan::CreateVectorIndex { .. }
                | LogicalPlan::CreateFulltextIndex { .. }
                | LogicalPlan::CreatePointIndex { .. }
                | LogicalPlan::CreateTextIndex { .. }
                | LogicalPlan::DropIndex { .. }
                | LogicalPlan::AnalyzeGraph { .. }
                | LogicalPlan::CreateConstraint { .. }
                | LogicalPlan::DropConstraint { .. }
                | LogicalPlan::CreateEdgeIndex { .. }
                | LogicalPlan::DropEdgeIndex { .. }
                | LogicalPlan::ShowIndexes
                | LogicalPlan::ShowConstraints
                | LogicalPlan::ShowFunctions
                | LogicalPlan::ShowProcedures
                | LogicalPlan::TransactionCommand { .. }
                | LogicalPlan::ShowSettings
        )
    })
}

/// Does the plan change data or schema: a write clause or DDL anywhere in it, or a procedure that
/// writes ([`procedure_writes`](crate::operators::call::procedure_writes))?
pub(crate) fn plan_is_mutating(plan: &LogicalPlan) -> bool {
    any_operator(plan, |node| match node {
        LogicalPlan::CreateNode { .. }
        | LogicalPlan::CreateRel { .. }
        | LogicalPlan::Delete { .. }
        | LogicalPlan::SetProperty { .. }
        | LogicalPlan::RemoveOp { .. }
        | LogicalPlan::Merge { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::CreateVectorIndex { .. }
        | LogicalPlan::CreateFulltextIndex { .. }
        | LogicalPlan::CreatePointIndex { .. }
        | LogicalPlan::CreateTextIndex { .. }
        | LogicalPlan::DropIndex { .. }
        | LogicalPlan::CreateConstraint { .. }
        | LogicalPlan::DropConstraint { .. }
        | LogicalPlan::CreateEdgeIndex { .. }
        | LogicalPlan::DropEdgeIndex { .. } => true,
        LogicalPlan::CallProcedure { procedure_name, .. } => {
            crate::operators::call::procedure_writes(procedure_name)
        }
        _ => false,
    })
}

/// Plan has a projection/aggregation (a RETURN)?
#[recursive::recursive]
pub(super) fn plan_has_projection(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Project { .. }
        | LogicalPlan::Aggregate { .. }
        | LogicalPlan::ScanFilterProject { .. } => true,
        LogicalPlan::CallProcedure { procedure_name, .. } => {
            // standalone CALL projects if the procedure has outputs, per the registry and the
            // built-in schema procedures (so bare `CALL db.labels()` yields `label` rows)
            crate::procedure::get_procedure(procedure_name)
                .map(|p| !p.outputs.is_empty())
                .unwrap_or(false)
                || crate::procedure::builtin_schema_proc_columns(procedure_name)
                    .is_some_and(|c| !c.is_empty())
        }
        LogicalPlan::Union { left, .. } => plan_has_projection(left),
        LogicalPlan::Sort { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::Filter { input, .. } => plan_has_projection(input),
        LogicalPlan::ShowIndexes
        | LogicalPlan::ShowConstraints
        | LogicalPlan::ShowFunctions
        | LogicalPlan::ShowProcedures
        | LogicalPlan::TransactionCommand { .. }
        | LogicalPlan::ShowSettings => true,
        LogicalPlan::IndexScan { .. }
        | LogicalPlan::PointIndexScan { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::CreateVectorIndex { .. }
        | LogicalPlan::CreateFulltextIndex { .. }
        | LogicalPlan::CreatePointIndex { .. }
        | LogicalPlan::CreateTextIndex { .. }
        | LogicalPlan::DropIndex { .. }
        | LogicalPlan::AnalyzeGraph { .. }
        | LogicalPlan::CreateConstraint { .. }
        | LogicalPlan::DropConstraint { .. }
        | LogicalPlan::CreateEdgeIndex { .. }
        | LogicalPlan::DropEdgeIndex { .. } => false,
        _ => false,
    }
}

/// Output column names of a plan; empty for `RETURN *` or plans without a projection.
#[recursive::recursive]
pub fn plan_columns(plan: &LogicalPlan) -> Vec<String> {
    match plan {
        LogicalPlan::Project { items, .. } => {
            if items.is_empty() {
                // RETURN *: columns unknown up front
                Vec::new()
            } else {
                items.iter().map(projection_name).collect()
            }
        }
        LogicalPlan::Aggregate { keys, aggs, .. } => {
            let mut cols: Vec<String> = keys.iter().map(projection_name).collect();
            cols.extend(aggs.iter().map(projection_name));
            cols
        }
        LogicalPlan::CallProcedure {
            procedure_name,
            yield_columns,
            ..
        } => {
            if yield_columns.is_empty() {
                // no explicit YIELD: all output columns, from the registry, then the built-in
                // schema procedures (bare `CALL db.labels()` projects `label`)
                if let Some(p) = crate::procedure::get_procedure(procedure_name) {
                    p.outputs.iter().map(|o| o.name.clone()).collect()
                } else if let Some(cols) =
                    crate::procedure::builtin_schema_proc_columns(procedure_name)
                {
                    cols.iter().map(|c| (*c).to_string()).collect()
                } else {
                    Vec::new()
                }
            } else {
                yield_columns
                    .iter()
                    .map(|(_, bound)| bound.clone())
                    .collect()
            }
        }
        LogicalPlan::Union { left, .. } => plan_columns(left),
        LogicalPlan::Sort { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::Filter { input, .. } => plan_columns(input),
        LogicalPlan::ScanFilterProject { items, .. } => items.iter().map(projection_name).collect(),
        LogicalPlan::ShowIndexes | LogicalPlan::ShowConstraints => {
            vec![
                "name".to_string(),
                "type".to_string(),
                "entityType".to_string(),
                "labelsOrTypes".to_string(),
                "properties".to_string(),
            ]
        }
        LogicalPlan::ShowFunctions => vec![
            "name".to_string(),
            "category".to_string(),
            "description".to_string(),
            "signature".to_string(),
            "aggregating".to_string(),
        ],
        LogicalPlan::ShowProcedures => vec![
            "name".to_string(),
            "description".to_string(),
            "signature".to_string(),
            "mode".to_string(),
        ],
        LogicalPlan::TransactionCommand { command, .. } => {
            command.columns().iter().map(|c| (*c).to_string()).collect()
        }
        LogicalPlan::ShowSettings => vec![
            "name".to_string(),
            "value".to_string(),
            "isDynamic".to_string(),
            "defaultValue".to_string(),
            "description".to_string(),
        ],
        LogicalPlan::IndexScan { .. }
        | LogicalPlan::PointIndexScan { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::CreateVectorIndex { .. }
        | LogicalPlan::CreateFulltextIndex { .. }
        | LogicalPlan::CreatePointIndex { .. }
        | LogicalPlan::CreateTextIndex { .. }
        | LogicalPlan::DropIndex { .. }
        | LogicalPlan::AnalyzeGraph { .. }
        | LogicalPlan::CreateConstraint { .. }
        | LogicalPlan::DropConstraint { .. }
        | LogicalPlan::CreateEdgeIndex { .. }
        | LogicalPlan::DropEdgeIndex { .. } => Vec::new(),
        _ => Vec::new(),
    }
}

/// All columns visible at this plan node (for RETURN *), inherited ones included, unlike
/// `plan_columns` which gives only the node's own output.
#[recursive::recursive]
pub(super) fn plan_visible_columns(plan: &LogicalPlan) -> Vec<String> {
    match plan {
        // Aggregate replaces columns with its outputs
        LogicalPlan::Aggregate { keys, aggs, .. } => {
            let mut cols: Vec<String> = keys.iter().map(projection_name).collect();
            cols.extend(aggs.iter().map(projection_name));
            cols
        }
        // Project replaces columns with its outputs (non-*)
        LogicalPlan::Project { items, input, .. } => {
            if items.is_empty() {
                plan_visible_columns(input)
            } else {
                items.iter().map(projection_name).collect()
            }
        }
        // CallProcedure adds its yield columns
        LogicalPlan::CallProcedure {
            input,
            yield_columns,
            procedure_name,
            ..
        } => {
            let mut cols = plan_visible_columns(input);
            if yield_columns.is_empty() {
                if let Some(proc) = crate::procedure::get_procedure(procedure_name) {
                    for out in &proc.outputs {
                        if !cols.contains(&out.name) {
                            cols.push(out.name.clone());
                        }
                    }
                } else if let Some(bcols) =
                    crate::procedure::builtin_schema_proc_columns(procedure_name)
                {
                    for c in bcols {
                        if !cols.iter().any(|x| x == c) {
                            cols.push((*c).to_string());
                        }
                    }
                }
            } else {
                for (_, bound) in yield_columns {
                    if !cols.contains(bound) {
                        cols.push(bound.clone());
                    }
                }
            }
            cols
        }
        // pass-through of input columns
        LogicalPlan::Sort { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. } => plan_visible_columns(input),
        // Unwind adds its alias
        LogicalPlan::Unwind { input, alias, .. } => {
            let mut cols = plan_visible_columns(input);
            if !cols.contains(alias) {
                cols.push(alias.clone());
            }
            cols
        }
        // LoadCsv adds row_alias
        LogicalPlan::LoadCsv {
            input, row_alias, ..
        } => {
            let mut cols = plan_visible_columns(input);
            if !cols.contains(row_alias) {
                cols.push(row_alias.clone());
            }
            cols
        }
        LogicalPlan::ScanFilterProject { items, .. } => items.iter().map(projection_name).collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn projection_name(item: &ProjectionItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    if let Some(ref source) = item.source_text {
        return source.clone();
    }
    expr_name(&item.expr)
}

#[recursive::recursive]
fn expr_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(base, key) => {
            format!("{}.{}", expr_name(base), key)
        }
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => {
            let display_name = canonical_function_name(name);
            if args.is_empty() {
                if *distinct {
                    format!("{display_name}(DISTINCT *)")
                } else {
                    format!("{display_name}(*)")
                }
            } else {
                let arg_names: Vec<String> = args.iter().map(expr_name).collect();
                if *distinct {
                    format!("{display_name}(DISTINCT {})", arg_names.join(", "))
                } else {
                    format!("{display_name}({})", arg_names.join(", "))
                }
            }
        }
        Expr::Literal(Literal::Integer(n)) => n.to_string(),
        Expr::Literal(Literal::Float(f)) => f.to_string(),
        Expr::Literal(Literal::String(s)) => format!("\"{s}\""),
        Expr::Literal(Literal::Bool(b)) => b.to_string(),
        Expr::Literal(Literal::Null) => "null".to_string(),
        Expr::IsNull(inner) => format!("{} IS NULL", expr_name(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_name(inner)),
        Expr::BinaryOp(l, op, r) => {
            let op_str = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Mod => "%",
                BinOp::Pow => "^",
                BinOp::Concat => "||",
                BinOp::And => "AND",
                BinOp::Or => "OR",
                BinOp::Xor => "XOR",
            };
            format!("{} {} {}", expr_name(l), op_str, expr_name(r))
        }
        Expr::UnaryOp(UnaryOp::Not, inner) => format!("NOT {}", expr_name(inner)),
        Expr::UnaryOp(UnaryOp::Neg, inner) => format!("-{}", expr_name(inner)),
        Expr::Comparison(base, pairs) => {
            let mut s = expr_name(base);
            for (op, e) in pairs {
                let op_str = match op {
                    CmpOp::Eq => "=",
                    CmpOp::Neq => "<>",
                    CmpOp::Lt => "<",
                    CmpOp::Lte => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Gte => ">=",
                };
                s = format!("{s} {op_str} {}", expr_name(e));
            }
            s
        }
        Expr::In(l, r) => format!("{} IN {}", expr_name(l), expr_name(r)),
        Expr::ListLiteral(items) => {
            let names: Vec<String> = items.iter().map(expr_name).collect();
            format!("[{}]", names.join(", "))
        }
        Expr::ListIndex(base, idx) => {
            format!("{}[{}]", expr_name(base), expr_name(idx))
        }
        Expr::StringOp(l, op, r) => {
            let op_str = match op {
                StringOp::StartsWith => "STARTS WITH",
                StringOp::EndsWith => "ENDS WITH",
                StringOp::Contains => "CONTAINS",
                StringOp::Regex => "=~",
            };
            format!("{} {} {}", expr_name(l), op_str, expr_name(r))
        }
        _ => "expr".to_string(),
    }
}

/// Lowercased function name -> canonical display form.
fn canonical_function_name(name: &str) -> &str {
    match name {
        "tostring" => "toString",
        "tointeger" | "toint" => "toInteger",
        "tofloat" => "toFloat",
        "toboolean" => "toBoolean",
        "toupper" | "touppercase" => "toUpper",
        "tolower" | "tolowercase" => "toLower",
        "startnode" => "startNode",
        "endnode" => "endNode",
        "typename" | "valuetype" => "valueType",
        "localdatetime" => "localDateTime",
        "localtime" => "localTime",
        "percentilecont" => "percentileCont",
        "percentiledisc" => "percentileDisc",
        "stdev" => "stDev",
        "stdevp" => "stDevP",
        other => other,
    }
}
