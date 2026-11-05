//! Physical plans as values for EXPLAIN and PROFILE: nested maps of `operatorType`, `args` and
//! `children`, with the planner's choices (access path, flattening, groups) as arguments.

use std::collections::BTreeMap;

use crate::types::CypherValue;

use super::super::chunk::DataPos;
use super::{CountKind, PhysNode, PhysicalPlan, SchemaOp, ShowOp};

/// The plan as a nested map.
pub fn plan_to_value(plan: &PhysicalPlan) -> CypherValue {
    node_to_value(&plan.root)
}

fn s(v: impl Into<String>) -> CypherValue {
    CypherValue::String(v.into())
}

fn pos(p: DataPos) -> CypherValue {
    s(format!("g{}.{}", p.group, p.col))
}

fn list_s(items: &[String]) -> CypherValue {
    CypherValue::List(items.iter().map(|i| s(i.clone())).collect())
}

/// Operator name, arguments and children of one node.
#[recursive::recursive]
pub fn node_to_value(node: &PhysNode) -> CypherValue {
    if let PhysNode::Capture { input, .. } = node {
        return node_to_value(input);
    }
    let mut args: BTreeMap<String, CypherValue> = BTreeMap::new();
    let children: Vec<&PhysNode> = node.children();
    let name = match node {
        PhysNode::Capture { .. } => unreachable!("capture is transparent"),
        PhysNode::EmptyRow { .. } => "EmptyRow",
        PhysNode::Seed { .. } => "Seed",
        PhysNode::Nest { .. } => "Apply",
        PhysNode::NodeScan {
            out,
            seek: Some(seek),
            ..
        } => {
            args.insert("variable".into(), pos(*out));
            args.insert("property".into(), s(seek.property.clone()));
            "NodeIndexSeekAnyLabel"
        }
        PhysNode::NodeScan { out, label, .. } => {
            args.insert("variable".into(), pos(*out));
            if let Some(l) = label {
                args.insert("label".into(), s(l.clone()));
                "NodeByLabelScan"
            } else {
                "AllNodesScan"
            }
        }
        PhysNode::NodeByIdSeek { out, label, .. } => {
            args.insert("variable".into(), pos(*out));
            if let Some(l) = label {
                args.insert("label".into(), s(l.clone()));
            }
            "NodeByIdSeek"
        }
        PhysNode::NodeByIdRange { out, label, .. } => {
            args.insert("variable".into(), pos(*out));
            if let Some(l) = label {
                args.insert("label".into(), s(l.clone()));
            }
            "NodeByIdRangeScan"
        }
        PhysNode::IndexSeek {
            out,
            label,
            property,
            ..
        } => {
            args.insert("variable".into(), pos(*out));
            args.insert("label".into(), s(label.clone()));
            args.insert("property".into(), s(property.clone()));
            "NodeIndexScan"
        }
        PhysNode::IndexRange {
            out,
            label,
            property,
            op,
            ..
        } => {
            args.insert("variable".into(), pos(*out));
            args.insert("label".into(), s(label.clone()));
            args.insert("property".into(), s(property.clone()));
            args.insert("op".into(), s(format!("{op:?}")));
            args.insert("access".into(), s("index range, label scan when broad"));
            "NodeIndexRangeScan"
        }
        PhysNode::TextIndexScan {
            out,
            label,
            property,
            kind,
            ..
        } => {
            args.insert("variable".into(), pos(*out));
            args.insert("label".into(), s(label.clone()));
            args.insert("property".into(), s(property.clone()));
            args.insert("predicate".into(), s(format!("{kind:?}")));
            "NodeTextIndexScan"
        }
        PhysNode::PointIndexScan {
            out,
            label,
            property,
            query,
        } => {
            args.insert("variable".into(), pos(*out));
            args.insert("label".into(), s(label.clone()));
            args.insert("property".into(), s(property.clone()));
            let kind = match query {
                super::PhysSpatialQuery::Distance { .. } => "distance",
                super::PhysSpatialQuery::BBox { .. } => "withinBBox",
                super::PhysSpatialQuery::Knn { .. } => "knn",
            };
            args.insert("query".into(), s(kind));
            "NodePointIndexScan"
        }
        PhysNode::EdgeTypeScan {
            from,
            rel,
            to,
            edge_types,
            direction,
        } => {
            args.insert("from".into(), pos(*from));
            if let Some(r) = rel {
                args.insert("rel".into(), pos(*r));
            }
            args.insert("to".into(), pos(*to));
            args.insert("edge_types".into(), s(edge_types.join("|")));
            args.insert("direction".into(), s(format!("{direction:?}")));
            "EdgeTypeScan"
        }
        PhysNode::CountSource { kind, .. } => {
            match kind {
                CountKind::Nodes { label } => {
                    args.insert("count".into(), s("nodes"));
                    if let Some(l) = label {
                        args.insert("label".into(), s(l.clone()));
                    }
                }
                CountKind::Edges {
                    edge_types,
                    direction,
                    from_label,
                    to_label,
                } => {
                    args.insert("count".into(), s("relationships"));
                    args.insert("edge_types".into(), s(edge_types.join("|")));
                    args.insert("direction".into(), s(format!("{direction:?}")));
                    if let Some(l) = from_label {
                        args.insert("from_label".into(), s(l.clone()));
                    }
                    if let Some(l) = to_label {
                        args.insert("to_label".into(), s(l.clone()));
                    }
                }
                CountKind::IndexRange {
                    label,
                    property,
                    op,
                    ..
                } => {
                    args.insert("count".into(), s("index range"));
                    args.insert("label".into(), s(label.clone()));
                    args.insert("property".into(), s(property.clone()));
                    args.insert("op".into(), s(format!("{op:?}")));
                }
                CountKind::IndexEq {
                    label, property, ..
                } => {
                    args.insert("count".into(), s("index lookup"));
                    args.insert("label".into(), s(label.clone()));
                    args.insert("property".into(), s(property.clone()));
                }
                CountKind::DegreeSum {
                    label,
                    property,
                    op,
                    dir,
                    ..
                } => {
                    args.insert("count".into(), s("filtered degree sum"));
                    args.insert("label".into(), s(label.clone()));
                    args.insert("property".into(), s(property.clone()));
                    args.insert("op".into(), s(format!("{op:?}")));
                    args.insert("direction".into(), s(format!("{dir:?}")));
                }
                CountKind::ColumnRange {
                    label,
                    property,
                    op,
                    ..
                } => {
                    args.insert("count".into(), s("column range"));
                    args.insert("label".into(), s(label.clone()));
                    args.insert("property".into(), s(property.clone()));
                    args.insert("op".into(), s(format!("{op:?}")));
                }
            }
            "CountFromStore"
        }
        PhysNode::Flatten { group, .. } => {
            args.insert("group".into(), CypherValue::Integer(*group as i64));
            "Flatten"
        }
        PhysNode::Filter { pred, group, .. } => {
            args.insert("predicate".into(), s(format!("{:?}", pred.source)));
            if let Some(g) = group {
                args.insert("group".into(), CypherValue::Integer(*g as i64));
            }
            "Filter"
        }
        PhysNode::AddColumn { expr, out, .. } => {
            args.insert("expression".into(), s(format!("{:?}", expr.source)));
            args.insert("out".into(), pos(*out));
            "Projection"
        }
        PhysNode::Project {
            items,
            distinct,
            where_,
            in_groups,
            ..
        } => {
            args.insert(
                "items".into(),
                CypherValue::List(
                    items
                        .iter()
                        .map(|(e, _)| s(format!("{:?}", e.source)))
                        .collect(),
                ),
            );
            if *distinct {
                args.insert("distinct".into(), CypherValue::Bool(true));
            }
            if where_.is_some() {
                args.insert("where".into(), CypherValue::Bool(true));
            }
            args.insert(
                "groups".into(),
                CypherValue::Integer(in_groups.len() as i64),
            );
            "Projection"
        }
        PhysNode::ChainWeights {
            out,
            weight,
            from_label,
            hops,
            count_only,
            ..
        } => {
            args.insert("out".into(), pos(*out));
            args.insert("weight".into(), pos(*weight));
            if let Some(l) = from_label {
                args.insert("from_label".into(), s(l.clone()));
            }
            args.insert(
                "hops".into(),
                CypherValue::List(
                    hops.iter()
                        .map(|h| {
                            s(format!(
                                "{:?} {} {}",
                                h.dir,
                                h.edge_types.join("|"),
                                h.to_label.as_deref().unwrap_or("")
                            ))
                        })
                        .collect(),
                ),
            );
            args.insert("count_only".into(), CypherValue::Bool(*count_only));
            "ChainWeights"
        }
        PhysNode::ExpandCount {
            from,
            out,
            dir,
            edge_types,
            to_label,
            prior_edges,
            ..
        } => {
            args.insert("from".into(), pos(*from));
            args.insert("out".into(), pos(*out));
            args.insert("direction".into(), s(format!("{dir:?}")));
            if !edge_types.is_empty() {
                args.insert("edge_types".into(), s(edge_types.join("|")));
            }
            if let Some(l) = to_label {
                args.insert("to_label".into(), s(l.clone()));
            }
            if !prior_edges.is_empty() {
                args.insert(
                    "prior_edges".into(),
                    CypherValue::List(prior_edges.iter().map(|p| pos(*p)).collect()),
                );
            }
            "ExpandCount"
        }
        PhysNode::Expand {
            from,
            rel,
            to,
            dir,
            edge_types,
            bound_to,
            in_place,
            ..
        } => {
            args.insert("from".into(), pos(*from));
            args.insert("rel".into(), pos(*rel));
            args.insert("to".into(), pos(*to));
            args.insert("direction".into(), s(format!("{dir:?}")));
            if *in_place {
                args.insert("in_place".into(), CypherValue::Bool(true));
            }
            if !edge_types.is_empty() {
                args.insert("edge_types".into(), s(edge_types.join("|")));
            }
            if bound_to.is_some() {
                "ExpandInto"
            } else {
                "Expand"
            }
        }
        PhysNode::VarExpand {
            from,
            to,
            dir,
            edge_types,
            min,
            max,
            ..
        } => {
            args.insert("from".into(), pos(*from));
            args.insert("to".into(), pos(*to));
            args.insert("direction".into(), s(format!("{dir:?}")));
            if !edge_types.is_empty() {
                args.insert("edge_types".into(), s(edge_types.join("|")));
            }
            args.insert("range".into(), s(format!("{min}..{max}")));
            "VarLengthExpand"
        }
        PhysNode::DistinctExpand {
            min,
            max,
            dir,
            edge_types,
            target_label,
            multi_source,
            ..
        } => {
            args.insert("direction".into(), s(format!("{dir:?}")));
            if multi_source.is_some() {
                args.insert("multi_source".into(), CypherValue::Bool(true));
            }
            if !edge_types.is_empty() {
                args.insert("edge_types".into(), s(edge_types.join("|")));
            }
            args.insert("range".into(), s(format!("{min}..{max}")));
            if let Some(l) = target_label {
                args.insert("target_label".into(), s(l.clone()));
            }
            "DistinctExpand"
        }
        PhysNode::Unwind { list, out, .. } => {
            args.insert("expression".into(), s(format!("{:?}", list.source)));
            args.insert("out".into(), pos(*out));
            "Unwind"
        }
        PhysNode::IndexLookupJoin {
            out,
            label,
            property,
            ..
        } => {
            args.insert("variable".into(), pos(*out));
            args.insert("label".into(), s(label.clone()));
            args.insert("property".into(), s(property.clone()));
            "NodeIndexLookupJoin"
        }
        PhysNode::VectorSearch {
            index_name,
            index_where,
            score,
            ..
        } => {
            args.insert("index".into(), s(index_name.clone()));
            if index_where.is_some() {
                args.insert("filtered".into(), CypherValue::Bool(true));
            }
            if let Some(sc) = score {
                args.insert("score".into(), pos(*sc));
            }
            "VectorSearch"
        }
        PhysNode::Limit {
            count,
            materialize,
            consume,
            ..
        } => {
            args.insert("count".into(), s(format!("{:?}", count.source)));
            if *materialize {
                args.insert("materialize".into(), CypherValue::Bool(true));
            }
            if consume.is_some() {
                args.insert("consume".into(), CypherValue::Bool(true));
            }
            "Limit"
        }
        PhysNode::Skip { count, .. } => {
            args.insert("count".into(), s(format!("{:?}", count.source)));
            "Skip"
        }
        PhysNode::Sort { keys, .. } => {
            args.insert(
                "keys".into(),
                CypherValue::List(
                    keys.iter()
                        .map(|(e, asc)| {
                            s(format!(
                                "{:?} {}",
                                e.source,
                                if *asc { "ASC" } else { "DESC" }
                            ))
                        })
                        .collect(),
                ),
            );
            "Sort"
        }
        PhysNode::Aggregate { keys, aggs, .. } => {
            args.insert(
                "keys".into(),
                CypherValue::List(
                    keys.iter()
                        .map(|(e, _)| s(format!("{:?}", e.source)))
                        .collect(),
                ),
            );
            args.insert(
                "aggregates".into(),
                CypherValue::List(aggs.iter().map(|a| s(a.func.clone())).collect()),
            );
            "EagerAggregation"
        }
        PhysNode::Eager { .. } => "Eager",
        PhysNode::Union { all, .. } => {
            if *all {
                args.insert("all".into(), CypherValue::Bool(true));
            }
            "Union"
        }
        PhysNode::CrossProduct { .. } => "CartesianProduct",
        PhysNode::HashJoin { left_keys, .. } => {
            args.insert("keys".into(), CypherValue::Integer(left_keys.len() as i64));
            "HashJoin"
        }
        PhysNode::OptionalMatch { body, .. } => {
            args.insert("match".into(), node_to_value(&body.root));
            "OptionalExpand"
        }
        PhysNode::BindPath { out, .. } => {
            args.insert("out".into(), pos(*out));
            "BindPath"
        }
        PhysNode::BindQppGroups { .. } => "BindQppGroups",
        PhysNode::BindVarLists { .. } => "BindVarLists",
        PhysNode::BindPathEndpoints { .. } => "BindPathEndpoints",
        PhysNode::BindEdgeEndpoints { .. } => "BindEdgeEndpoints",
        PhysNode::CallProcedure { name, .. } => {
            args.insert("procedure".into(), s(name.clone()));
            "ProcedureCall"
        }
        PhysNode::CallSubquery { body, .. } => {
            args.insert("body".into(), node_to_value(&body.root));
            "CallSubquery"
        }
        PhysNode::CallInTransactions { body, on_error, .. } => {
            args.insert("body".into(), node_to_value(&body.root));
            args.insert("on_error".into(), s(format!("{on_error:?}")));
            "CallInTransactions"
        }
        PhysNode::ForEach { body, .. } => {
            args.insert("body".into(), node_to_value(&body.root));
            "ForEach"
        }
        PhysNode::ShortestPath {
            all,
            k,
            min_depth,
            max_depth,
            ..
        } => {
            args.insert("range".into(), s(format!("{min_depth}..{max_depth}")));
            if let Some(k) = k {
                args.insert("k".into(), CypherValue::Integer(*k as i64));
            }
            if *all {
                "AllShortestPaths"
            } else {
                "ShortestPath"
            }
        }
        PhysNode::WeightedShortestPath {
            weight_property, ..
        } => {
            args.insert("weight".into(), s(weight_property.clone()));
            "WeightedShortestPath"
        }
        PhysNode::LoadCsv { with_headers, .. } => {
            args.insert("headers".into(), CypherValue::Bool(*with_headers));
            "LoadCsv"
        }
        PhysNode::CreateNode { labels, .. } => {
            args.insert("labels".into(), list_s(labels));
            "CreateNode"
        }
        PhysNode::CreateRel { edge_type, .. } => {
            args.insert("type".into(), s(edge_type.clone()));
            "CreateRelationship"
        }
        PhysNode::SetProps { items, .. } => {
            args.insert("items".into(), CypherValue::Integer(items.len() as i64));
            "SetProperty"
        }
        PhysNode::Remove { items, .. } => {
            args.insert("items".into(), CypherValue::Integer(items.len() as i64));
            "Remove"
        }
        PhysNode::Delete { detach, .. } => {
            if *detach {
                args.insert("detach".into(), CypherValue::Bool(true));
            }
            "Delete"
        }
        PhysNode::Merge { .. } => "Merge",
        PhysNode::Schema { op, .. } => match op {
            SchemaOp::CreateIndex { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreateIndex"
            }
            SchemaOp::CreateVectorIndex { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreateVectorIndex"
            }
            SchemaOp::CreateFulltextIndex { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreateFulltextIndex"
            }
            SchemaOp::CreatePointIndex { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreatePointIndex"
            }
            SchemaOp::CreateTextIndex { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreateTextIndex"
            }
            SchemaOp::DropIndex { name } => {
                args.insert("name".into(), s(name.clone()));
                "DropIndex"
            }
            SchemaOp::AnalyzeGraph { labels } => {
                args.insert("labels".into(), list_s(labels));
                "AnalyzeGraph"
            }
            SchemaOp::CreateConstraint { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreateConstraint"
            }
            SchemaOp::DropConstraint { name } => {
                args.insert("name".into(), s(name.clone()));
                "DropConstraint"
            }
            SchemaOp::CreateEdgeIndex { name, .. } => {
                args.insert("name".into(), s(name.clone()));
                "CreateEdgeIndex"
            }
            SchemaOp::DropEdgeIndex { name } => {
                args.insert("name".into(), s(name.clone()));
                "DropEdgeIndex"
            }
        },
        PhysNode::Show { op, .. } => match op {
            ShowOp::Indexes => "ShowIndexes",
            ShowOp::Constraints => "ShowConstraints",
            ShowOp::Functions => "ShowFunctions",
            ShowOp::Procedures => "ShowProcedures",
            ShowOp::Settings => "ShowSettings",
        },
        PhysNode::TransactionCommand { command, .. } => match command {
            crate::ast::TransactionCommandKind::Show => "ShowTransactions",
            crate::ast::TransactionCommandKind::Terminate => "TerminateTransactions",
        },
        PhysNode::NoRows => "NoRows",
    };
    let mut map = BTreeMap::new();
    map.insert("operatorType".to_string(), s(name));
    map.insert("args".to_string(), CypherValue::Map(args));
    map.insert(
        "children".to_string(),
        CypherValue::List(children.into_iter().map(node_to_value).collect()),
    );
    CypherValue::Map(map)
}
