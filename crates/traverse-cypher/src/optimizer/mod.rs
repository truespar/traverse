//! Cost-based Cypher optimizer between planner and executor: rule-based rewrites
//! (predicate push-down, filter fusion, WHERE -> IndexScan, ...) and DP join
//! ordering. Write operators pass through unchanged; their read subtrees are
//! optimized like any read plan.

pub mod cardinality;
pub mod cost;
pub mod eager;
pub mod join_order;
pub mod rewrite;

use traverse_core::tx::view::GraphView;

use crate::planner::{IndexHints, LogicalPlan};

/// Full optimization pipeline:
/// 1. rule-based rewrites (index scans, predicate split / push-down / fusion)
/// 2. cost-based join ordering (only with a CrossProduct)
/// 3. post-join rewrites (re-push and re-fuse after reordering)
pub fn optimize(plan: LogicalPlan, graph: &dyn GraphView, hints: &IndexHints) -> LogicalPlan {
    if is_mutating_plan(&plan) {
        // write operators stay untouched, but their READ subtrees are ordinary read
        // plans and get the full pipeline. Otherwise imports like
        //   LOAD CSV ... MATCH (a {k: row.x}) MATCH (b {k: row.y}) MERGE ...
        // stay raw cartesian products that materialize every combination and exhaust
        // memory; lookup-join and push-down rewrites turn them into per-row seeks.
        let plan = optimize_write_read_subtrees(plan, graph, hints);
        // last: writes stream, so decide where a row must not pass a write before the previous
        // rows' writes are done (see `eager`). Runs after every other rewrite; none of them
        // understands `Eager`, nor needs to.
        return eager::insert_eager_barriers(plan);
    }

    // parameters are deliberately NOT resolved at the top level. The plan cache
    // keys on literal-stripped text, so `WHERE id(n) = 0` and `WHERE id(n) = 5`
    // share an entry; inlining `$__sp0 -> Literal(0)` would bake `0` into the
    // cached plan for every later call. Rewrites depending on the value (e.g.
    // `convert_filter_to_node_by_id_range`) take `Expr::Parameter` and resolve at
    // execute time in the operator. `optimize_subquery_plan` in the executor may
    // inline: subquery plans aren't cached.

    // phase 1: rule-based rewrites. Fold constants first so trivially false Filters cost nothing
    // downstream, and `WHERE 1 = 0` is a known Bool(false) before push-down places it
    let plan = rewrite::fold_constants(plan);
    let plan = rewrite::convert_filter_to_index_scan(plan, hints);
    let plan = rewrite::split_conjunctions(plan);
    let plan = rewrite::push_filters_down(plan);
    let plan = rewrite::fuse_filters(plan);
    // narrow unlabeled scans from label predicates (`WHERE n:A&!B`, `(n:(A|B)&!C)`); after
    // fuse_filters (one canonical Filter), before ScanFilterProject fusion
    let plan = rewrite::push_label_filters_into_scan(plan);
    // correlated `var.prop = <expr over other vars>` above a CrossProduct with an indexed Scan side
    // -> input-driven index seek (lookup join). After fuse_filters (one AND-chain Filter), before
    // join ordering (which would only reorder the O(nxm) scan)
    let plan = rewrite::convert_cross_product_to_index_lookup_join(plan, hints);
    // OR over a pattern's two endpoints, both indexable -> disjoint UNION ALL of two seek-rooted
    // branches
    let plan = rewrite::split_or_filter_into_union_seeks(plan, hints);
    // with `id(var) = lit` pushed next to its Scan, collapse both into a NodeById seek: after
    // push_filters_down + fuse_filters, before reverse_expand_direction (which expects the seek in
    // place)
    let plan = rewrite::convert_filter_to_node_by_id(plan);
    // like `convert_filter_to_node_by_id` for ranges (`id(var) < lit`, `>=`, ...): `NodeByIdRange`
    // visits only the id span instead of the whole label
    let plan = rewrite::convert_filter_to_node_by_id_range(plan);

    // phase 1.5: reverse expand direction when the target filter is selective
    let plan = rewrite::reverse_expand_direction(plan, graph);

    // phase 2: cost-based join ordering (only with a CrossProduct)
    let plan = if has_cross_product(&plan) {
        join_order::optimize_join_order(plan, graph)
    } else {
        plan
    };

    // phase 3: post-join rewrites
    let plan = rewrite::split_conjunctions(plan);
    let plan = rewrite::push_filters_down(plan);
    let plan = rewrite::fuse_filters(plan);
    let plan = rewrite::convert_filter_to_node_by_id(plan);
    let plan = rewrite::convert_filter_to_node_by_id_range(plan);
    let plan = rewrite::convert_cross_to_hash_join(plan);

    // phase 3.5: reverse expand direction again after post-join rewrites
    let plan = rewrite::reverse_expand_direction(plan, graph);

    // phase 3.6: index scans again (reversal may have put range filters next to scans)
    let plan = rewrite::convert_filter_to_index_scan(plan, hints);

    // phase 3.7: spatial (point) index acceleration: point.distance/withinBBox filters and kNN
    // ORDER BY + LIMIT to the R-tree. After filters sit next to their scans, before scan/filter
    // fusion (which only matches plain Scan)
    let plan = rewrite::convert_spatial_predicates(plan, graph);

    // phase 3.8: text (trigram) index acceleration for CONTAINS / ENDS WITH / STARTS WITH / string
    // equality. After convert_filter_to_index_scan, so a b-tree equality IndexScan wins when both
    // exist
    let plan = rewrite::convert_text_predicates(plan, graph);

    // phase 4: fuse distinct expansion chains
    let plan = rewrite::fuse_distinct_expand(plan);

    let plan = rewrite::convert_anchorless_expand_to_edge_type_scan(plan);

    // un-group aggregates keyed by one-row scalars carried through a CrossProduct (`WITH count(...)
    // AS a MATCH ...`), restoring the ungrouped count / label-index fast paths
    let plan = rewrite::lift_scalar_carry_aggregate(plan);

    // phase 5: fuse scan + filter + project (+ limit)

    rewrite::fuse_scan_filter_project(plan, graph)
}

/// Descends through write and pass-through operators, running the read pipeline on each maximal
/// non-mutating subtree. Unknown mutating shapes come back unchanged.
fn optimize_write_read_subtrees(
    plan: LogicalPlan,
    graph: &dyn GraphView,
    hints: &IndexHints,
) -> LogicalPlan {
    // ITERATIVE spine walk: big import scripts plan as hundreds of nested CreateNode/CreateRel
    // levels (the Movies script is ~600 deep), too deep to recurse. Detach inputs going down,
    // optimize the read subtree at the bottom, reattach going up
    fn take_input(node: &mut LogicalPlan) -> Option<LogicalPlan> {
        let slot = match node {
            LogicalPlan::CreateNode { input, .. }
            | LogicalPlan::CreateRel { input, .. }
            | LogicalPlan::Merge { input, .. }
            | LogicalPlan::SetProperty { input, .. }
            | LogicalPlan::RemoveOp { input, .. }
            | LogicalPlan::Delete { input, .. }
            | LogicalPlan::ForEach { input, .. }
            | LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Skip { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Unwind { input, .. }
            | LogicalPlan::LoadCsv { input, .. } => input,
            _ => return None,
        };
        Some(std::mem::replace(&mut **slot, LogicalPlan::EmptyRow))
    }
    #[recursive::recursive]
    fn put_input(node: &mut LogicalPlan, new_input: LogicalPlan) {
        if let LogicalPlan::CreateNode { input, .. }
        | LogicalPlan::CreateRel { input, .. }
        | LogicalPlan::Merge { input, .. }
        | LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. }
        | LogicalPlan::ForEach { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::LoadCsv { input, .. } = node
        {
            **input = new_input;
        }
    }

    let mut spine: Vec<LogicalPlan> = Vec::new();
    let mut cur = plan;
    loop {
        if !is_mutating_plan(&cur) {
            // SAFE SUBSET only, no fusion / pruning / reordering: those choose what to keep from
            // what the READ plan consumes and can't see the writes above (fusion could drop a
            // binding a DELETE above needs). The access-path rewrites below keep bindings
            cur = rewrite::fold_constants(cur);
            cur = rewrite::convert_filter_to_index_scan(cur, hints);
            cur = rewrite::split_conjunctions(cur);
            cur = rewrite::push_filters_down(cur);
            cur = rewrite::fuse_filters(cur);
            cur = rewrite::push_label_filters_into_scan(cur);
            cur = rewrite::convert_cross_product_to_index_lookup_join(cur, hints);
            // un-indexed correlated MATCH ... MATCH (b {k: <expr over a>}): without an index the
            // lookup join can't fire and the plan would stay a cartesian product under a Filter.
            // HashJoin keeps the same bindings, so it belongs here too: one probe per row instead
            // of every pair
            cur = rewrite::convert_cross_to_hash_join(cur);
            cur = rewrite::split_or_filter_into_union_seeks(cur, hints);
            cur = rewrite::convert_filter_to_node_by_id(cur);
            cur = rewrite::convert_filter_to_node_by_id_range(cur);
            let _ = graph;
            break;
        }
        // Union branches / unknown mutating shapes: leave as is
        let mut node = cur;
        match take_input(&mut node) {
            Some(inner) => {
                spine.push(node);
                cur = inner;
            }
            None => {
                cur = node;
                break;
            }
        }
    }
    while let Some(mut node) = spine.pop() {
        put_input(&mut node, cur);
        cur = node;
    }
    cur
}

/// Contains any write?
#[recursive::recursive]
pub(crate) fn is_mutating_plan(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::CreateNode { .. }
        | LogicalPlan::CreateRel { .. }
        | LogicalPlan::SetProperty { .. }
        | LogicalPlan::RemoveOp { .. }
        | LogicalPlan::Delete { .. }
        | LogicalPlan::Merge { .. }
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
        | LogicalPlan::DropEdgeIndex { .. } => true,
        LogicalPlan::ShowIndexes
        | LogicalPlan::ShowConstraints
        | LogicalPlan::ShowFunctions
        | LogicalPlan::ShowProcedures
        | LogicalPlan::ShowSettings => false,
        LogicalPlan::TransactionCommand { input, .. } => is_mutating_plan(input),

        LogicalPlan::IndexLookupJoin { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::LoadCsv { input, .. }
        | LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. }
        | LogicalPlan::DistinctExpand { input, .. }
        | LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindQppGroups { input, .. }
        | LogicalPlan::BindVarLists { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. }
        | LogicalPlan::CallProcedure { input, .. }
        | LogicalPlan::VectorSearchScan { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::ShortestPath { input, .. }
        | LogicalPlan::WeightedShortestPath { input, .. } => is_mutating_plan(input),

        LogicalPlan::CallSubquery { input, body, .. }
        | LogicalPlan::CallInTransactions { input, body, .. } => {
            is_mutating_plan(input) || is_mutating_plan(body)
        }

        // FOREACH always mutates (updates-only body)
        LogicalPlan::ForEach { input, body, .. } => {
            is_mutating_plan(input) || is_mutating_plan(body)
        }

        LogicalPlan::Union { left, right, .. }
        | LogicalPlan::CrossProduct { left, right }
        | LogicalPlan::HashJoin { left, right, .. }
        | LogicalPlan::OptionalMatch {
            input: left,
            match_plan: right,
            ..
        } => is_mutating_plan(left) || is_mutating_plan(right),

        LogicalPlan::Scan { .. }
        | LogicalPlan::IndexScan { .. }
        | LogicalPlan::IndexRangeScan { .. }
        | LogicalPlan::PointIndexScan { .. }
        | LogicalPlan::TextIndexScan { .. }
        | LogicalPlan::NodeById { .. }
        | LogicalPlan::NodeByIdRange { .. }
        | LogicalPlan::EdgeTypeScan { .. }
        | LogicalPlan::ScanFilterProject { .. }
        | LogicalPlan::SeededRows(_)
        | LogicalPlan::ContextSeed
        | LogicalPlan::EmptyRow => false,
    }
}

/// Contains a `CrossProduct`?
#[recursive::recursive]
fn has_cross_product(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::CrossProduct { .. } => true,
        LogicalPlan::ContextSeed => false,

        LogicalPlan::IndexLookupJoin { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::LoadCsv { input, .. }
        | LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. }
        | LogicalPlan::DistinctExpand { input, .. }
        | LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindQppGroups { input, .. }
        | LogicalPlan::BindVarLists { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. }
        | LogicalPlan::CallProcedure { input, .. }
        | LogicalPlan::VectorSearchScan { input, .. }
        | LogicalPlan::CreateNode { input, .. }
        | LogicalPlan::CreateRel { input, .. }
        | LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. }
        | LogicalPlan::Merge { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::ShortestPath { input, .. }
        | LogicalPlan::WeightedShortestPath { input, .. } => has_cross_product(input),

        LogicalPlan::CallSubquery { input, body, .. }
        | LogicalPlan::CallInTransactions { input, body, .. } => {
            has_cross_product(input) || has_cross_product(body)
        }

        LogicalPlan::ForEach { input, body, .. } => {
            has_cross_product(input) || has_cross_product(body)
        }

        LogicalPlan::Union { left, right, .. }
        | LogicalPlan::HashJoin { left, right, .. }
        | LogicalPlan::OptionalMatch {
            input: left,
            match_plan: right,
            ..
        } => has_cross_product(left) || has_cross_product(right),

        LogicalPlan::Scan { .. }
        | LogicalPlan::IndexScan { .. }
        | LogicalPlan::IndexRangeScan { .. }
        | LogicalPlan::PointIndexScan { .. }
        | LogicalPlan::TextIndexScan { .. }
        | LogicalPlan::NodeById { .. }
        | LogicalPlan::NodeByIdRange { .. }
        | LogicalPlan::EdgeTypeScan { .. }
        | LogicalPlan::ScanFilterProject { .. }
        | LogicalPlan::SeededRows(_)
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
        | LogicalPlan::ShowSettings => false,
        LogicalPlan::TransactionCommand { input, .. } => has_cross_product(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    #[test]
    fn mutating_plan_unchanged() {
        let plan = LogicalPlan::CreateNode {
            input: Box::new(LogicalPlan::EmptyRow),
            variable: Some("n".to_string()),
            labels: vec!["Person".to_string()],
            dynamic_labels: Vec::new(),
            properties: None,
        };

        let g = traverse_core::graph::Graph::new();
        let hints = IndexHints::new();
        let result = optimize(plan.clone(), &g, &hints);

        // mutating plans pass through
        assert!(matches!(result, LogicalPlan::CreateNode { .. }));
    }

    #[test]
    fn read_only_plan_is_not_mutating() {
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Scan {
                variable: "n".to_string(),
                label: Some("Person".to_string()),
            }),
            predicate: Expr::Literal(Literal::Bool(true)),
        };
        assert!(!is_mutating_plan(&plan));
    }

    #[test]
    fn has_cross_product_detects_nested() {
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::CrossProduct {
                left: Box::new(LogicalPlan::Scan {
                    variable: "a".to_string(),
                    label: None,
                }),
                right: Box::new(LogicalPlan::Scan {
                    variable: "b".to_string(),
                    label: None,
                }),
            }),
            predicate: Expr::Literal(Literal::Bool(true)),
        };
        assert!(has_cross_product(&plan));
    }
}
