//! Plans as values for EXPLAIN and PROFILE.

use std::collections::BTreeMap;

use crate::planner::{LogicalPlan, SpatialQuery};
use crate::types::CypherValue;

use super::describe::projection_name;

/// LogicalPlan -> nested CypherValue Map for EXPLAIN/PROFILE.
#[recursive::recursive]
pub fn plan_to_cypher_value(plan: &LogicalPlan) -> CypherValue {
    let mut map = BTreeMap::new();

    let (operator_type, args, children) = match plan {
        LogicalPlan::Scan { variable, label } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            if let Some(lbl) = label {
                args.insert("label".into(), CypherValue::String(lbl.clone()));
            }
            ("AllNodesScan", args, vec![])
        }
        LogicalPlan::ContextSeed => ("ContextSeed", BTreeMap::new(), vec![]),
        LogicalPlan::IndexScan {
            variable,
            label,
            property,
            value,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            args.insert("value".into(), CypherValue::String(format!("{value:?}")));
            ("NodeIndexScan", args, vec![])
        }
        LogicalPlan::IndexLookupJoin {
            input,
            variable,
            label,
            property,
            key,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            args.insert("key".into(), CypherValue::String(format!("{key:?}")));
            (
                "NodeIndexLookupJoin",
                args,
                vec![plan_to_cypher_value(input)],
            )
        }
        LogicalPlan::PointIndexScan {
            variable,
            label,
            property,
            query,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            let kind = match query {
                SpatialQuery::Distance { .. } => "distance",
                SpatialQuery::BBox { .. } => "withinBBox",
                SpatialQuery::Knn { .. } => "knn",
            };
            args.insert("query".into(), CypherValue::String(kind.into()));
            ("NodePointIndexScan", args, vec![])
        }
        LogicalPlan::TextIndexScan {
            variable,
            label,
            property,
            kind,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            args.insert("predicate".into(), CypherValue::String(format!("{kind:?}")));
            ("NodeTextIndexScan", args, vec![])
        }
        LogicalPlan::VectorSearchScan {
            input,
            variable,
            index_name,
            index_where,
            limit,
            score_var,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("index".into(), CypherValue::String(index_name.clone()));
            args.insert("limit".into(), CypherValue::String(format!("{limit:?}")));
            if index_where.is_some() {
                args.insert("filtered".into(), CypherValue::Bool(true));
            }
            if let Some(sv) = score_var {
                args.insert("score".into(), CypherValue::String(sv.clone()));
            }
            ("VectorSearch", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::NodeById {
            variable,
            label,
            id_expr,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            if let Some(lbl) = label {
                args.insert("label".into(), CypherValue::String(lbl.clone()));
            }
            args.insert(
                "id_expr".into(),
                CypherValue::String(format!("{id_expr:?}")),
            );
            ("NodeByIdSeek", args, vec![])
        }
        LogicalPlan::NodeByIdRange {
            variable,
            label,
            lower,
            upper,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            if let Some(lbl) = label {
                args.insert("label".into(), CypherValue::String(lbl.clone()));
            }
            args.insert(
                "lower".into(),
                CypherValue::String(
                    lower
                        .as_ref()
                        .map(|e| format!("{e:?}"))
                        .unwrap_or_else(|| "-inf".into()),
                ),
            );
            args.insert(
                "upper".into(),
                CypherValue::String(
                    upper
                        .as_ref()
                        .map(|e| format!("{e:?}"))
                        .unwrap_or_else(|| "+inf".into()),
                ),
            );
            ("NodeByIdRangeScan", args, vec![])
        }
        LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from_var.clone()));
            if !rel_var.is_empty() {
                args.insert("rel".into(), CypherValue::String(rel_var.clone()));
            }
            args.insert("to".into(), CypherValue::String(to_var.clone()));
            args.insert(
                "edge_types".into(),
                CypherValue::String(edge_types.join("|")),
            );
            args.insert(
                "direction".into(),
                CypherValue::String(format!("{direction:?}")),
            );
            ("EdgeTypeScan", args, vec![])
        }
        LogicalPlan::IndexRangeScan {
            variable,
            label,
            property,
            op,
            value,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            args.insert("op".into(), CypherValue::String(format!("{op:?}")));
            args.insert("value".into(), CypherValue::String(format!("{value:?}")));
            ("NodeIndexRangeScan", args, vec![])
        }
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            if !rel_var.is_empty() {
                args.insert("rel".into(), CypherValue::String(rel_var.clone()));
            }
            if !edge_types.is_empty() {
                args.insert("types".into(), CypherValue::String(edge_types.join("|")));
            }
            ("Expand(All)", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            if !rel_var.is_empty() {
                args.insert("rel".into(), CypherValue::String(rel_var.clone()));
            }
            if !edge_types.is_empty() {
                args.insert("types".into(), CypherValue::String(edge_types.join("|")));
            }
            ("Expand(Into)", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            if !rel_var.is_empty() {
                args.insert("rel".into(), CypherValue::String(rel_var.clone()));
            }
            if !edge_types.is_empty() {
                args.insert("types".into(), CypherValue::String(edge_types.join("|")));
            }
            ("Expand(Both)", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Filter { input, predicate } => {
            let mut args = BTreeMap::new();
            args.insert(
                "predicate".into(),
                CypherValue::String(format!("{predicate:?}")),
            );
            ("Filter", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Project {
            input,
            items,
            distinct,
            ..
        } => {
            let mut args = BTreeMap::new();
            let cols: Vec<String> = items.iter().map(projection_name).collect();
            args.insert("columns".into(), CypherValue::String(cols.join(", ")));
            if *distinct {
                args.insert("distinct".into(), CypherValue::Bool(true));
            }
            ("Projection", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Sort { input, .. } => {
            ("Sort", BTreeMap::new(), vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Limit { input, .. } => {
            ("Limit", BTreeMap::new(), vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Skip { input, .. } => {
            ("Skip", BTreeMap::new(), vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Eager { input } => {
            ("Eager", BTreeMap::new(), vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Aggregate {
            input, keys, aggs, ..
        } => {
            let mut args = BTreeMap::new();
            let key_names: Vec<String> = keys.iter().map(projection_name).collect();
            let agg_names: Vec<String> = aggs.iter().map(projection_name).collect();
            args.insert("keys".into(), CypherValue::String(key_names.join(", ")));
            args.insert(
                "aggregations".into(),
                CypherValue::String(agg_names.join(", ")),
            );
            ("EagerAggregation", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Union { left, right, all } => {
            let mut args = BTreeMap::new();
            args.insert("all".into(), CypherValue::Bool(*all));
            (
                "Union",
                args,
                vec![plan_to_cypher_value(left), plan_to_cypher_value(right)],
            )
        }
        LogicalPlan::Unwind { input, alias, .. } => {
            let mut args = BTreeMap::new();
            args.insert("alias".into(), CypherValue::String(alias.clone()));
            ("Unwind", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::LoadCsv {
            input,
            row_alias,
            with_headers,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("row_alias".into(), CypherValue::String(row_alias.clone()));
            args.insert("with_headers".into(), CypherValue::Bool(*with_headers));
            ("LoadCsv", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::CreateNode {
            input,
            variable,
            labels,
            ..
        } => {
            let mut args = BTreeMap::new();
            if let Some(v) = variable {
                args.insert("variable".into(), CypherValue::String(v.clone()));
            }
            if !labels.is_empty() {
                args.insert("labels".into(), CypherValue::String(labels.join(":")));
            }
            ("CreateNode", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::CreateRel {
            input,
            from,
            to,
            edge_type,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            args.insert("type".into(), CypherValue::String(edge_type.clone()));
            (
                "CreateRelationship",
                args,
                vec![plan_to_cypher_value(input)],
            )
        }
        LogicalPlan::SetProperty { input, .. } => (
            "SetProperty",
            BTreeMap::new(),
            vec![plan_to_cypher_value(input)],
        ),
        LogicalPlan::RemoveOp { input, .. } => {
            ("Remove", BTreeMap::new(), vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Delete { input, detach, .. } => {
            let mut args = BTreeMap::new();
            if *detach {
                args.insert("detach".into(), CypherValue::Bool(true));
            }
            ("Delete", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::Merge { input, .. } => {
            ("Merge", BTreeMap::new(), vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::OptionalMatch {
            input, match_plan, ..
        } => (
            "OptionalExpand",
            BTreeMap::new(),
            vec![
                plan_to_cypher_value(input),
                plan_to_cypher_value(match_plan),
            ],
        ),
        LogicalPlan::CrossProduct { left, right } => (
            "CartesianProduct",
            BTreeMap::new(),
            vec![plan_to_cypher_value(left), plan_to_cypher_value(right)],
        ),
        LogicalPlan::HashJoin {
            left,
            right,
            left_keys,
            right_keys,
        } => {
            let mut args = BTreeMap::new();
            args.insert(
                "left_keys".into(),
                CypherValue::String(
                    left_keys
                        .iter()
                        .map(|k| format!("{k:?}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            );
            args.insert(
                "right_keys".into(),
                CypherValue::String(
                    right_keys
                        .iter()
                        .map(|k| format!("{k:?}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            );
            (
                "HashJoin",
                args,
                vec![plan_to_cypher_value(left), plan_to_cypher_value(right)],
            )
        }
        LogicalPlan::BindPath {
            input, variable, ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            ("BindPath", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::BindQppGroups {
            input,
            src_var,
            dst_var,
            ..
        } => {
            let mut args = BTreeMap::new();
            if let Some(v) = src_var {
                args.insert("src".into(), CypherValue::String(v.clone()));
            }
            if let Some(v) = dst_var {
                args.insert("dst".into(), CypherValue::String(v.clone()));
            }
            ("BindQppGroups", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::BindVarLists { input, groups } => {
            let mut args = BTreeMap::new();
            args.insert(
                "vars".into(),
                CypherValue::String(
                    groups
                        .iter()
                        .map(|(v, _)| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            );
            ("BindVarLists", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::BindPathEndpoints {
            input,
            rel_var,
            from_var,
            to_var,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("rel".into(), CypherValue::String(rel_var.clone()));
            args.insert("from".into(), CypherValue::String(from_var.clone()));
            args.insert("to".into(), CypherValue::String(to_var.clone()));
            ("BindPathEndpoints", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::BindEdgeEndpoints {
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        } => {
            let mut args = BTreeMap::new();
            args.insert("edge".into(), CypherValue::String(edge_var.clone()));
            args.insert("from".into(), CypherValue::String(from_var.clone()));
            args.insert("to".into(), CypherValue::String(to_var.clone()));
            args.insert(
                "direction".into(),
                CypherValue::String(format!("{direction:?}")),
            );
            if !edge_types.is_empty() {
                args.insert("types".into(), CypherValue::String(edge_types.join("|")));
            }
            ("BindEdgeEndpoints", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::CallProcedure {
            input,
            procedure_name,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert(
                "procedure".into(),
                CypherValue::String(procedure_name.clone()),
            );
            ("ProcedureCall", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::CallSubquery {
            input,
            imports,
            body,
            return_aliases,
        } => {
            let mut args = BTreeMap::new();
            if let Some(imports) = imports {
                args.insert(
                    "imports".into(),
                    CypherValue::List(imports.iter().cloned().map(CypherValue::String).collect()),
                );
            }
            if !return_aliases.is_empty() {
                args.insert(
                    "yields".into(),
                    CypherValue::List(
                        return_aliases
                            .iter()
                            .cloned()
                            .map(CypherValue::String)
                            .collect(),
                    ),
                );
            }
            (
                "CallSubquery",
                args,
                vec![plan_to_cypher_value(input), plan_to_cypher_value(body)],
            )
        }
        LogicalPlan::CallInTransactions {
            input,
            body,
            report_status,
            ..
        } => {
            let mut args = BTreeMap::new();
            if let Some(rs) = report_status {
                args.insert("reportStatus".into(), CypherValue::String(rs.clone()));
            }
            (
                "CallInTransactions",
                args,
                vec![plan_to_cypher_value(input), plan_to_cypher_value(body)],
            )
        }
        LogicalPlan::ForEach {
            input,
            variable,
            list,
            body,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            args.insert("list".into(), CypherValue::String(format!("{list:?}")));
            (
                "ForEach",
                args,
                vec![plan_to_cypher_value(input), plan_to_cypher_value(body)],
            )
        }
        LogicalPlan::SeededRows(frames) => {
            let mut args = BTreeMap::new();
            args.insert("rows".into(), CypherValue::Integer(frames.len() as i64));
            ("SeededRows", args, vec![])
        }
        LogicalPlan::EmptyRow => ("EmptyRow", BTreeMap::new(), vec![]),
        LogicalPlan::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            ("CreateIndex", args, vec![])
        }
        LogicalPlan::CreateVectorIndex {
            name,
            label,
            property,
            dimensions,
            similarity,
            quantization,
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            if let Some(d) = dimensions {
                args.insert("dimensions".into(), CypherValue::Integer(*d as i64));
            }
            args.insert("similarity".into(), CypherValue::String(similarity.clone()));
            if let Some(q) = quantization {
                args.insert("quantization".into(), CypherValue::Bool(*q));
            }
            ("CreateVectorIndex", args, vec![])
        }
        LogicalPlan::CreateFulltextIndex {
            name,
            labels,
            properties,
            analyzer,
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            args.insert(
                "labels".into(),
                CypherValue::List(labels.iter().cloned().map(CypherValue::String).collect()),
            );
            args.insert(
                "properties".into(),
                CypherValue::List(
                    properties
                        .iter()
                        .cloned()
                        .map(CypherValue::String)
                        .collect(),
                ),
            );
            if let Some(a) = analyzer {
                args.insert("analyzer".into(), CypherValue::String(a.clone()));
            }
            ("CreateFulltextIndex", args, vec![])
        }
        LogicalPlan::CreatePointIndex {
            name,
            label,
            property,
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            ("CreatePointIndex", args, vec![])
        }
        LogicalPlan::CreateTextIndex {
            name,
            label,
            property,
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            args.insert("label".into(), CypherValue::String(label.clone()));
            args.insert("property".into(), CypherValue::String(property.clone()));
            ("CreateTextIndex", args, vec![])
        }
        LogicalPlan::DropIndex { name } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            ("DropIndex", args, vec![])
        }
        LogicalPlan::AnalyzeGraph { labels } => {
            let mut args = BTreeMap::new();
            if !labels.is_empty() {
                args.insert("labels".into(), CypherValue::String(labels.join(", ")));
            }
            ("AnalyzeGraph", args, vec![])
        }
        LogicalPlan::CreateConstraint {
            name,
            label,
            property,
            extra_properties,
            constraint_type,
            is_relationship,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            let key = if *is_relationship {
                "relationshipType"
            } else {
                "label"
            };
            args.insert(key.into(), CypherValue::String(label.clone()));
            let prop_str = if extra_properties.is_empty() {
                property.clone()
            } else {
                let mut all = vec![property.clone()];
                all.extend(extra_properties.iter().cloned());
                all.join(", ")
            };
            args.insert("property".into(), CypherValue::String(prop_str));
            args.insert("type".into(), CypherValue::String(constraint_type.clone()));
            ("CreateConstraint", args, vec![])
        }
        LogicalPlan::DropConstraint { name } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            ("DropConstraint", args, vec![])
        }
        LogicalPlan::CreateEdgeIndex {
            name,
            edge_type,
            property,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            args.insert("edgeType".into(), CypherValue::String(edge_type.clone()));
            if let Some(p) = property {
                args.insert("property".into(), CypherValue::String(p.clone()));
            }
            ("CreateEdgeIndex", args, vec![])
        }
        LogicalPlan::DropEdgeIndex { name } => {
            let mut args = BTreeMap::new();
            args.insert("name".into(), CypherValue::String(name.clone()));
            ("DropEdgeIndex", args, vec![])
        }
        LogicalPlan::ShowIndexes => ("ShowIndexes", BTreeMap::new(), vec![]),
        LogicalPlan::ShowConstraints => ("ShowConstraints", BTreeMap::new(), vec![]),
        LogicalPlan::ShowFunctions => ("ShowFunctions", BTreeMap::new(), vec![]),
        LogicalPlan::ShowProcedures => ("ShowProcedures", BTreeMap::new(), vec![]),
        LogicalPlan::TransactionCommand { input, command, .. } => (
            match command {
                crate::ast::TransactionCommandKind::Show => "ShowTransactions",
                crate::ast::TransactionCommandKind::Terminate => "TerminateTransactions",
            },
            BTreeMap::new(),
            vec![plan_to_cypher_value(input)],
        ),
        LogicalPlan::ShowSettings => ("ShowSettings", BTreeMap::new(), vec![]),
        LogicalPlan::ShortestPath {
            input,
            from,
            to,
            all,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            args.insert("all".into(), CypherValue::Bool(*all));
            (
                if *all {
                    "AllShortestPaths"
                } else {
                    "ShortestPath"
                },
                args,
                vec![plan_to_cypher_value(input)],
            )
        }
        LogicalPlan::WeightedShortestPath {
            input,
            from,
            to,
            weight_property,
            ..
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            args.insert(
                "weight".into(),
                CypherValue::String(weight_property.clone()),
            );
            (
                "WeightedShortestPath",
                args,
                vec![plan_to_cypher_value(input)],
            )
        }
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
        } => {
            let mut args = BTreeMap::new();
            args.insert("from".into(), CypherValue::String(from.clone()));
            args.insert("to".into(), CypherValue::String(to.clone()));
            args.insert("min_hops".into(), CypherValue::Integer(*min_hops as i64));
            args.insert("max_hops".into(), CypherValue::Integer(*max_hops as i64));
            args.insert(
                "direction".into(),
                CypherValue::String(format!("{direction:?}")),
            );
            if !edge_types.is_empty() {
                args.insert("types".into(), CypherValue::String(edge_types.join("|")));
            }
            if let Some(lbl) = target_label {
                args.insert("target_label".into(), CypherValue::String(lbl.clone()));
            }
            if *global_dedup {
                args.insert("global_dedup".into(), CypherValue::String("true".into()));
            }
            if target_filter.is_some() {
                args.insert("target_filter".into(), CypherValue::String("true".into()));
            }
            ("DistinctExpand", args, vec![plan_to_cypher_value(input)])
        }
        LogicalPlan::ScanFilterProject {
            variable,
            label,
            predicate,
            items,
            distinct,
            limit,
        } => {
            let mut args = BTreeMap::new();
            args.insert("variable".into(), CypherValue::String(variable.clone()));
            if let Some(lbl) = label {
                args.insert("label".into(), CypherValue::String(lbl.clone()));
            }
            let cols: Vec<String> = items.iter().map(projection_name).collect();
            args.insert("columns".into(), CypherValue::String(cols.join(", ")));
            if *distinct {
                args.insert("distinct".into(), CypherValue::Bool(true));
            }
            if predicate.is_some() {
                args.insert("has_filter".into(), CypherValue::Bool(true));
            }
            if let Some(lim) = limit {
                args.insert("limit".into(), CypherValue::Integer(*lim as i64));
            }
            ("ScanFilterProject", args, vec![])
        }
    };

    map.insert(
        "operatorType".into(),
        CypherValue::String(operator_type.into()),
    );
    map.insert("args".into(), CypherValue::Map(args));
    map.insert("children".into(), CypherValue::List(children));

    CypherValue::Map(map)
}
