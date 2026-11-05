//! Physical operators: pull-based over chunks. Each `next` fills or narrows groups of the shared
//! [`ResultSet`] and reports whether the chunk holds rows; operators own their children and are
//! built per execution from the immutable plan.

pub mod aggregate;
pub mod bind;
pub mod call;
pub mod chain;
pub mod expand;
pub mod flow;
pub mod join;
pub mod load_csv;
pub mod path;
pub mod project;
pub mod schema;
pub mod sort;
pub mod source;
pub mod subquery;
pub mod write;

use crate::error::CypherError;
use crate::memory::Charge;
use crate::types::CypherValue;

use super::chunk::{DataPos, GroupId, ResultSet};
use super::ctx::ExecCtx;
use super::plan::{PhysNode, SeedRow};
use super::vector::{VECTOR_CAPACITY, ValueRef, Vector, VectorBuilder};

/// A physical operator.
pub trait PhysOperator {
    /// Produces the next chunk into `rs`; `false` when exhausted.
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError>;

    /// Rewinds the operator (and its inputs) for a new run.
    fn reset(&mut self);

    /// Hands seed rows to the [`PhysNode::Seed`] source below; operators forward it to their
    /// inputs.
    fn seed(&mut self, rows: Vec<SeedRow>);
}

/// Stand-ins the parallel scheduler plants in a worker's tree: operators replacing whole nodes
/// (a morsel's slice of a leaf, a replay of the chunks a breaker below produced) and prebuilt
/// tables for hash joins whose build side already ran. Each is taken when its node is reached.
pub(crate) struct Substs<'p> {
    stand_ins: Vec<(*const PhysNode, Box<dyn PhysOperator + 'p>)>,
    probes: Vec<(*const PhysNode, std::sync::Arc<join::BuildTable>)>,
}

impl<'p> Substs<'p> {
    /// Nothing substituted.
    pub(crate) fn none() -> Self {
        Self {
            stand_ins: Vec::new(),
            probes: Vec::new(),
        }
    }

    /// Replaces `at` with `op`.
    pub(crate) fn stand_in(&mut self, at: &PhysNode, op: Box<dyn PhysOperator + 'p>) {
        self.stand_ins.push((at as *const PhysNode, op));
    }

    /// The hash join `at` probes `table` instead of building its left side.
    pub(crate) fn probe(&mut self, at: &PhysNode, table: std::sync::Arc<join::BuildTable>) {
        self.probes.push((at as *const PhysNode, table));
    }

    /// Everything was taken?
    pub(crate) fn is_empty(&self) -> bool {
        self.stand_ins.is_empty() && self.probes.is_empty()
    }

    fn take_stand_in(&mut self, node: &PhysNode) -> Option<Box<dyn PhysOperator + 'p>> {
        let i = self
            .stand_ins
            .iter()
            .position(|(at, _)| std::ptr::eq(node, *at))?;
        Some(self.stand_ins.remove(i).1)
    }

    fn take_probe(&mut self, node: &PhysNode) -> Option<std::sync::Arc<join::BuildTable>> {
        let i = self
            .probes
            .iter()
            .position(|(at, _)| std::ptr::eq(node, *at))?;
        Some(self.probes.remove(i).1)
    }
}

/// Builds the operator tree of `node`.
pub fn build_operator<'p>(node: &'p PhysNode) -> Result<Box<dyn PhysOperator + 'p>, CypherError> {
    build_with(node, &mut Substs::none())
}

/// [`build_operator`] with the nodes in `sub` replaced by their stand-ins.
#[recursive::recursive]
pub(crate) fn build_with<'p>(
    node: &'p PhysNode,
    sub: &mut Substs<'p>,
) -> Result<Box<dyn PhysOperator + 'p>, CypherError> {
    if let Some(op) = sub.take_stand_in(node) {
        return Ok(op);
    }
    Ok(match node {
        PhysNode::EmptyRow { group } => Box::new(source::EmptyRowOp::new(*group)),
        PhysNode::Seed { group, cols } => Box::new(source::SeedOp::new(*group, cols)),
        PhysNode::Nest { input, inner } => Box::new(source::NestOp::new(
            build_with(input, sub)?,
            build_with(inner, sub)?,
        )),
        PhysNode::NodeScan { out, label, seek } => Box::new(source::NodeScanOp::new(
            *out,
            label.as_deref(),
            seek.as_ref(),
        )),
        PhysNode::NodeByIdSeek { out, label, id } => {
            Box::new(source::NodeByIdSeekOp::new(*out, label.as_deref(), id))
        }
        PhysNode::NodeByIdRange {
            out,
            label,
            lower,
            upper,
        } => Box::new(source::NodeByIdRangeOp::new(
            *out,
            label.as_deref(),
            lower.as_ref(),
            upper.as_ref(),
        )),
        PhysNode::IndexSeek {
            out,
            label,
            property,
            value,
        } => Box::new(source::IndexSeekOp::new(*out, label, property, value)),
        PhysNode::IndexRange {
            out,
            label,
            property,
            op,
            value,
        } => Box::new(source::IndexRangeOp::new(*out, label, property, *op, value)),
        PhysNode::TextIndexScan {
            out,
            label,
            property,
            kind,
            pattern,
        } => Box::new(source::TextIndexScanOp::new(
            *out, label, property, *kind, pattern,
        )),
        PhysNode::PointIndexScan {
            out,
            label,
            property,
            query,
        } => Box::new(source::PointIndexScanOp::new(*out, label, property, query)),
        PhysNode::EdgeTypeScan {
            from,
            rel,
            to,
            edge_types,
            direction,
        } => Box::new(source::EdgeTypeScanOp::new(
            *from, *rel, *to, edge_types, *direction,
        )),
        PhysNode::CountSource {
            kind,
            outs,
            out_group,
        } => Box::new(source::CountSourceOp::new(kind, outs, *out_group)),
        PhysNode::NoRows => Box::new(source::NoRowsOp),
        PhysNode::Flatten { input, group } => {
            Box::new(flow::FlattenOp::new(build_with(input, sub)?, *group))
        }
        PhysNode::Capture { input, positions } => {
            Box::new(flow::CaptureOp::new(build_with(input, sub)?, positions))
        }
        PhysNode::Filter { input, pred, group } => {
            Box::new(flow::FilterOp::new(build_with(input, sub)?, pred, *group))
        }
        PhysNode::AddColumn { input, expr, out } => {
            Box::new(flow::AddColumnOp::new(build_with(input, sub)?, expr, *out))
        }
        PhysNode::Project {
            input,
            items,
            moves,
            in_groups,
            out_group,
            distinct,
            where_,
        } => Box::new(project::ProjectOp::new(
            build_with(input, sub)?,
            items,
            moves,
            in_groups,
            *out_group,
            *distinct,
            where_.as_ref(),
        )),
        PhysNode::Expand {
            input,
            from,
            rel,
            to,
            out_group,
            dir,
            edge_types,
            prior_edges,
            bound_to,
            in_place,
        } => Box::new(expand::ExpandOp::new(
            build_with(input, sub)?,
            *from,
            *rel,
            *to,
            *out_group,
            *dir,
            edge_types,
            prior_edges,
            *bound_to,
            *in_place,
        )),
        PhysNode::ChainWeights {
            out,
            weight,
            out_group,
            from_label,
            hops,
            count_only,
            exact,
            exact_cols,
        } => Box::new(chain::ChainWeightsOp::new(
            *out,
            *weight,
            *out_group,
            from_label.as_deref(),
            hops,
            *count_only,
            build_with(exact, sub)?,
            *exact_cols,
        )),
        PhysNode::ExpandCount {
            input,
            from,
            out,
            dir,
            edge_types,
            to_label,
            prior_edges,
        } => Box::new(expand::ExpandCountOp::new(
            build_with(input, sub)?,
            *from,
            *out,
            *dir,
            edge_types,
            to_label.as_deref(),
            prior_edges,
        )),
        PhysNode::VarExpand {
            input,
            from,
            rel,
            to,
            out_group,
            dir,
            edge_types,
            min,
            max,
            prior_edges,
            bound_to,
        } => Box::new(expand::VarExpandOp::new(
            build_with(input, sub)?,
            *from,
            *rel,
            *to,
            *out_group,
            *dir,
            edge_types,
            *min,
            *max,
            prior_edges,
            *bound_to,
        )),
        PhysNode::DistinctExpand {
            input,
            from,
            to,
            out_group,
            min,
            max,
            dir,
            edge_types,
            target_label,
            global_dedup,
            multi_source,
            target_filter,
        } => Box::new(expand::DistinctExpandOp::new(
            build_with(input, sub)?,
            *from,
            *to,
            *out_group,
            *min,
            *max,
            *dir,
            edge_types,
            target_label.as_deref(),
            *global_dedup,
            multi_source.as_deref(),
            target_filter.as_ref(),
        )),
        PhysNode::Unwind {
            input,
            list,
            out,
            out_group,
        } => Box::new(flow::UnwindOp::new(
            build_with(input, sub)?,
            list,
            *out,
            *out_group,
        )),
        PhysNode::IndexLookupJoin {
            input,
            key,
            out,
            out_group,
            label,
            property,
        } => Box::new(expand::IndexLookupJoinOp::new(
            build_with(input, sub)?,
            key,
            *out,
            *out_group,
            label,
            property,
        )),
        PhysNode::VectorSearch {
            input,
            out,
            score,
            out_group,
            index_name,
            query,
            index_where,
            limit,
        } => Box::new(expand::VectorSearchOp::new(
            build_with(input, sub)?,
            *out,
            *score,
            *out_group,
            index_name,
            query,
            index_where.as_ref(),
            limit,
        )),
        PhysNode::Limit {
            input,
            count,
            in_groups,
            materialize,
            consume,
        } => Box::new(flow::LimitOp::new(
            build_with(input, sub)?,
            count,
            in_groups,
            *materialize,
            consume.as_ref(),
        )),
        PhysNode::Skip {
            input,
            count,
            in_groups,
            consume,
        } => Box::new(flow::SkipOp::new(
            build_with(input, sub)?,
            count,
            in_groups,
            consume.as_ref(),
        )),
        PhysNode::Sort {
            input,
            keys,
            in_groups,
            out_group,
            moves,
            limit,
        } => Box::new(sort::SortOp::new(
            build_with(input, sub)?,
            keys,
            in_groups,
            *out_group,
            moves,
            limit.as_ref(),
        )),
        PhysNode::Aggregate {
            input,
            keys,
            aggs,
            post,
            in_groups,
            out_group,
        } => Box::new(aggregate::AggregateOp::new(
            build_with(input, sub)?,
            keys,
            aggs,
            post,
            in_groups,
            *out_group,
        )),
        PhysNode::Eager {
            input,
            in_groups,
            out_group,
            moves,
        } => Box::new(flow::EagerOp::new(
            build_with(input, sub)?,
            in_groups,
            *out_group,
            moves,
        )),
        PhysNode::Union {
            left,
            right,
            all,
            out_group,
            moves_left,
            moves_right,
            left_groups,
            right_groups,
        } => Box::new(flow::UnionOp::new(
            build_with(left, sub)?,
            build_with(right, sub)?,
            *all,
            *out_group,
            moves_left,
            moves_right,
            left_groups,
            right_groups,
        )),
        PhysNode::CrossProduct {
            left,
            right,
            left_groups,
            right_groups,
            left_out,
            moves_left,
            right_out,
            moves_right,
        } => Box::new(join::CrossProductOp::new(
            build_with(left, sub)?,
            build_with(right, sub)?,
            left_groups,
            right_groups,
            *left_out,
            moves_left,
            *right_out,
            moves_right,
        )),
        PhysNode::HashJoin {
            left,
            right,
            left_keys,
            right_keys,
            left_groups,
            right_groups,
            out_group,
            moves_left,
            moves_right,
        } => match sub.take_probe(node) {
            Some(table) => Box::new(join::HashJoinOp::probe(
                build_with(right, sub)?,
                table,
                right_keys,
                right_groups,
                *out_group,
                moves_left,
                moves_right,
            )),
            None => Box::new(join::HashJoinOp::new(
                build_with(left, sub)?,
                build_with(right, sub)?,
                left_keys,
                right_keys,
                left_groups,
                right_groups,
                *out_group,
                moves_left,
                moves_right,
            )),
        },
        PhysNode::OptionalMatch {
            input,
            body,
            in_groups,
            out_group,
            moves,
            body_out,
        } => Box::new(join::OptionalMatchOp::new(
            build_with(input, sub)?,
            body,
            in_groups,
            *out_group,
            moves,
            body_out,
        )?),
        PhysNode::BindPath {
            input,
            out,
            node_vars,
            edge_vars,
        } => Box::new(bind::BindPathOp::new(
            build_with(input, sub)?,
            *out,
            node_vars,
            edge_vars,
        )),
        PhysNode::BindQppGroups {
            input,
            anchor,
            rel,
            src,
            dst,
        } => Box::new(bind::BindQppGroupsOp::new(
            build_with(input, sub)?,
            *anchor,
            *rel,
            *src,
            *dst,
        )),
        PhysNode::BindVarLists { input, groups } => {
            Box::new(bind::BindVarListsOp::new(build_with(input, sub)?, groups))
        }
        PhysNode::BindPathEndpoints {
            input,
            rel,
            from,
            to,
            direction,
            out_group,
            in_groups,
            moves,
            from_bound,
            to_bound,
        } => Box::new(bind::BindPathEndpointsOp::new(
            build_with(input, sub)?,
            *rel,
            *from,
            *to,
            direction.clone(),
            *out_group,
            in_groups,
            moves,
            *from_bound,
            *to_bound,
        )),
        PhysNode::BindEdgeEndpoints {
            input,
            edge,
            from,
            to,
            direction,
            edge_types,
            out_group,
            in_groups,
            moves,
            from_bound,
            to_bound,
        } => Box::new(bind::BindEdgeEndpointsOp::new(
            build_with(input, sub)?,
            *edge,
            *from,
            *to,
            direction.clone(),
            edge_types,
            *out_group,
            in_groups,
            moves,
            *from_bound,
            *to_bound,
        )),
        PhysNode::CallProcedure {
            input,
            name,
            args,
            yield_columns,
            in_groups,
            out_group,
            in_layout,
            out_layout,
        } => Box::new(call::CallProcedureOp::new(
            build_with(input, sub)?,
            name,
            args.as_ref(),
            yield_columns,
            in_groups,
            *out_group,
            in_layout,
            out_layout,
        )),
        PhysNode::CallSubquery {
            input,
            body,
            in_groups,
            out_group,
            moves,
            body_out,
        } => Box::new(subquery::CallSubqueryOp::new(
            build_with(input, sub)?,
            body,
            in_groups,
            *out_group,
            moves,
            body_out,
        )?),
        PhysNode::CallInTransactions {
            input,
            body,
            batch_size,
            on_error,
            report_status,
            in_groups,
            out_group,
            moves,
            body_out,
        } => Box::new(subquery::CallInTransactionsOp::new(
            build_with(input, sub)?,
            body,
            batch_size.as_ref(),
            *on_error,
            *report_status,
            in_groups,
            *out_group,
            moves,
            body_out,
        )?),
        PhysNode::ForEach {
            input,
            list,
            body,
            in_groups,
            out_group,
            moves,
        } => Box::new(subquery::ForEachOp::new(
            build_with(input, sub)?,
            list,
            body,
            in_groups,
            *out_group,
            moves,
        )?),
        PhysNode::ShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            min_depth,
            max_depth,
            path,
            rel,
            all,
            k,
            k_groups,
            out_group,
        } => Box::new(path::ShortestPathOp::new(
            build_with(input, sub)?,
            *from,
            *to,
            edge_types,
            direction.clone(),
            *min_depth,
            *max_depth,
            *path,
            *rel,
            *all,
            *k,
            *k_groups,
            *out_group,
        )),
        PhysNode::WeightedShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path,
            rel,
            cost,
            out_group,
        } => Box::new(path::WeightedShortestPathOp::new(
            build_with(input, sub)?,
            *from,
            *to,
            edge_types,
            direction.clone(),
            weight_property,
            *path,
            *rel,
            *cost,
            *out_group,
        )),
        PhysNode::LoadCsv {
            input,
            url,
            with_headers,
            row,
            ctx,
            out_group,
            field_terminator,
        } => Box::new(load_csv::LoadCsvOp::new(
            build_with(input, sub)?,
            url,
            *with_headers,
            *row,
            *ctx,
            *out_group,
            *field_terminator,
        )),
        PhysNode::CreateNode {
            input,
            out,
            labels,
            dynamic_labels,
            properties,
            in_groups,
            out_group,
            moves,
        } => Box::new(write::CreateNodeOp::new(
            build_with(input, sub)?,
            *out,
            labels,
            dynamic_labels,
            properties.as_ref(),
            in_groups,
            *out_group,
            moves,
        )),
        PhysNode::CreateRel {
            input,
            out,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
            in_groups,
            out_group,
            moves,
        } => Box::new(write::CreateRelOp::new(
            build_with(input, sub)?,
            *out,
            *from,
            *to,
            edge_type,
            dynamic_edge_type.as_ref(),
            properties.as_ref(),
            in_groups,
            *out_group,
            moves,
        )),
        PhysNode::SetProps {
            input,
            items,
            in_groups,
            out_group,
            moves,
        } => Box::new(write::SetPropsOp::new(
            build_with(input, sub)?,
            items,
            in_groups,
            *out_group,
            moves,
        )),
        PhysNode::Remove {
            input,
            items,
            in_groups,
            out_group,
            moves,
        } => Box::new(write::RemoveOp::new(
            build_with(input, sub)?,
            items,
            in_groups,
            *out_group,
            moves,
        )),
        PhysNode::Delete {
            input,
            exprs,
            detach,
            in_groups,
            out_group,
            moves,
        } => Box::new(write::DeleteOp::new(
            build_with(input, sub)?,
            exprs,
            *detach,
            in_groups,
            *out_group,
            moves,
        )),
        PhysNode::Merge {
            input,
            pattern,
            on_create,
            on_match,
            binds,
            out_layout,
            in_groups,
            out_group,
            moves,
        } => Box::new(write::MergeOp::new(
            build_with(input, sub)?,
            pattern,
            on_create.as_deref(),
            on_match.as_deref(),
            binds,
            out_layout,
            in_groups,
            *out_group,
            moves,
        )?),
        PhysNode::Schema { op, group } => Box::new(schema::SchemaOpExec::new(op, *group)),
        PhysNode::Show {
            op,
            cols,
            out_group,
        } => Box::new(schema::ShowOpExec::new(*op, cols, *out_group)),
        PhysNode::TransactionCommand {
            input,
            command,
            ids,
            cols,
            in_groups,
            out_group,
            moves,
        } => Box::new(schema::TransactionCommandOp::new(
            build_with(input, sub)?,
            *command,
            ids.as_ref(),
            cols,
            in_groups,
            *out_group,
            moves,
        )),
    })
}

/// Short name of a physical node, for diagnostics.
pub fn node_name(node: &PhysNode) -> &'static str {
    match node {
        PhysNode::EmptyRow { .. } => "EmptyRow",
        PhysNode::Seed { .. } => "Seed",
        PhysNode::NodeScan { .. } => "NodeScan",
        PhysNode::NodeByIdSeek { .. } => "NodeByIdSeek",
        PhysNode::NodeByIdRange { .. } => "NodeByIdRange",
        PhysNode::IndexSeek { .. } => "IndexSeek",
        PhysNode::IndexRange { .. } => "IndexRange",
        PhysNode::TextIndexScan { .. } => "TextIndexScan",
        PhysNode::PointIndexScan { .. } => "PointIndexScan",
        PhysNode::EdgeTypeScan { .. } => "EdgeTypeScan",
        PhysNode::Nest { .. } => "Nest",
        PhysNode::CountSource { .. } => "CountSource",
        PhysNode::Flatten { .. } => "Flatten",
        PhysNode::Capture { .. } => "Capture",
        PhysNode::Filter { .. } => "Filter",
        PhysNode::AddColumn { .. } => "AddColumn",
        PhysNode::Project { .. } => "Project",
        PhysNode::Expand { .. } => "Expand",
        PhysNode::ExpandCount { .. } => "ExpandCount",
        PhysNode::ChainWeights { .. } => "ChainWeights",
        PhysNode::VarExpand { .. } => "VarExpand",
        PhysNode::DistinctExpand { .. } => "DistinctExpand",
        PhysNode::Unwind { .. } => "Unwind",
        PhysNode::IndexLookupJoin { .. } => "IndexLookupJoin",
        PhysNode::VectorSearch { .. } => "VectorSearch",
        PhysNode::Limit { .. } => "Limit",
        PhysNode::Skip { .. } => "Skip",
        PhysNode::Sort { .. } => "Sort",
        PhysNode::Aggregate { .. } => "Aggregate",
        PhysNode::Eager { .. } => "Eager",
        PhysNode::Union { .. } => "Union",
        PhysNode::CrossProduct { .. } => "CrossProduct",
        PhysNode::HashJoin { .. } => "HashJoin",
        PhysNode::OptionalMatch { .. } => "OptionalMatch",
        PhysNode::BindPath { .. } => "BindPath",
        PhysNode::BindQppGroups { .. } => "BindQppGroups",
        PhysNode::BindVarLists { .. } => "BindVarLists",
        PhysNode::BindPathEndpoints { .. } => "BindPathEndpoints",
        PhysNode::BindEdgeEndpoints { .. } => "BindEdgeEndpoints",
        PhysNode::CallProcedure { .. } => "CallProcedure",
        PhysNode::CallSubquery { .. } => "CallSubquery",
        PhysNode::CallInTransactions { .. } => "CallInTransactions",
        PhysNode::ForEach { .. } => "ForEach",
        PhysNode::ShortestPath { .. } => "ShortestPath",
        PhysNode::WeightedShortestPath { .. } => "WeightedShortestPath",
        PhysNode::LoadCsv { .. } => "LoadCsv",
        PhysNode::CreateNode { .. } => "CreateNode",
        PhysNode::CreateRel { .. } => "CreateRel",
        PhysNode::SetProps { .. } => "SetProps",
        PhysNode::Remove { .. } => "Remove",
        PhysNode::Delete { .. } => "Delete",
        PhysNode::Merge { .. } => "Merge",
        PhysNode::Schema { .. } => "Schema",
        PhysNode::Show { .. } => "Show",
        PhysNode::TransactionCommand { .. } => "TransactionCommand",
        PhysNode::NoRows => "NoRows",
    }
}

// -- shared helpers ---------------------------------------------------------------

/// Installs `cols` as the columns of `group` with `len` lanes (every other column of the group
/// becomes nulls), resetting selection and flattening.
pub(crate) fn install(rs: &mut ResultSet, group: GroupId, len: usize, cols: Vec<(u16, Vector)>) {
    let g = rs.group_mut(group);
    let width = g.cols.len();
    let mut filled = vec![false; width];
    for (c, v) in cols {
        debug_assert_eq!(v.len(), len);
        g.cols[c as usize] = v;
        filled[c as usize] = true;
    }
    for (c, ok) in filled.iter().enumerate() {
        if !ok {
            g.cols[c] = Vector::nulls(len);
        }
    }
    g.reset(len);
}

/// Rows materialized column by column, charged to the query's memory budget, emitted in chunks.
pub(crate) struct Staged {
    cols: Vec<VectorBuilder>,
    rows: usize,
    charge: Charge,
    bytes_since_charge: usize,
    finished: Option<Vec<Vector>>,
    emit_pos: usize,
}

impl Staged {
    /// `width` columns.
    pub(crate) fn new(width: usize) -> Self {
        Self {
            cols: (0..width).map(|_| VectorBuilder::new()).collect(),
            rows: 0,
            charge: Charge::new(),
            bytes_since_charge: 0,
            finished: None,
            emit_pos: 0,
        }
    }

    /// Rows staged.
    pub(crate) fn len(&self) -> usize {
        self.rows
    }

    /// No rows?
    pub(crate) fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Appends one row: `f` pushes exactly one value per column, in order.
    #[inline]
    pub(crate) fn push_row<F: FnOnce(&mut [VectorBuilder])>(
        &mut self,
        f: F,
    ) -> Result<(), CypherError> {
        f(&mut self.cols);
        self.rows += 1;
        // an estimate per row: a word per column, plus the heap of strings and values is counted
        // when the chunk finishes
        self.bytes_since_charge += 16 * self.cols.len().max(1);
        if self.bytes_since_charge >= 256 * 1024 {
            self.charge.add(self.bytes_since_charge)?;
            self.bytes_since_charge = 0;
        }
        Ok(())
    }

    /// Appends every row of the current chunk of `rs`, copying `moves` (source -> output col).
    pub(crate) fn push_moves(
        &mut self,
        rs: &ResultSet,
        in_groups: &[GroupId],
        moves: &[(DataPos, u16)],
    ) -> Result<(), CypherError> {
        rs.for_each_row(in_groups, |lanes| {
            self.push_row(|cols| {
                for (src, c) in moves {
                    cols[*c as usize].push(rs.value_at(*src, lanes));
                }
            })
        })
    }

    /// Finishes staging; rows are then emitted with [`emit`](Self::emit).
    pub(crate) fn finish(&mut self) -> Result<(), CypherError> {
        if self.finished.is_none() {
            // a column nothing moved into (bound later in the group, by an expansion in place
            // for one) holds nulls for every row
            let rows = self.rows;
            let cols: Vec<Vector> = std::mem::take(&mut self.cols)
                .into_iter()
                .map(|b| {
                    let v = b.finish();
                    if v.is_empty() && rows > 0 {
                        Vector::nulls(rows)
                    } else {
                        v
                    }
                })
                .collect();
            let bytes: usize = cols.iter().map(Vector::heap_bytes).sum();
            self.charge
                .add(bytes.saturating_sub(self.rows * 16 * cols.len().max(1)))?;
            self.charge.flush()?;
            self.finished = Some(cols);
        }
        Ok(())
    }

    /// Emits the next chunk of staged rows into `group` (columns in staged order); `false` when
    /// everything was emitted.
    pub(crate) fn emit(&mut self, rs: &mut ResultSet, group: GroupId) -> Result<bool, CypherError> {
        self.finish()?;
        let cols = self.finished.as_ref().unwrap();
        if self.emit_pos >= self.rows {
            return Ok(false);
        }
        let end = (self.emit_pos + VECTOR_CAPACITY).min(self.rows);
        let sel: Vec<u32> = (self.emit_pos as u32..end as u32).collect();
        let out: Vec<(u16, Vector)> = cols
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, v.take(&sel)))
            .collect();
        install(rs, group, end - self.emit_pos, out);
        self.emit_pos = end;
        Ok(true)
    }

    /// Emits rows `[start, end)` of the staged rows into `group`.
    pub(crate) fn emit_range(
        &mut self,
        rs: &mut ResultSet,
        group: GroupId,
        start: usize,
        end: usize,
    ) -> Result<(), CypherError> {
        self.finish()?;
        let cols = self.finished.as_ref().unwrap();
        let sel: Vec<u32> = (start as u32..end as u32).collect();
        let out: Vec<(u16, Vector)> = cols
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, v.take(&sel)))
            .collect();
        install(rs, group, end - start, out);
        Ok(())
    }

    /// Emits the staged rows in the order `order` gives (indices into the rows), chunk by chunk.
    pub(crate) fn emit_ordered(
        &mut self,
        rs: &mut ResultSet,
        group: GroupId,
        order: &[u32],
    ) -> Result<bool, CypherError> {
        self.finish()?;
        let cols = self.finished.as_ref().unwrap();
        if self.emit_pos >= order.len() {
            return Ok(false);
        }
        let end = (self.emit_pos + VECTOR_CAPACITY).min(order.len());
        let sel = &order[self.emit_pos..end];
        let out: Vec<(u16, Vector)> = cols
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, v.take(sel)))
            .collect();
        install(rs, group, end - self.emit_pos, out);
        self.emit_pos = end;
        Ok(true)
    }

    /// The value at row `row`, column `c` (after finishing).
    pub(crate) fn value(&self, row: usize, c: usize) -> ValueRef<'_> {
        self.finished.as_ref().unwrap()[c].get(row)
    }

    /// The staged rows as columns.
    pub(crate) fn into_cols(mut self) -> Result<Vec<Vector>, CypherError> {
        self.finish()?;
        Ok(self.finished.take().unwrap_or_default())
    }
}

/// The moves of a row consumer as `(source, output column)` pairs.
pub(crate) fn move_cols(moves: &[(DataPos, DataPos)]) -> Vec<(DataPos, u16)> {
    moves.iter().map(|(s, d)| (*s, d.col)).collect()
}

/// Collects `(id, source, target)` of the edges in `v` (an edge or a list of them) into `out`.
pub(crate) fn edge_ends_of(
    v: ValueRef<'_>,
    out: &mut Vec<(
        traverse_core::graph::types::EdgeId,
        traverse_core::graph::types::NodeId,
        traverse_core::graph::types::NodeId,
    )>,
) {
    match v {
        ValueRef::Edge { id, src, tgt, .. } => out.push((id, src, tgt)),
        ValueRef::Any(cv) => ends_in(cv, out),
        _ => {}
    }
}

fn ends_in(
    cv: &CypherValue,
    out: &mut Vec<(
        traverse_core::graph::types::EdgeId,
        traverse_core::graph::types::NodeId,
        traverse_core::graph::types::NodeId,
    )>,
) {
    match cv {
        CypherValue::Edge {
            id, source, target, ..
        }
        | CypherValue::EdgeRef {
            id, source, target, ..
        } => out.push((*id, *source, *target)),
        CypherValue::List(items) => {
            for it in items {
                ends_in(it, out);
            }
        }
        _ => {}
    }
}

/// Edge ids held by a value: a relationship, or a list of relationships.
pub(crate) fn edge_ids_of(v: ValueRef<'_>, out: &mut Vec<traverse_core::graph::types::EdgeId>) {
    match v {
        ValueRef::Edge { id, .. } => out.push(id),
        ValueRef::Any(CypherValue::Edge { id, .. }) => out.push(*id),
        ValueRef::Any(CypherValue::EdgeRef { id, .. }) => out.push(*id),
        ValueRef::Any(CypherValue::List(items)) => {
            for it in items {
                if let CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } = it {
                    out.push(*id);
                }
            }
        }
        _ => {}
    }
}

/// `LIMIT` / `SKIP` count: a non-negative integer.
pub(crate) fn eval_count(
    expr: &super::expr::CompiledExpr,
    rs: &ResultSet,
    cx: &ExecCtx<'_>,
    what: &str,
) -> Result<usize, CypherError> {
    let mut sc = super::expr::EvalScratch::new();
    let v = expr.eval_scalar(rs, cx.view(), &mut sc)?;
    match v {
        CypherValue::Integer(n) if n >= 0 => Ok(n as usize),
        CypherValue::Integer(_) => Err(CypherError::semantic(format!(
            "{what} must be a non-negative integer"
        ))
        .with_status(crate::status::GqlStatus::NegativeValue)),
        other => Err(CypherError::type_error(format!(
            "expected integer, got {}",
            other.type_name()
        ))),
    }
}
