//! Cardinality estimates for logical plan operators.

use traverse_core::tx::view::GraphView;

use crate::ast::*;
use crate::planner::{LogicalPlan, SpatialQuery};

/// Default selectivities.
const SEL_EQ: f64 = 0.01;
const SEL_RANGE: f64 = 0.33;
const SEL_IS_NULL: f64 = 0.05;
const SEL_IS_NOT_NULL: f64 = 0.95;
const SEL_STRING_OP: f64 = 0.15;
const SEL_IN: f64 = 0.05;
const SEL_DEFAULT: f64 = 0.5;

/// Estimated output cardinality of a plan node.
#[recursive::recursive]
pub fn estimate_cardinality(plan: &LogicalPlan, graph: &dyn GraphView) -> f64 {
    match plan {
        LogicalPlan::EmptyRow | LogicalPlan::ContextSeed => 1.0,

        LogicalPlan::IndexLookupJoin { input, .. } => {
            // one seek per input row, ~unique-key selectivity
            estimate_cardinality(input, graph).max(1.0)
        }

        LogicalPlan::Scan { label, .. } => match label {
            None => graph.node_count().max(1) as f64,
            Some(l) => {
                if let Some(stats) = graph.statistics()
                    && let Some(ls) = stats.label_stats.get(l)
                {
                    return ls.count.max(1) as f64;
                }
                let count = graph.nodes_by_label(l).len();
                count.max(1) as f64
            }
        },

        LogicalPlan::NodeById { .. } => 1.0,

        LogicalPlan::NodeByIdRange {
            lower,
            upper,
            label,
            ..
        } => {
            // bounds resolve at run time; at plan time read a literal integer, else assume half the
            // node count (parameters usually point at small id ranges)
            let extract_literal = |e: &Option<Expr>| -> Option<u64> {
                if let Some(Expr::Literal(Literal::Integer(n))) = e {
                    return Some(if *n < 0 { 0 } else { *n as u64 });
                }
                None
            };
            let nc = graph.node_count().max(1) as f64;
            let lo = extract_literal(lower).map(|v| v as f64).unwrap_or(0.0);
            let hi = extract_literal(upper)
                .map(|v| v as f64)
                .unwrap_or_else(|| if upper.is_some() { nc * 0.5 } else { nc });
            let range = (hi - lo).max(1.0);
            let label_sel = match label {
                Some(l) => {
                    if let Some(stats) = graph.statistics() {
                        if let Some(ls) = stats.label_stats.get(l) {
                            (ls.count as f64 / nc).clamp(0.0, 1.0)
                        } else {
                            1.0
                        }
                    } else {
                        1.0
                    }
                }
                None => 1.0,
            };
            (range * label_sel).max(1.0)
        }

        LogicalPlan::EdgeTypeScan {
            edge_types,
            direction,
            ..
        } => {
            use traverse_core::graph::types::Direction;
            // exact per-type count from the `edges_by_type` index sizes (cheap)
            let mut count = 0usize;
            for ty in edge_types {
                count += graph.edges_by_type(ty).len();
            }
            // UNDIRECTED emits each edge twice
            let multiplier = if matches!(direction, Direction::Both) {
                2
            } else {
                1
            };
            ((count * multiplier).max(1)) as f64
        }

        LogicalPlan::IndexScan {
            label, property, ..
        } => {
            if let Some(stats) = graph.statistics()
                && let Some(ls) = stats.label_stats.get(label)
            {
                let key = format!("{label}.{property}");
                if let Some(ps) = stats.property_stats.get(&key)
                    && ps.distinct_count > 0
                {
                    return (ls.count as f64 / ps.distinct_count as f64).max(1.0);
                }
                return (ls.count as f64 * SEL_EQ).max(1.0);
            }
            // fallback: index scans are usually selective
            (graph.node_count() as f64 * SEL_EQ).max(1.0)
        }

        LogicalPlan::PointIndexScan { label, query, .. } => {
            // spatial predicates are selective; kNN returns exactly `k` rows, radius/bbox a small
            // fraction of the label
            let label_count = graph
                .statistics()
                .and_then(|s| s.label_stats.get(label).map(|ls| ls.count as f64))
                .unwrap_or_else(|| graph.node_count() as f64);
            match query {
                SpatialQuery::Knn { .. } => 10.0_f64.min(label_count).max(1.0),
                _ => (label_count * SEL_EQ).max(1.0),
            }
        }

        LogicalPlan::TextIndexScan { label, .. } => {
            // trigram candidates for a string predicate are selective
            let label_count = graph
                .statistics()
                .and_then(|s| s.label_stats.get(label).map(|ls| ls.count as f64))
                .unwrap_or_else(|| graph.node_count() as f64);
            (label_count * SEL_STRING_OP).max(1.0)
        }

        LogicalPlan::IndexRangeScan { label, .. } => {
            if let Some(stats) = graph.statistics()
                && let Some(ls) = stats.label_stats.get(label)
            {
                return (ls.count as f64 * SEL_RANGE).max(1.0);
            }
            (graph.node_count() as f64 * SEL_RANGE).max(1.0)
        }

        LogicalPlan::ExpandOut {
            input, edge_types, ..
        }
        | LogicalPlan::ExpandIn {
            input, edge_types, ..
        }
        | LogicalPlan::ExpandBoth {
            input, edge_types, ..
        } => {
            let input_card = estimate_cardinality(input, graph);
            if let Some(stats) = graph.statistics() {
                if let Some(et) = edge_types.first()
                    && let Some(es) = stats.edge_type_stats.get(et)
                {
                    let distinct = match plan {
                        LogicalPlan::ExpandOut { .. } => es.distinct_sources.max(1),
                        LogicalPlan::ExpandIn { .. } => es.distinct_targets.max(1),
                        _ => es.distinct_sources.max(1).min(es.distinct_targets.max(1)),
                    };
                    return input_card * (es.count as f64 / distinct as f64);
                }
                // untyped expand: average degree
                let avg_degree = if stats.total_nodes > 0 {
                    stats.total_edges as f64 / stats.total_nodes as f64
                } else {
                    1.0
                };
                return input_card * avg_degree;
            }
            // fallback: moderate fan-out
            input_card * 2.0
        }

        LogicalPlan::Filter { input, predicate } => {
            let input_card = estimate_cardinality(input, graph);
            let sel = estimate_selectivity(predicate, graph);
            (input_card * sel).max(1.0)
        }

        LogicalPlan::CrossProduct { left, right } => {
            let left_card = estimate_cardinality(left, graph);
            let right_card = estimate_cardinality(right, graph);
            left_card * right_card
        }

        LogicalPlan::HashJoin { left, right, .. } => {
            let left_card = estimate_cardinality(left, graph);
            let right_card = estimate_cardinality(right, graph);
            // equi-join: roughly min(left, right) for selective joins
            (left_card * right_card * SEL_EQ).max(left_card.min(right_card))
        }

        LogicalPlan::Limit { input, count } => {
            let input_card = estimate_cardinality(input, graph);
            if let Expr::Literal(Literal::Integer(n)) = count {
                input_card.min(*n as f64)
            } else {
                input_card
            }
        }

        LogicalPlan::VectorSearchScan { input, limit, .. } => {
            // up to k rows per input row
            let input_card = estimate_cardinality(input, graph).max(1.0);
            let k = if let Expr::Literal(Literal::Integer(n)) = limit {
                *n as f64
            } else {
                10.0
            };
            (input_card * k).max(1.0)
        }

        LogicalPlan::Eager { input } => estimate_cardinality(input, graph),

        LogicalPlan::Skip { input, count } => {
            let input_card = estimate_cardinality(input, graph);
            if let Expr::Literal(Literal::Integer(n)) = count {
                (input_card - *n as f64).max(0.0)
            } else {
                input_card
            }
        }

        LogicalPlan::Aggregate { input, .. } => {
            let input_card = estimate_cardinality(input, graph);
            (input_card * 0.1).max(1.0)
        }

        LogicalPlan::Project {
            input, distinct, ..
        } => {
            let input_card = estimate_cardinality(input, graph);
            if *distinct {
                (input_card * 0.9).max(1.0)
            } else {
                input_card
            }
        }

        LogicalPlan::Sort { input, .. } => estimate_cardinality(input, graph),

        LogicalPlan::Union { left, right, all } => {
            let left_card = estimate_cardinality(left, graph);
            let right_card = estimate_cardinality(right, graph);
            if *all {
                left_card + right_card
            } else {
                // UNION dedups
                (left_card + right_card) * 0.8
            }
        }

        LogicalPlan::Unwind { input, .. } => {
            // assume ~5 list elements
            estimate_cardinality(input, graph) * 5.0
        }

        LogicalPlan::LoadCsv { input, .. } => {
            // assume ~100 CSV rows
            estimate_cardinality(input, graph) * 100.0
        }

        LogicalPlan::OptionalMatch {
            input, match_plan, ..
        } => {
            let input_card = estimate_cardinality(input, graph);
            let match_card = estimate_cardinality(match_plan, graph);
            // at least input_card rows (left join), maybe more
            input_card.max(match_card)
        }

        LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindQppGroups { input, .. }
        | LogicalPlan::BindVarLists { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. }
        | LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. }
        | LogicalPlan::CallProcedure { input, .. } => estimate_cardinality(input, graph),

        LogicalPlan::CallSubquery { input, body, .. }
        | LogicalPlan::CallInTransactions { input, body, .. } => {
            // CALL { body } is a per-row cross product: each outer row yields `body.cardinality`
            // rows
            estimate_cardinality(input, graph) * estimate_cardinality(body, graph)
        }

        LogicalPlan::CreateNode { input, .. }
        | LogicalPlan::CreateRel { input, .. }
        | LogicalPlan::Merge { input, .. } => estimate_cardinality(input, graph),

        // FOREACH keeps the input cardinality (body output discarded)
        LogicalPlan::ForEach { input, .. } => estimate_cardinality(input, graph),

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
        | LogicalPlan::ShowSettings => 0.0,
        LogicalPlan::TransactionCommand { input, .. } => estimate_cardinality(input, graph),

        LogicalPlan::ShortestPath { input, .. }
        | LogicalPlan::WeightedShortestPath { input, .. } => {
            // each input row yields 0 or 1 path
            estimate_cardinality(input, graph)
        }

        LogicalPlan::DistinctExpand { input, .. } => {
            // each input row yields some distinct targets
            estimate_cardinality(input, graph) * 10.0
        }

        LogicalPlan::ScanFilterProject {
            label,
            predicate,
            distinct,
            limit,
            ..
        } => {
            let scan_card = match label {
                None => graph.node_count().max(1) as f64,
                Some(l) => {
                    if let Some(stats) = graph.statistics() {
                        if let Some(ls) = stats.label_stats.get(l) {
                            ls.count.max(1) as f64
                        } else {
                            let count = graph.nodes_by_label(l).len();
                            count.max(1) as f64
                        }
                    } else {
                        let count = graph.nodes_by_label(l).len();
                        count.max(1) as f64
                    }
                }
            };
            // filter selectivity
            let filtered = if let Some(pred) = predicate {
                (scan_card * estimate_selectivity(pred, graph)).max(1.0)
            } else {
                scan_card
            };
            // distinct factor
            let card = if *distinct {
                (filtered * 0.9).max(1.0)
            } else {
                filtered
            };
            // cap at the absorbed limit
            if let Some(lim) = limit {
                card.min(*lim as f64)
            } else {
                card
            }
        }
    }
}

/// Estimated predicate selectivity (fraction of rows that pass).
#[recursive::recursive]
pub(crate) fn estimate_selectivity(expr: &Expr, graph: &dyn GraphView) -> f64 {
    match expr {
        Expr::BinaryOp(_, BinOp::And, _) => {
            // all ANDed conjuncts
            let mut conjuncts = Vec::new();
            collect_and_conjuncts(expr, &mut conjuncts);
            conjuncts
                .iter()
                .map(|c| estimate_selectivity(c, graph))
                .product()
        }

        Expr::BinaryOp(left, BinOp::Or, right) => {
            let sl = estimate_selectivity(left, graph);
            let sr = estimate_selectivity(right, graph);
            sl + sr - sl * sr
        }

        Expr::Comparison(_, pairs) => {
            if pairs.len() == 1 {
                match pairs[0].0 {
                    CmpOp::Eq => estimate_eq_selectivity(&pairs[0].1, graph),
                    CmpOp::Neq => 1.0 - SEL_EQ,
                    CmpOp::Lt | CmpOp::Lte | CmpOp::Gt | CmpOp::Gte => SEL_RANGE,
                }
            } else {
                // chained comparison: selectivities compound
                pairs
                    .iter()
                    .map(|(op, _)| match op {
                        CmpOp::Eq => SEL_EQ,
                        CmpOp::Neq => 1.0 - SEL_EQ,
                        CmpOp::Lt | CmpOp::Lte | CmpOp::Gt | CmpOp::Gte => SEL_RANGE,
                    })
                    .product()
            }
        }

        Expr::IsNull(_) => SEL_IS_NULL,
        Expr::IsNotNull(_) => SEL_IS_NOT_NULL,

        Expr::StringOp(_, _, _) => SEL_STRING_OP,
        Expr::In(_, _) => SEL_IN,

        Expr::UnaryOp(UnaryOp::Not, inner) => 1.0 - estimate_selectivity(inner, graph),

        // existential subquery / pattern predicate: moderate selectivity
        Expr::ExistentialSubquery(_)
        | Expr::FullExistentialSubquery(_)
        | Expr::PatternPredicate(_) => 0.3,

        _ => SEL_DEFAULT,
    }
}

/// Equality selectivity; currently the default, not the property histograms.
fn estimate_eq_selectivity(value_expr: &Expr, graph: &dyn GraphView) -> f64 {
    let _ = (value_expr, graph);
    SEL_EQ
}

/// Collects the conjuncts of nested ANDs.
#[recursive::recursive]
fn collect_and_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            collect_and_conjuncts(left, out);
            collect_and_conjuncts(right, out);
        }
        other => out.push(other),
    }
}
