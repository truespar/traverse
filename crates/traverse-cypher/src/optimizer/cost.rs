//! Cost model for logical plan operators. In-memory graph, so cost ~ rows touched.

use traverse_core::tx::view::GraphView;

use super::cardinality::estimate_cardinality;
use crate::ast::Expr;
use crate::planner::LogicalPlan;

/// Per-row cost constants.
const SCAN_PER_ROW: f64 = 1.0;
const INDEX_SCAN_STARTUP: f64 = 2.0;
const INDEX_SCAN_PER_ROW: f64 = 0.5;
const FILTER_PER_ROW: f64 = 0.2;
const EXPAND_PER_ROW: f64 = 2.0;
const CROSS_PRODUCT_PER_ROW: f64 = 1.0;
const SORT_LOG_FACTOR: f64 = 1.5;
const PROJECT_PER_ROW: f64 = 0.1;
const AGGREGATE_PER_ROW: f64 = 0.5;
const HASH_JOIN_BUILD_PER_ROW: f64 = 1.5;
const HASH_JOIN_PROBE_PER_ROW: f64 = 0.5;

/// `(cardinality, cost)` of a plan node.
#[recursive::recursive]
pub fn estimate_cost(plan: &LogicalPlan, graph: &dyn GraphView) -> (f64, f64) {
    match plan {
        LogicalPlan::EmptyRow | LogicalPlan::ContextSeed => (1.0, 0.0),

        LogicalPlan::IndexLookupJoin { input, .. } => {
            let (in_card, in_cost) = estimate_cost(input, graph);
            let card = in_card.max(1.0);
            (
                card,
                in_cost + in_card * (INDEX_SCAN_STARTUP + INDEX_SCAN_PER_ROW),
            )
        }

        LogicalPlan::Scan { .. } => {
            let card = estimate_cardinality(plan, graph);
            (card, card * SCAN_PER_ROW)
        }

        LogicalPlan::IndexScan { .. }
        | LogicalPlan::IndexRangeScan { .. }
        | LogicalPlan::PointIndexScan { .. }
        | LogicalPlan::TextIndexScan { .. } => {
            let card = estimate_cardinality(plan, graph);
            (card, INDEX_SCAN_STARTUP + card * INDEX_SCAN_PER_ROW)
        }

        LogicalPlan::NodeById { .. } => {
            // O(1) direct fetch, at most one row; cheaper than an index scan (no probe + filter)
            (1.0, 1.0)
        }

        LogicalPlan::NodeByIdRange { .. } => {
            let card = estimate_cardinality(plan, graph);
            (card, card * SCAN_PER_ROW)
        }

        LogicalPlan::EdgeTypeScan { .. } => {
            // cardinality = edges of the matched types
            let card = estimate_cardinality(plan, graph);
            (card, card * SCAN_PER_ROW)
        }

        LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. } => {
            let (_, input_cost) = estimate_cost(input, graph);
            let card = estimate_cardinality(plan, graph);
            (card, input_cost + card * EXPAND_PER_ROW)
        }

        LogicalPlan::Filter { input, .. } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            let card = estimate_cardinality(plan, graph);
            (card, input_cost + input_card * FILTER_PER_ROW)
        }

        LogicalPlan::CrossProduct { left, right } => {
            let (_, left_cost) = estimate_cost(left, graph);
            let (_, right_cost) = estimate_cost(right, graph);
            let card = estimate_cardinality(plan, graph);
            (card, left_cost + right_cost + card * CROSS_PRODUCT_PER_ROW)
        }

        LogicalPlan::HashJoin { left, right, .. } => {
            let (left_card, left_cost) = estimate_cost(left, graph);
            let (right_card, right_cost) = estimate_cost(right, graph);
            let card = estimate_cardinality(plan, graph);
            let join_cost =
                left_card * HASH_JOIN_BUILD_PER_ROW + right_card * HASH_JOIN_PROBE_PER_ROW;
            (card, left_cost + right_cost + join_cost)
        }

        LogicalPlan::Sort { input, .. } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            let sort_cost = if input_card > 1.0 {
                input_card * input_card.log2() * SORT_LOG_FACTOR
            } else {
                0.0
            };
            (input_card, input_cost + sort_cost)
        }

        LogicalPlan::Limit { input, count } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            let card = if let Expr::Literal(crate::ast::Literal::Integer(n)) = count {
                input_card.min(*n as f64)
            } else {
                input_card
            };
            (card, input_cost)
        }

        LogicalPlan::Eager { input } => {
            let (card, cost) = estimate_cost(input, graph);
            (card, cost + card * 0.1)
        }

        LogicalPlan::Skip { input, count } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            let card = if let Expr::Literal(crate::ast::Literal::Integer(n)) = count {
                (input_card - *n as f64).max(0.0)
            } else {
                input_card
            };
            (card, input_cost)
        }

        LogicalPlan::Aggregate { input, .. } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            let card = (input_card * 0.1).max(1.0);
            (card, input_cost + input_card * AGGREGATE_PER_ROW)
        }

        LogicalPlan::Project { input, .. } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            (input_card, input_cost + input_card * PROJECT_PER_ROW)
        }

        LogicalPlan::Union { left, right, all } => {
            let (left_card, left_cost) = estimate_cost(left, graph);
            let (right_card, right_cost) = estimate_cost(right, graph);
            let card = if *all {
                left_card + right_card
            } else {
                (left_card + right_card) * 0.8
            };
            (card, left_cost + right_cost)
        }

        LogicalPlan::Unwind { input, .. } => {
            let (_, input_cost) = estimate_cost(input, graph);
            let card = estimate_cardinality(plan, graph);
            (card, input_cost + card * 0.1)
        }

        LogicalPlan::LoadCsv { input, .. } => {
            let (_, input_cost) = estimate_cost(input, graph);
            let card = estimate_cardinality(plan, graph);
            // CSV I/O is expensive per row
            (card, input_cost + card * 1.0)
        }

        LogicalPlan::OptionalMatch {
            input, match_plan, ..
        } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            let (_, match_cost) = estimate_cost(match_plan, graph);
            let card = estimate_cardinality(plan, graph);
            (card, input_cost + match_cost + input_card * 0.1)
        }

        // pass-through operators
        LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindQppGroups { input, .. }
        | LogicalPlan::BindVarLists { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. }
        | LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. }
        | LogicalPlan::CallProcedure { input, .. }
        | LogicalPlan::TransactionCommand { input, .. }
        | LogicalPlan::CreateNode { input, .. }
        | LogicalPlan::CreateRel { input, .. }
        | LogicalPlan::Merge { input, .. } => estimate_cost(input, graph),

        LogicalPlan::VectorSearchScan { input, .. } => {
            // per input row: one ANN query (roughly an index startup) + k rows out
            let (input_card, input_cost) = estimate_cost(input, graph);
            let card = estimate_cardinality(plan, graph);
            (
                card,
                input_cost + input_card.max(1.0) * INDEX_SCAN_STARTUP + card * INDEX_SCAN_PER_ROW,
            )
        }

        LogicalPlan::CallSubquery { input, body, .. }
        | LogicalPlan::CallInTransactions { input, body, .. } => {
            // per-row cross product: each outer row pays the full body cost; combined card from
            // `estimate_cardinality`
            let (input_card, input_cost) = estimate_cost(input, graph);
            let (_, body_cost) = estimate_cost(body, graph);
            let card = estimate_cardinality(plan, graph);
            (card, input_cost + input_card * body_cost)
        }

        LogicalPlan::ForEach { input, body, .. } => {
            // cardinality unchanged (input rows pass through); each row pays the body cost once
            // (iterations approximated as one)
            let (input_card, input_cost) = estimate_cost(input, graph);
            let (_, body_cost) = estimate_cost(body, graph);
            (input_card, input_cost + input_card * body_cost)
        }

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
        | LogicalPlan::ShowSettings => (0.0, 0.0),

        LogicalPlan::ShortestPath { input, .. }
        | LogicalPlan::WeightedShortestPath { input, .. } => estimate_cost(input, graph),

        LogicalPlan::DistinctExpand { input, .. } => {
            let (input_card, input_cost) = estimate_cost(input, graph);
            // cheaper than repeated Expand thanks to dedup
            (input_card * 10.0, input_cost + input_card * 5.0)
        }

        LogicalPlan::ScanFilterProject { .. } => {
            let card = estimate_cardinality(plan, graph);
            (
                card,
                card * (SCAN_PER_ROW + FILTER_PER_ROW + PROJECT_PER_ROW),
            )
        }
    }
}
