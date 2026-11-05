//! Plan walking: rebuilding a plan bottom-up and the variables plans and expressions use.

use std::collections::HashSet;

use crate::ast::Expr;
use crate::planner::{LogicalPlan, collect_expr_variables};

/// Does `e` use `variable`?
pub(super) fn expr_uses_var(e: &Expr, variable: &str) -> bool {
    let mut vars = std::collections::HashSet::new();
    collect_expr_variables(e, &mut vars);
    vars.contains(variable)
}

/// Variables an expression references.
pub(super) fn expr_variables(expr: &Expr) -> HashSet<String> {
    let mut vars = HashSet::new();
    collect_expr_variables(expr, &mut vars);
    vars
}

/// Output variables of a plan node.
pub fn plan_output_variables(plan: &LogicalPlan) -> HashSet<String> {
    let mut vars = HashSet::new();
    collect_plan_variables(plan, &mut vars);
    vars
}

#[recursive::recursive]
fn collect_plan_variables(plan: &LogicalPlan, out: &mut HashSet<String>) {
    match plan {
        LogicalPlan::ContextSeed => {}
        LogicalPlan::Scan { variable, .. } => {
            out.insert(variable.clone());
        }
        LogicalPlan::IndexLookupJoin {
            input, variable, ..
        } => {
            collect_plan_variables(input, out);
            out.insert(variable.clone());
        }
        LogicalPlan::IndexScan { variable, .. }
        | LogicalPlan::IndexRangeScan { variable, .. }
        | LogicalPlan::PointIndexScan { variable, .. }
        | LogicalPlan::TextIndexScan { variable, .. }
        | LogicalPlan::NodeById { variable, .. }
        | LogicalPlan::NodeByIdRange { variable, .. } => {
            out.insert(variable.clone());
        }
        LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            ..
        } => {
            out.insert(from_var.clone());
            if !rel_var.is_empty() {
                out.insert(rel_var.clone());
            }
            out.insert(to_var.clone());
        }
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            ..
        }
        | LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            ..
        }
        | LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(from.clone());
            if !rel_var.is_empty() {
                out.insert(rel_var.clone());
            }
            out.insert(to.clone());
        }
        LogicalPlan::Filter { input, .. } => {
            collect_plan_variables(input, out);
        }
        LogicalPlan::Project { input, items, .. } => {
            collect_plan_variables(input, out);
            for item in items {
                if let Some(alias) = &item.alias {
                    out.insert(alias.clone());
                }
            }
        }
        LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Eager { input } => {
            collect_plan_variables(input, out);
        }
        LogicalPlan::Aggregate { keys, aggs, .. } => {
            for item in keys.iter().chain(aggs.iter()) {
                if let Some(alias) = &item.alias {
                    out.insert(alias.clone());
                } else {
                    out.insert(crate::planner::simple_expr_key(&item.expr));
                }
            }
        }
        LogicalPlan::Union { left, .. } => {
            collect_plan_variables(left, out);
        }
        LogicalPlan::Unwind { input, alias, .. } => {
            collect_plan_variables(input, out);
            out.insert(alias.clone());
        }
        LogicalPlan::LoadCsv {
            input, row_alias, ..
        } => {
            collect_plan_variables(input, out);
            out.insert(row_alias.clone());
        }
        LogicalPlan::CreateNode {
            input, variable, ..
        } => {
            collect_plan_variables(input, out);
            if let Some(v) = variable {
                out.insert(v.clone());
            }
        }
        LogicalPlan::CreateRel {
            input, variable, ..
        } => {
            collect_plan_variables(input, out);
            if let Some(v) = variable {
                out.insert(v.clone());
            }
        }
        LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. } => {
            collect_plan_variables(input, out);
        }
        LogicalPlan::Merge { input, .. } => {
            collect_plan_variables(input, out);
        }
        LogicalPlan::OptionalMatch {
            input,
            new_variables,
            ..
        } => {
            collect_plan_variables(input, out);
            for v in new_variables {
                out.insert(v.clone());
            }
        }
        LogicalPlan::CrossProduct { left, right } => {
            collect_plan_variables(left, out);
            collect_plan_variables(right, out);
        }
        LogicalPlan::HashJoin { left, right, .. } => {
            collect_plan_variables(left, out);
            collect_plan_variables(right, out);
        }
        LogicalPlan::BindPath {
            input, variable, ..
        } => {
            collect_plan_variables(input, out);
            out.insert(variable.clone());
        }
        LogicalPlan::BindPathEndpoints {
            input,
            from_var,
            to_var,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(from_var.clone());
            out.insert(to_var.clone());
        }
        LogicalPlan::BindEdgeEndpoints {
            input,
            from_var,
            to_var,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(from_var.clone());
            out.insert(to_var.clone());
        }
        LogicalPlan::CallProcedure {
            input,
            yield_columns,
            ..
        } => {
            collect_plan_variables(input, out);
            for (_, var) in yield_columns {
                out.insert(var.clone());
            }
        }
        LogicalPlan::VectorSearchScan {
            input,
            variable,
            score_var,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(variable.clone());
            if let Some(sv) = score_var {
                out.insert(sv.clone());
            }
        }
        LogicalPlan::BindQppGroups {
            input,
            src_var,
            dst_var,
            ..
        } => {
            collect_plan_variables(input, out);
            if let Some(v) = src_var {
                out.insert(v.clone());
            }
            if let Some(v) = dst_var {
                out.insert(v.clone());
            }
        }
        LogicalPlan::BindVarLists { input, groups } => {
            collect_plan_variables(input, out);
            for (v, _) in groups {
                out.insert(v.clone());
            }
        }
        LogicalPlan::CallSubquery {
            input,
            body,
            return_aliases,
            ..
        }
        | LogicalPlan::CallInTransactions {
            input,
            body,
            return_aliases,
            ..
        } => {
            collect_plan_variables(input, out);
            collect_plan_variables(body, out);
            for a in return_aliases {
                out.insert(a.clone());
            }
        }
        LogicalPlan::ForEach {
            input,
            variable,
            body,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(variable.clone());
            collect_plan_variables(body, out);
        }
        LogicalPlan::ShortestPath {
            input,
            from,
            to,
            path_var,
            rel_var,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(from.clone());
            out.insert(to.clone());
            if let Some(pv) = path_var {
                out.insert(pv.clone());
            }
            if let Some(rv) = rel_var {
                out.insert(rv.clone());
            }
        }
        LogicalPlan::WeightedShortestPath {
            input,
            from,
            to,
            path_var,
            rel_var,
            cost_var,
            ..
        } => {
            collect_plan_variables(input, out);
            out.insert(from.clone());
            out.insert(to.clone());
            if let Some(pv) = path_var {
                out.insert(pv.clone());
            }
            if let Some(rv) = rel_var {
                out.insert(rv.clone());
            }
            if let Some(cv) = cost_var {
                out.insert(cv.clone());
            }
        }
        LogicalPlan::DistinctExpand {
            input, from, to, ..
        } => {
            collect_plan_variables(input, out);
            out.insert(from.clone());
            out.insert(to.clone());
        }
        LogicalPlan::ScanFilterProject {
            variable, items, ..
        } => {
            out.insert(variable.clone());
            for item in items {
                if let Some(alias) = &item.alias {
                    out.insert(alias.clone());
                }
            }
        }
        LogicalPlan::EmptyRow => {}
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
        | LogicalPlan::ShowSettings => {}
        LogicalPlan::TransactionCommand { input, command, .. } => {
            collect_plan_variables(input, out);
            for column in command.columns() {
                out.insert((*column).to_string());
            }
        }
    }
}

/// Rebuilds `plan` by value, applying `f` to every child subtree, then the node (post-order).
/// Optimizer passes are `f`s over this walker.
#[recursive::recursive]
pub fn recurse_transform(plan: LogicalPlan, f: &dyn Fn(LogicalPlan) -> LogicalPlan) -> LogicalPlan {
    let transformed = match plan {
        LogicalPlan::ContextSeed
        | LogicalPlan::Scan { .. }
        | LogicalPlan::IndexScan { .. }
        | LogicalPlan::IndexRangeScan { .. }
        | LogicalPlan::PointIndexScan { .. }
        | LogicalPlan::TextIndexScan { .. }
        | LogicalPlan::NodeById { .. }
        | LogicalPlan::NodeByIdRange { .. }
        | LogicalPlan::EdgeTypeScan { .. }
        | LogicalPlan::ScanFilterProject { .. }
        | LogicalPlan::EmptyRow
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
        | LogicalPlan::DropEdgeIndex { .. }
        | LogicalPlan::ShowIndexes
        | LogicalPlan::ShowConstraints
        | LogicalPlan::ShowFunctions
        | LogicalPlan::ShowProcedures
        | LogicalPlan::ShowSettings => plan,
        LogicalPlan::TransactionCommand {
            input,
            command,
            ids,
        } => LogicalPlan::TransactionCommand {
            input: Box::new(recurse_transform(*input, f)),
            command,
            ids,
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
            input: Box::new(recurse_transform(*input, f)),
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
            input: Box::new(recurse_transform(*input, f)),
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
            input: Box::new(recurse_transform(*input, f)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(recurse_transform(*input, f)),
            predicate,
        },
        LogicalPlan::IndexLookupJoin {
            input,
            variable,
            label,
            property,
            key,
        } => LogicalPlan::IndexLookupJoin {
            input: Box::new(recurse_transform(*input, f)),
            variable,
            label,
            property,
            key,
        },
        LogicalPlan::VectorSearchScan {
            input,
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
        } => LogicalPlan::VectorSearchScan {
            input: Box::new(recurse_transform(*input, f)),
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
        },
        LogicalPlan::BindQppGroups {
            input,
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        } => LogicalPlan::BindQppGroups {
            input: Box::new(recurse_transform(*input, f)),
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        },
        LogicalPlan::BindVarLists { input, groups } => LogicalPlan::BindVarLists {
            input: Box::new(recurse_transform(*input, f)),
            groups,
        },
        LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } => LogicalPlan::Project {
            input: Box::new(recurse_transform(*input, f)),
            items,
            distinct,
            where_clause,
        },
        LogicalPlan::Sort { input, items } => LogicalPlan::Sort {
            input: Box::new(recurse_transform(*input, f)),
            items,
        },
        LogicalPlan::Limit { input, count } => LogicalPlan::Limit {
            input: Box::new(recurse_transform(*input, f)),
            count,
        },
        LogicalPlan::Skip { input, count } => LogicalPlan::Skip {
            input: Box::new(recurse_transform(*input, f)),
            count,
        },
        LogicalPlan::Eager { input } => LogicalPlan::Eager {
            input: Box::new(recurse_transform(*input, f)),
        },
        LogicalPlan::Aggregate { input, keys, aggs } => LogicalPlan::Aggregate {
            input: Box::new(recurse_transform(*input, f)),
            keys,
            aggs,
        },
        LogicalPlan::Union { left, right, all } => LogicalPlan::Union {
            left: Box::new(recurse_transform(*left, f)),
            right: Box::new(recurse_transform(*right, f)),
            all,
        },
        LogicalPlan::Unwind {
            input,
            expr,
            alias,
            clear_source,
        } => LogicalPlan::Unwind {
            input: Box::new(recurse_transform(*input, f)),
            expr,
            alias,
            clear_source,
        },
        LogicalPlan::LoadCsv {
            input,
            url,
            with_headers,
            row_alias,
            field_terminator,
        } => LogicalPlan::LoadCsv {
            input: Box::new(recurse_transform(*input, f)),
            url,
            with_headers,
            row_alias,
            field_terminator,
        },
        LogicalPlan::CreateNode {
            input,
            variable,
            labels,
            dynamic_labels,
            properties,
        } => LogicalPlan::CreateNode {
            input: Box::new(recurse_transform(*input, f)),
            variable,
            labels,
            dynamic_labels,
            properties,
        },
        LogicalPlan::CreateRel {
            input,
            variable,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
        } => LogicalPlan::CreateRel {
            input: Box::new(recurse_transform(*input, f)),
            variable,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
        },
        LogicalPlan::SetProperty { input, items } => LogicalPlan::SetProperty {
            input: Box::new(recurse_transform(*input, f)),
            items,
        },
        LogicalPlan::RemoveOp { input, items } => LogicalPlan::RemoveOp {
            input: Box::new(recurse_transform(*input, f)),
            items,
        },
        LogicalPlan::Delete {
            input,
            exprs,
            detach,
        } => LogicalPlan::Delete {
            input: Box::new(recurse_transform(*input, f)),
            exprs,
            detach,
        },
        LogicalPlan::Merge {
            input,
            pattern,
            on_create,
            on_match,
        } => LogicalPlan::Merge {
            input: Box::new(recurse_transform(*input, f)),
            pattern,
            on_create,
            on_match,
        },
        LogicalPlan::OptionalMatch {
            input,
            match_plan,
            new_variables,
        } => LogicalPlan::OptionalMatch {
            input: Box::new(recurse_transform(*input, f)),
            match_plan: Box::new(recurse_transform(*match_plan, f)),
            new_variables,
        },
        LogicalPlan::CrossProduct { left, right } => LogicalPlan::CrossProduct {
            left: Box::new(recurse_transform(*left, f)),
            right: Box::new(recurse_transform(*right, f)),
        },
        LogicalPlan::HashJoin {
            left,
            right,
            left_keys,
            right_keys,
        } => LogicalPlan::HashJoin {
            left: Box::new(recurse_transform(*left, f)),
            right: Box::new(recurse_transform(*right, f)),
            left_keys,
            right_keys,
        },
        LogicalPlan::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } => LogicalPlan::BindPath {
            input: Box::new(recurse_transform(*input, f)),
            variable,
            node_vars,
            edge_vars,
        },
        LogicalPlan::BindPathEndpoints {
            input,
            rel_var,
            from_var,
            to_var,
            direction,
        } => LogicalPlan::BindPathEndpoints {
            input: Box::new(recurse_transform(*input, f)),
            rel_var,
            from_var,
            to_var,
            direction,
        },
        LogicalPlan::BindEdgeEndpoints {
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        } => LogicalPlan::BindEdgeEndpoints {
            input: Box::new(recurse_transform(*input, f)),
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        },
        LogicalPlan::CallProcedure {
            input,
            procedure_name,
            args,
            yield_columns,
        } => LogicalPlan::CallProcedure {
            input: Box::new(recurse_transform(*input, f)),
            procedure_name,
            args,
            yield_columns,
        },
        LogicalPlan::CallSubquery {
            input,
            imports,
            body,
            return_aliases,
        } => {
            // rewrite the body in its own scope; no fusing with the outer plan (per-row scoping
            // rules it out)
            LogicalPlan::CallSubquery {
                input: Box::new(recurse_transform(*input, f)),
                imports,
                body: Box::new(recurse_transform(*body, f)),
                return_aliases,
            }
        }
        LogicalPlan::CallInTransactions {
            input,
            body,
            batch_size,
            on_error,
            report_status,
            return_aliases,
        } => LogicalPlan::CallInTransactions {
            input: Box::new(recurse_transform(*input, f)),
            // the body's leaf is seeded per row at execution, so leave it; its interior can still
            // be optimised
            body: Box::new(recurse_transform(*body, f)),
            batch_size,
            on_error,
            report_status,
            return_aliases,
        },
        LogicalPlan::ForEach {
            input,
            variable,
            list,
            body,
        } => LogicalPlan::ForEach {
            input: Box::new(recurse_transform(*input, f)),
            variable,
            list,
            body: Box::new(recurse_transform(*body, f)),
        },
        LogicalPlan::ShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            min_depth,
            max_depth,
            path_var,
            rel_var,
            all,
            k,
            k_groups,
        } => LogicalPlan::ShortestPath {
            input: Box::new(recurse_transform(*input, f)),
            from,
            to,
            edge_types,
            direction,
            min_depth,
            max_depth,
            path_var,
            rel_var,
            all,
            k,
            k_groups,
        },
        LogicalPlan::WeightedShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path_var,
            rel_var,
            cost_var,
        } => LogicalPlan::WeightedShortestPath {
            input: Box::new(recurse_transform(*input, f)),
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path_var,
            rel_var,
            cost_var,
        },
        LogicalPlan::DistinctExpand {
            input,
            from,
            to,
            min_hops,
            max_hops,
            direction,
            edge_types,
            target_label,
            global_dedup,
            target_filter,
        } => LogicalPlan::DistinctExpand {
            input: Box::new(recurse_transform(*input, f)),
            from,
            to,
            min_hops,
            max_hops,
            direction,
            edge_types,
            target_label,
            global_dedup,
            target_filter,
        },
    };

    f(transformed)
}
