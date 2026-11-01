//! Pull-based (Volcano) operators. Each implements [`Operator::next`], pulling one [`Frame`] at a
//! time from its child; every operator lives in its own compilation unit, so changing one can't
//! shift another's code layout.

pub(crate) mod aggregate;
mod aggregate_par;
mod aggregate_stream;
pub(crate) mod batch_types;
mod bind;
pub(crate) mod call;
mod eager;
mod expand;
mod filter;
mod flow;
mod join;
pub(crate) mod load_csv;
mod path;
pub(crate) mod pipeline_par;
mod project;
mod scan;
mod schema;
mod subquery;
mod traversal_par;
mod write;

use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::error::CypherError;
use crate::executor::{extract_expand_chain, plan_is_mutating};
use crate::frame::{Frame, SymbolTable};
use crate::memory::RowBuffer;
use crate::planner::LogicalPlan;

/// Pull-based query operator; operators form a tree, each pulling frames from its children via
/// [`next`](Operator::next) and emitting transformed frames one at a time.
pub(crate) trait Operator {
    /// Next frame; `Ok(None)` when exhausted.
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError>;

    /// LIMIT hint from a parent: produce at most `limit` frames. Operators that benefit (e.g.
    /// ExpandOp) pass it to their materialisation; non-filtering intermediaries (ProjectOp without
    /// DISTINCT/WHERE) forward it to their child. Default no-op.
    fn limit_hint(&mut self, _limit: usize) {}
}

// -- Operator tree construction -----------------------------------------

/// Builds an operator tree from a logical plan (recursively).
#[inline(never)]
#[recursive::recursive]
pub(crate) fn build_operator_tree<'p>(plan: &'p LogicalPlan) -> Box<dyn Operator + 'p> {
    match plan {
        LogicalPlan::EmptyRow => Box::new(scan::EmptyRowOp::new()),

        LogicalPlan::ContextSeed => Box::new(scan::ContextSeedOp::new()),

        LogicalPlan::Scan { variable, label } => {
            Box::new(scan::ScanOp::new(variable, label.as_deref()))
        }

        LogicalPlan::NodeById {
            variable,
            label,
            id_expr,
        } => Box::new(scan::NodeByIdOp::new(variable, label.as_deref(), id_expr)),

        LogicalPlan::NodeByIdRange {
            variable,
            label,
            lower,
            upper,
        } => Box::new(scan::NodeByIdRangeOp::new(
            variable,
            label.as_deref(),
            lower.as_ref(),
            upper.as_ref(),
        )),

        LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
        } => Box::new(scan::EdgeTypeScanOp::new(
            from_var, rel_var, to_var, edge_types, *direction,
        )),

        LogicalPlan::IndexScan {
            variable,
            label,
            property,
            value,
        } => Box::new(scan::IndexScanOp::new(variable, label, property, value)),

        LogicalPlan::IndexLookupJoin {
            input,
            variable,
            label,
            property,
            key,
        } => {
            let input_op = build_operator_tree(input);
            Box::new(scan::IndexLookupJoinOp::new(
                input_op, variable, label, property, key,
            ))
        }

        LogicalPlan::IndexRangeScan {
            variable,
            label,
            property,
            op,
            value,
        } => Box::new(scan::IndexRangeScanOp::new(
            variable, label, property, *op, value,
        )),

        LogicalPlan::PointIndexScan {
            variable,
            label,
            property,
            query,
        } => Box::new(scan::PointIndexScanOp::new(
            variable, label, property, query,
        )),

        LogicalPlan::TextIndexScan {
            variable,
            label,
            property,
            kind,
            pattern,
        } => Box::new(scan::TextIndexScanOp::new(
            variable, label, property, *kind, pattern,
        )),

        LogicalPlan::VectorSearchScan {
            input,
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
        } => {
            let child = build_operator_tree(input);
            Box::new(scan::VectorSearchScanOp::new(
                child,
                variable,
                index_name,
                query_vector,
                index_where.as_ref(),
                limit,
                score_var.as_deref(),
            ))
        }

        LogicalPlan::ScanFilterProject { .. } => Box::new(scan::ScanFilterProjectOp::new(plan)),

        LogicalPlan::Filter { input, predicate } => {
            // constant-folded `false` predicate: emit nothing, never iterate the input (e.g.
            // `EXISTS { MATCH (n) WHERE 1 = 0 RETURN n }` never scans)
            if matches!(
                predicate,
                crate::ast::Expr::Literal(crate::ast::Literal::Bool(false))
            ) {
                return Box::new(scan::NoRowsOp::new());
            }
            // fuse Filter + Expand into one ExpandFilterOp: look through intermediate Filters (e.g.
            // label checks) for the underlying Expand and fuse THIS predicate into it; intermediate
            // filters become FilterOps around the fused op
            if let Some((expand_plan, dir, intermediates)) = find_expand_through_filters(input) {
                // ExpandFilterOp with this predicate fused in
                let mut op: Box<dyn Operator + 'p> =
                    Box::new(expand::ExpandFilterOp::new(expand_plan, predicate, dir));
                // intermediate filters on top, innermost first
                for intermediate_pred in intermediates {
                    op = Box::new(filter::FilterOp::new(op, intermediate_pred));
                }
                return op;
            }
            let child = build_operator_tree(input);
            Box::new(filter::FilterOp::new(child, predicate))
        }

        LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } => {
            let child = build_operator_tree(input);
            Box::new(project::ProjectOp::new(
                child,
                items,
                *distinct,
                where_clause.as_ref(),
            ))
        }

        LogicalPlan::Limit { input, count } => {
            // streaming expand chain: depth-first for 2+ consecutive fixed-length expands under a
            // LIMIT
            if extract_expand_chain(input).is_some() {
                return Box::new(flow::StreamingExpandChainLimitOp::new(plan));
            }
            // writes: LIMIT must not cut side effects short, so materialize the child, then limit
            if plan_is_mutating(input) {
                return Box::new(flow::MaterializingLimitOp::new(plan));
            }
            let child = build_operator_tree(input);
            Box::new(flow::LimitOp::new(child, count))
        }

        LogicalPlan::Skip { input, count } => {
            let child = build_operator_tree(input);
            Box::new(flow::SkipOp::new(child, count))
        }

        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        }
        | LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        }
        | LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => {
            let dir = expand_direction(plan).unwrap();
            if range.is_some() {
                // variable length: buffered
                Box::new(expand::ExpandOp::new(plan, dir))
            } else {
                // single hop: streaming
                let child = build_operator_tree(input);
                Box::new(expand::StreamingExpandOp::new(
                    child,
                    dir,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    prior_edge_vars,
                ))
            }
        }

        LogicalPlan::DistinctExpand { .. } => Box::new(expand::DistinctExpandOp::new(plan)),

        LogicalPlan::ShortestPath { .. } => Box::new(path::ShortestPathOp::new(plan)),

        LogicalPlan::WeightedShortestPath { .. } => {
            Box::new(path::WeightedShortestPathOp::new(plan))
        }

        LogicalPlan::Sort { input, items } => {
            let child = build_operator_tree(input);
            Box::new(flow::SortOp::new(child, items))
        }

        LogicalPlan::Aggregate { .. } => Box::new(aggregate::AggregateOp::new(plan)),

        LogicalPlan::Unwind {
            input,
            expr,
            alias,
            clear_source,
        } => {
            let child = build_operator_tree(input);
            Box::new(flow::UnwindOp::new(child, expr, alias, *clear_source))
        }

        LogicalPlan::LoadCsv { .. } => Box::new(load_csv::LoadCsvOp::new(plan)),

        LogicalPlan::Union { .. } => Box::new(flow::UnionOp::new(plan)),

        LogicalPlan::CrossProduct { .. } => Box::new(join::CrossProductOp::new(plan)),

        LogicalPlan::HashJoin { .. } => Box::new(join::HashJoinOp::new(plan)),

        LogicalPlan::Eager { .. } => Box::new(eager::EagerOp::new(plan)),

        LogicalPlan::OptionalMatch { .. } => Box::new(join::OptionalMatchOp::new(plan)),

        LogicalPlan::CreateNode { .. } => Box::new(write::CreateNodeOp::new(plan)),

        LogicalPlan::CreateRel { .. } => Box::new(write::CreateRelOp::new(plan)),

        LogicalPlan::SetProperty { .. } => Box::new(write::SetPropertyOp::new(plan)),

        LogicalPlan::RemoveOp { .. } => Box::new(write::RemoveOpOp::new(plan)),

        LogicalPlan::Delete { .. } => Box::new(write::DeleteOp::new(plan)),

        LogicalPlan::Merge { .. } => Box::new(write::MergeOp::new(plan)),

        LogicalPlan::BindPath { .. } => Box::new(bind::BindPathOp::new(plan)),

        LogicalPlan::BindQppGroups { .. } => Box::new(bind::BindQppGroupsOp::new(plan)),

        LogicalPlan::BindVarLists { .. } => Box::new(bind::BindVarListsOp::new(plan)),

        LogicalPlan::BindPathEndpoints { .. } => Box::new(bind::BindPathEndpointsOp::new(plan)),

        LogicalPlan::BindEdgeEndpoints { .. } => Box::new(bind::BindEdgeEndpointsOp::new(plan)),

        LogicalPlan::CallProcedure { .. } => Box::new(call::CallProcedureOp::new(plan)),

        LogicalPlan::CallSubquery { .. } => Box::new(subquery::CallSubqueryOp::new(plan)),

        LogicalPlan::CallInTransactions { .. } => {
            Box::new(subquery::CallInTransactionsOp::new(plan))
        }

        LogicalPlan::ForEach { .. } => Box::new(write::ForEachOp::new(plan)),

        LogicalPlan::SeededRows(frames) => Box::new(scan::SeededRowsOp::new(frames)),

        LogicalPlan::CreateIndex { .. } => Box::new(schema::CreateIndexOp::new(plan)),

        LogicalPlan::CreateFulltextIndex { .. } => {
            Box::new(schema::CreateFulltextIndexOp::new(plan))
        }

        LogicalPlan::CreatePointIndex { .. } => Box::new(schema::CreatePointIndexOp::new(plan)),

        LogicalPlan::CreateTextIndex { .. } => Box::new(schema::CreateTextIndexOp::new(plan)),

        LogicalPlan::CreateVectorIndex { .. } => Box::new(schema::CreateVectorIndexOp::new(plan)),

        LogicalPlan::DropIndex { .. } => Box::new(schema::DropIndexOp::new(plan)),

        LogicalPlan::AnalyzeGraph { .. } => Box::new(schema::AnalyzeGraphOp::new(plan)),

        LogicalPlan::CreateConstraint { .. } => Box::new(schema::CreateConstraintOp::new(plan)),

        LogicalPlan::DropConstraint { .. } => Box::new(schema::DropConstraintOp::new(plan)),

        LogicalPlan::CreateEdgeIndex { .. } => Box::new(schema::CreateEdgeIndexOp::new(plan)),

        LogicalPlan::DropEdgeIndex { .. } => Box::new(schema::DropEdgeIndexOp::new(plan)),

        LogicalPlan::ShowIndexes => Box::new(schema::ShowIndexesOp::new()),

        LogicalPlan::ShowFunctions => Box::new(schema::ShowFunctionsOp::new()),

        LogicalPlan::ShowProcedures => Box::new(schema::ShowProceduresOp::new()),

        LogicalPlan::TransactionCommand {
            input,
            command,
            ids,
        } => Box::new(schema::TransactionCommandOp::new(
            build_operator_tree(input),
            *command,
            ids.clone(),
        )),

        LogicalPlan::ShowSettings => Box::new(schema::ShowSettingsOp::new()),

        LogicalPlan::ShowConstraints => Box::new(schema::ShowConstraintsOp::new()),
    }
}

/// Walks `Filter`s down to an underlying Expand: `(expand_plan, direction,
/// intermediate_predicates)`, the predicates between the caller's filter and the expand
/// innermost-first, for layering on the fused ExpandFilterOp. At most 2 intermediate filters (label
/// check + property filter).
fn find_expand_through_filters(
    plan: &LogicalPlan,
) -> Option<(&LogicalPlan, Direction, Vec<&Expr>)> {
    // direct child is an Expand: no intermediates
    if let Some(dir) = expand_direction(plan) {
        return Some((plan, dir, Vec::new()));
    }
    // look through 1-2 intermediate Filters
    let mut current = plan;
    let mut intermediates = Vec::new();
    for _ in 0..2 {
        if let LogicalPlan::Filter { input, predicate } = current {
            intermediates.push(predicate);
            let dir = expand_direction(input);
            if let Some(d) = dir {
                return Some((input, d, intermediates));
            }
            current = input.as_ref();
        } else {
            break;
        }
    }
    None
}

fn expand_direction(plan: &LogicalPlan) -> Option<Direction> {
    match plan {
        LogicalPlan::ExpandOut { .. } => Some(Direction::Outgoing),
        LogicalPlan::ExpandIn { .. } => Some(Direction::Incoming),
        LogicalPlan::ExpandBoth { .. } => Some(Direction::Both),
        _ => None,
    }
}

/// Drains an operator tree into a charged buffer: the bridge to materializing operators. Every
/// buffered frame is charged to the query memory budget (see `memory`); the timeout is checked
/// every 4096 frames.
#[inline(never)]
pub(crate) fn drain_operator(
    op: &mut dyn Operator,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<RowBuffer, CypherError> {
    let mut frames = RowBuffer::new();
    let mut count: u32 = 0;
    while let Some(frame) = op.next(graph, symtab)? {
        frames.push(frame)?;
        count = count.wrapping_add(1);
        if count & 0xFFF == 0 {
            crate::query::check_interrupt()?;
        }
    }
    Ok(frames)
}

/// Runs an operator tree for its side effects, keeping no rows. For statements without `RETURN`, so
/// a write over a huge input (or cross product) runs in constant memory.
#[inline(never)]
pub(crate) fn drain_operator_discard(
    op: &mut dyn Operator,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<usize, CypherError> {
    let mut count: usize = 0;
    while let Some(frame) = op.next(graph, symtab)? {
        drop(frame);
        count += 1;
        if count & 0xFFF == 0 {
            crate::query::check()?;
        }
    }
    Ok(count)
}

/// Runs a plan subtree through the operator tree.
pub(crate) fn exec_via_operators(
    plan: &LogicalPlan,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<RowBuffer, CypherError> {
    let mut op = build_operator_tree(plan);
    drain_operator(&mut *op, graph, symtab)
}

/// Runs a plan subtree with its leaf EmptyRow replaced by a seed frame. OptionalMatchOp uses it to
/// run match_plan on the slot-based operator pipeline instead of the HashMap-based exec_subquery.
#[recursive::recursive]
pub(crate) fn exec_via_operators_seeded(
    plan: &LogicalPlan,
    seed: Frame,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    match plan {
        LogicalPlan::EmptyRow => Ok(vec![seed]),
        LogicalPlan::Scan { variable, label } => {
            // variable already bound in the seed: check the label (if any), return the seed, no
            // rescan
            if let Some(slot) = symtab.slot(variable) {
                match &seed[slot] {
                    crate::frame::SlotValue::NodeRef(nid) => {
                        if let Some(lbl) = label {
                            let node = graph.get_node(*nid).map_err(CypherError::from)?;
                            let matches = node
                                .labels
                                .iter()
                                .any(|lid| graph.strings().resolve(lid.0) == lbl.as_str());
                            if matches {
                                return Ok(vec![seed]);
                            } else {
                                return Ok(Vec::new());
                            }
                        }
                        return Ok(vec![seed]);
                    }
                    crate::frame::SlotValue::Null => {
                        // not bound: full scan
                    }
                    _ => {
                        return Ok(vec![seed]);
                    }
                }
            }
            // not bound in the seed: normal scan merged into the seed
            let node_ids = match label {
                Some(lbl) => graph.nodes_by_label(lbl),
                None => graph.nodes_vec(),
            };
            let slot = symtab.slot(variable).unwrap();
            let mut result = Vec::with_capacity(node_ids.len());
            let mut count: u32 = 0;
            for nid in node_ids {
                let mut frame = seed.clone();
                frame[slot] = crate::frame::SlotValue::NodeRef(nid);
                result.push(frame);
                count = count.wrapping_add(1);
                if count & 0xFFF == 0 {
                    crate::query::check()?;
                }
            }
            Ok(result)
        }
        LogicalPlan::Filter { input, predicate } => {
            let input_frames = exec_via_operators_seeded(input, seed, graph, symtab)?;
            let compiled = crate::bytecode::try_compile(predicate, symtab);
            let mut result = Vec::new();
            for frame in input_frames {
                let pass = if let Some(ref bc) = compiled {
                    crate::bytecode::eval_bytecode(bc, &frame, &*graph)?
                } else {
                    crate::executor::is_truthy(&crate::executor::eval_expr(
                        predicate, &frame, &*graph, symtab,
                    )?)
                };
                if pass {
                    result.push(frame);
                }
            }
            Ok(result)
        }
        LogicalPlan::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } => {
            let frames = exec_via_operators_seeded(input, seed, graph, symtab)?;
            Ok(bind::bind_path_frames(
                frames, variable, node_vars, edge_vars, symtab,
            ))
        }
        LogicalPlan::BindQppGroups {
            input,
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        } => {
            let frames = exec_via_operators_seeded(input, seed, graph, symtab)?;
            Ok(bind::bind_qpp_group_frames(
                frames,
                anchor_var,
                rel_var,
                src_var.as_deref(),
                dst_var.as_deref(),
                symtab,
            ))
        }
        LogicalPlan::BindVarLists { input, groups } => {
            let frames = exec_via_operators_seeded(input, seed, graph, symtab)?;
            Ok(bind::bind_var_list_frames(frames, groups, symtab))
        }
        LogicalPlan::VectorSearchScan {
            input,
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
        } => {
            // OPTIONAL MATCH ... SEARCH: search per seeded input frame
            let input_frames = exec_via_operators_seeded(input, seed, graph, symtab)?;
            let mut result = Vec::new();
            for frame in input_frames {
                result.extend(scan::vector_search_rows(
                    &frame,
                    variable,
                    index_name,
                    query_vector,
                    index_where.as_ref(),
                    limit,
                    score_var.as_deref(),
                    &*graph,
                    symtab,
                )?);
            }
            Ok(result)
        }
        _ => exec_plan_with_seeded_input(plan, seed, graph, symtab),
    }
}

/// Runs a plan node whose input contains an EmptyRow leaf: resolves the input with the seed, then
/// runs the node on the operator path.
fn exec_plan_with_seeded_input(
    plan: &LogicalPlan,
    seed: Frame,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    // input child plan
    let input = match plan {
        LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. } => input.as_ref(),
        // other plan types (BindPath, DistinctExpand, ShortestPath, ...): Record-based exec_subquery
        // handles them all
        _ => return exec_subquery_fallback(plan, seed, graph, symtab),
    };

    // resolve the input subtree with the seed
    let input_frames = exec_via_operators_seeded(input, seed, graph, symtab)?;

    // expand over the resolved input frames
    let (from, rel_var, to, edge_types, range, prior_edge_vars, direction) = match plan {
        LogicalPlan::ExpandOut {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => (
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            traverse_core::graph::types::Direction::Outgoing,
        ),
        LogicalPlan::ExpandIn {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => (
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            traverse_core::graph::types::Direction::Incoming,
        ),
        LogicalPlan::ExpandBoth {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => (
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            traverse_core::graph::types::Direction::Both,
        ),
        _ => unreachable!(),
    };

    crate::executor::exec_expand_frames(
        &input_frames,
        from,
        rel_var,
        to,
        edge_types,
        range,
        prior_edge_vars,
        direction,
        graph,
        None,
        symtab,
    )
}

/// Record-based exec_subquery fallback for plan types the seeded operator path doesn't handle
/// (BindPath, DistinctExpand, ...).
fn exec_subquery_fallback(
    plan: &LogicalPlan,
    seed: Frame,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    let rec = crate::frame::frame_to_record(&seed, symtab);
    let records = crate::executor::exec_subquery(plan, &rec, &*graph)?;
    let mut result = Vec::with_capacity(records.len());
    for r in &records {
        let mut frame = seed.clone();
        for (k, v) in r {
            if let Some(slot) = symtab.slot(k) {
                frame[slot] = crate::frame::SlotValue::from(v.clone());
            }
        }
        result.push(frame);
    }
    Ok(result)
}
