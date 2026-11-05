//! `LogicalPlan` -> [`PhysicalPlan`]: variables to positions, groups and flattening, access
//! paths, subquery seeding.
//!
//! The lowering walks the logical plan bottom-up carrying a [`Layout`]. Sources open a new
//! group; per-row producers (expansion, unwind, lookups) flatten the groups they read from and
//! open a new group for what they produce; row consumers (projection with expressions,
//! aggregation, sort, joins, writes) take the cross product of the live groups and place their
//! outputs in one fresh flat group. A plan run per outer row (OPTIONAL MATCH, CALL subqueries,
//! subquery expressions, FOREACH, batches) is lowered with the outer variables placed in a seed
//! group; its first source nests under the seed.

use std::collections::HashSet;
use std::sync::Arc;

use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use crate::ast::{CmpOp, Expr, Literal, MatchClause, ProjectionItem, RelDirection, SingleQuery};
use crate::error::CypherError;
use crate::planner::{IndexHints, LogicalPlan, SpatialQuery, is_aggregate_function};

use super::super::chunk::{DataPos, GroupId};
use super::super::expr::compile::SubqueryLowerer;
use super::super::expr::{CompiledExpr, compile_expr_with};
use super::super::layout::{GroupAlloc, Layout, SlotType};
use super::super::ops::load_csv::CSV_CTX_SLOT;
use super::{
    AggSpec, Consume, CountKind, PhysNode, PhysSpatialQuery, PhysicalPlan, PostAgg, RemoveAction,
    SchemaOp, SeedSource, SetAction, ShowOp, SubPlan,
};
use super::{ChainHop, NodeSeek};

/// Compiles one expression against `layout` with full subquery support, for callers outside a
/// plan (the procedure boundary).
pub fn compile_expr_full(
    expr: &Expr,
    layout: &Layout,
    graph: &dyn GraphView,
) -> Result<CompiledExpr, CypherError> {
    let hints = crate::build_index_hints_for(graph);
    let mut sub = SubLowerer {
        graph,
        hints: &hints,
    };
    compile_expr_with(expr, layout, &mut sub)
}

/// Compiles a logical plan.
pub fn compile(plan: &LogicalPlan, graph: &dyn GraphView) -> Result<PhysicalPlan, CypherError> {
    let mut l = Lowerer::new(graph);
    let out = l.lower(plan, Layout::new())?;
    l.finish(plan, out)
}

/// Compiles a logical plan whose rows are joined with the rows of `seed`: the seed variables
/// are available at the seed group's positions.
pub fn compile_with_seeds(
    plan: &LogicalPlan,
    seed: &Layout,
    graph: &dyn GraphView,
) -> Result<PhysicalPlan, CypherError> {
    let mut l = Lowerer::new(graph);
    // the seed group is group 0 of the plan's own result set
    let g = l.alloc.new_group();
    let mut layout = Layout::new();
    let mut cols = Vec::new();
    for (name, _, ty) in seed.bindings() {
        let pos = layout.bind_in(&mut l.alloc, name, g, ty);
        cols.push(pos);
    }
    l.pending_base = Some(PhysNode::Seed { group: g, cols });
    let out = l.lower(plan, layout)?;
    l.finish(plan, out)
}

/// A lowered subtree: its operator and the layout after it.
struct Lowered {
    node: PhysNode,
    layout: Layout,
}

/// Lowering state of one plan.
struct Lowerer<'g> {
    graph: &'g dyn GraphView,
    alloc: GroupAlloc,
    hints: IndexHints,
    /// A seed source the first leaf nests under.
    pending_base: Option<PhysNode>,
}

impl<'g> Lowerer<'g> {
    fn new(graph: &'g dyn GraphView) -> Self {
        Self {
            graph,
            alloc: GroupAlloc::new(),
            hints: crate::build_index_hints_for(graph),
            pending_base: None,
        }
    }

    fn finish(self, plan: &LogicalPlan, out: Lowered) -> Result<PhysicalPlan, CypherError> {
        let mut columns = crate::executor::plan_columns(plan);
        if columns.is_empty()
            && let LogicalPlan::Project { input, .. } = plan
        {
            columns = crate::executor::plan_visible_columns(input);
        }
        if columns.is_empty() && crate::executor::plan_has_projection(plan) {
            // RETURN * with no statically known columns: every visible variable
            columns = out
                .layout
                .names()
                .filter(|n| !n.starts_with("__"))
                .map(str::to_string)
                .collect();
        }
        let columns: Vec<(String, DataPos)> = columns
            .into_iter()
            .map(|name| {
                let pos = out
                    .layout
                    .pos(&name)
                    .unwrap_or(DataPos::new(u16::MAX, u16::MAX));
                (name, pos)
            })
            .collect();
        let out_groups = out.layout.groups();
        let depth = super::node_depth(&out.node);
        Ok(PhysicalPlan {
            root: out.node,
            widths: self.alloc.widths().to_vec(),
            output: out.layout,
            columns,
            out_groups,
            mutating: crate::executor::plan_is_mutating(plan),
            schema: crate::executor::plan_is_schema(plan),
            has_projection: crate::executor::plan_has_projection(plan),
            depth,
        })
    }

    // -- helpers ----------------------------------------------------------------

    fn compile(&mut self, expr: &Expr, layout: &Layout) -> Result<CompiledExpr, CypherError> {
        let mut sub = SubLowerer {
            graph: self.graph,
            hints: &self.hints,
        };
        compile_expr_with(expr, layout, &mut sub)
    }

    /// Wraps `node` in a `Flatten` per group of `groups` (ascending), marking them flat.
    fn flatten(&mut self, mut l: Lowered, groups: &[GroupId]) -> Lowered {
        for &g in groups {
            if l.layout.is_flat(g) {
                continue;
            }
            l.node = PhysNode::Flatten {
                input: Box::new(l.node),
                group: g,
            };
            l.layout.mark_flat(g);
        }
        l
    }

    /// Flattens every unflat group `positions` read from.
    fn flatten_for(&mut self, l: Lowered, positions: &[DataPos]) -> Lowered {
        let groups = l.layout.unflat_among(positions);
        self.flatten(l, &groups)
    }

    /// Flattens all but one of the unflat groups `positions` read from: the remaining one is the
    /// evaluation group (`None` when every referenced group is flat).
    fn eval_group_for(&mut self, l: Lowered, positions: &[DataPos]) -> (Lowered, Option<GroupId>) {
        let groups = l.layout.unflat_among(positions);
        if groups.is_empty() {
            return (l, None);
        }
        let keep = *groups.last().unwrap();
        let l = self.flatten(l, &groups[..groups.len() - 1]);
        (l, Some(keep))
    }

    /// Opens the flat output group of a row consumer: `(in_groups, out_group, out_layout, moves)`.
    fn consume(
        &mut self,
        layout: &Layout,
    ) -> (Vec<GroupId>, GroupId, Layout, Vec<(DataPos, DataPos)>) {
        let in_groups = layout.groups();
        let out_group = self.alloc.new_group();
        let (out_layout, moves) = layout.flattened_into(&mut self.alloc, out_group);
        (in_groups, out_group, out_layout, moves)
    }

    /// A source-rooted node, nested under the pending seed base when there is one.
    fn source(&mut self, node: PhysNode) -> PhysNode {
        match self.pending_base.take() {
            Some(base) => PhysNode::Nest {
                input: Box::new(base),
                inner: Box::new(node),
            },
            None => node,
        }
    }

    /// Records the entity-typed positions of `layout` as bound below a projection boundary.
    /// Wraps a projection boundary's input in a bound-entity capture of its entity columns.
    fn capture(&self, l: Lowered) -> Lowered {
        let mut positions: Vec<DataPos> = l
            .layout
            .bindings()
            .filter(|(name, _, ty)| {
                !name.starts_with("__")
                    && matches!(ty, SlotType::Node | SlotType::Edge | SlotType::Any)
            })
            .map(|(_, pos, _)| pos)
            .collect();
        positions.sort();
        positions.dedup();
        if positions.is_empty() {
            return l;
        }
        Lowered {
            node: PhysNode::Capture {
                input: Box::new(l.node),
                positions,
            },
            layout: l.layout,
        }
    }

    /// A `has_label` predicate on a bound variable.
    fn label_filter(&mut self, l: Lowered, var: &str, label: &str) -> Result<Lowered, CypherError> {
        let pred = crate::planner::make_has_label_filter(var, label);
        self.lower_filter(l, &pred)
    }

    fn lower_filter(&mut self, l: Lowered, pred: &Expr) -> Result<Lowered, CypherError> {
        if matches!(pred, Expr::Literal(Literal::Bool(false))) {
            return Ok(Lowered {
                node: PhysNode::NoRows,
                layout: l.layout,
            });
        }
        if matches!(pred, Expr::Literal(Literal::Bool(true))) {
            return Ok(l);
        }
        let compiled = self.compile(pred, &l.layout)?;
        let (l, group) = self.eval_group_for(l, &compiled.positions());
        Ok(Lowered {
            node: PhysNode::Filter {
                input: Box::new(l.node),
                pred: compiled,
                group,
            },
            layout: l.layout,
        })
    }

    // -- the walk ------------------------------------------------------------------

    #[recursive::recursive]
    fn lower(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        match plan {
            LogicalPlan::EmptyRow | LogicalPlan::ContextSeed => self.arm_empty_row(plan, layout),
            LogicalPlan::Scan { .. } => self.arm_scan(plan, layout),
            LogicalPlan::NodeById { .. } => self.arm_node_by_id(plan, layout),
            LogicalPlan::NodeByIdRange { .. } => self.arm_node_by_id_range(plan, layout),
            LogicalPlan::IndexScan { .. } => self.arm_index_scan(plan, layout),
            LogicalPlan::IndexRangeScan { .. } => self.arm_index_range_scan(plan, layout),
            LogicalPlan::TextIndexScan { .. } => self.arm_text_index_scan(plan, layout),
            LogicalPlan::PointIndexScan { .. } => self.arm_point_index_scan(plan, layout),
            LogicalPlan::EdgeTypeScan { .. } => self.arm_edge_type_scan(plan, layout),
            LogicalPlan::ScanFilterProject { .. } => self.arm_scan_filter_project(plan, layout),
            LogicalPlan::Filter { .. } => self.arm_filter(plan, layout),
            LogicalPlan::Project { .. } => self.arm_project(plan, layout),
            LogicalPlan::Aggregate { .. } => self.arm_aggregate(plan, layout),
            LogicalPlan::Sort { .. } => self.arm_sort(plan, layout),
            LogicalPlan::Limit { .. } => self.arm_limit(plan, layout),
            LogicalPlan::Skip { .. } => self.arm_skip(plan, layout),
            LogicalPlan::Eager { .. } => self.arm_eager(plan, layout),
            LogicalPlan::Union { .. } => self.arm_union(plan, layout),
            LogicalPlan::CrossProduct { .. } => self.arm_cross_product(plan, layout),
            LogicalPlan::HashJoin { .. } => self.arm_hash_join(plan, layout),
            LogicalPlan::OptionalMatch { .. } => self.arm_optional_match(plan, layout),
            LogicalPlan::ExpandOut { .. }
            | LogicalPlan::ExpandIn { .. }
            | LogicalPlan::ExpandBoth { .. } => self.lower_expand(plan, layout),
            LogicalPlan::DistinctExpand { .. } => self.arm_distinct_expand(plan, layout),
            LogicalPlan::Unwind { .. } => self.arm_unwind(plan, layout),
            LogicalPlan::IndexLookupJoin { .. } => self.arm_index_lookup_join(plan, layout),
            LogicalPlan::VectorSearchScan { .. } => self.arm_vector_search_scan(plan, layout),
            LogicalPlan::BindPath { .. } => self.arm_bind_path(plan, layout),
            LogicalPlan::BindQppGroups { .. } => self.arm_bind_qpp_groups(plan, layout),
            LogicalPlan::BindVarLists { .. } => self.arm_bind_var_lists(plan, layout),
            LogicalPlan::BindPathEndpoints { .. } => self.arm_bind_path_endpoints(plan, layout),
            LogicalPlan::BindEdgeEndpoints { .. } => self.arm_bind_edge_endpoints(plan, layout),
            LogicalPlan::CallProcedure { .. } => self.arm_call_procedure(plan, layout),
            LogicalPlan::CallSubquery { .. } => self.arm_call_subquery(plan, layout),
            LogicalPlan::CallInTransactions { .. } => self.arm_call_in_transactions(plan, layout),
            LogicalPlan::ForEach { .. } => self.arm_for_each(plan, layout),
            LogicalPlan::ShortestPath { .. } => self.arm_shortest_path(plan, layout),
            LogicalPlan::WeightedShortestPath { .. } => {
                self.arm_weighted_shortest_path(plan, layout)
            }
            LogicalPlan::LoadCsv { .. } => self.arm_load_csv(plan, layout),
            LogicalPlan::CreateNode { .. } => self.arm_create_node(plan, layout),
            LogicalPlan::CreateRel { .. } => self.arm_create_rel(plan, layout),
            LogicalPlan::SetProperty { .. } => self.arm_set_property(plan, layout),
            LogicalPlan::RemoveOp { .. } => self.arm_remove_op(plan, layout),
            LogicalPlan::Delete { .. } => self.arm_delete(plan, layout),
            LogicalPlan::Merge { .. } => self.arm_merge(plan, layout),
            LogicalPlan::CreateIndex {
                name,
                label,
                property,
                extra_properties,
                ..
            } => self.schema(
                SchemaOp::CreateIndex {
                    name: name.clone(),
                    label: label.clone(),
                    property: property.clone(),
                    extra_properties: extra_properties.clone(),
                },
                layout,
            ),
            LogicalPlan::CreateVectorIndex {
                name,
                label,
                property,
                dimensions,
                similarity,
                quantization,
                ..
            } => self.schema(
                SchemaOp::CreateVectorIndex {
                    name: name.clone(),
                    label: label.clone(),
                    property: property.clone(),
                    dimensions: *dimensions,
                    similarity: similarity.clone(),
                    quantization: *quantization,
                },
                layout,
            ),
            LogicalPlan::CreateFulltextIndex {
                name,
                labels,
                properties,
                analyzer,
                ..
            } => self.schema(
                SchemaOp::CreateFulltextIndex {
                    name: name.clone(),
                    labels: labels.clone(),
                    properties: properties.clone(),
                    analyzer: analyzer.clone(),
                },
                layout,
            ),
            LogicalPlan::CreatePointIndex {
                name,
                label,
                property,
                ..
            } => self.schema(
                SchemaOp::CreatePointIndex {
                    name: name.clone(),
                    label: label.clone(),
                    property: property.clone(),
                },
                layout,
            ),
            LogicalPlan::CreateTextIndex {
                name,
                label,
                property,
                ..
            } => self.schema(
                SchemaOp::CreateTextIndex {
                    name: name.clone(),
                    label: label.clone(),
                    property: property.clone(),
                },
                layout,
            ),
            LogicalPlan::DropIndex { .. } => self.arm_drop_index(plan, layout),
            LogicalPlan::AnalyzeGraph { labels, .. } => self.schema(
                SchemaOp::AnalyzeGraph {
                    labels: labels.clone(),
                },
                layout,
            ),
            LogicalPlan::CreateConstraint {
                name,
                label,
                property,
                extra_properties,
                constraint_type,
                is_relationship,
                if_not_exists,
                ..
            } => self.schema(
                SchemaOp::CreateConstraint {
                    name: name.clone(),
                    label: label.clone(),
                    property: property.clone(),
                    extra_properties: extra_properties.clone(),
                    constraint_type: constraint_type.clone(),
                    is_relationship: *is_relationship,
                    if_not_exists: *if_not_exists,
                },
                layout,
            ),
            LogicalPlan::DropConstraint { .. } => self.arm_drop_constraint(plan, layout),
            LogicalPlan::CreateEdgeIndex {
                name,
                edge_type,
                property,
                extra_properties,
                ..
            } => self.schema(
                SchemaOp::CreateEdgeIndex {
                    name: name.clone(),
                    edge_type: edge_type.clone(),
                    property: property.clone(),
                    extra_properties: extra_properties.clone(),
                },
                layout,
            ),
            LogicalPlan::DropEdgeIndex { .. } => self.arm_drop_edge_index(plan, layout),
            LogicalPlan::ShowIndexes => self.show(ShowOp::Indexes, layout),
            LogicalPlan::ShowConstraints => self.show(ShowOp::Constraints, layout),
            LogicalPlan::ShowFunctions => self.show(ShowOp::Functions, layout),
            LogicalPlan::ShowProcedures => self.show(ShowOp::Procedures, layout),
            LogicalPlan::ShowSettings => self.show(ShowOp::Settings, layout),
            LogicalPlan::TransactionCommand { .. } => self.arm_transaction_command(plan, layout),
        }
    }

    fn arm_empty_row(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let (LogicalPlan::EmptyRow | LogicalPlan::ContextSeed) = plan else {
            unreachable!()
        };
        if let Some(base) = self.pending_base.take() {
            return Ok(Lowered { node: base, layout });
        }
        let g = self.alloc.new_group();
        Ok(Lowered {
            node: PhysNode::EmptyRow { group: g },
            layout,
        })
    }

    fn arm_scan(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Scan { variable, label } = plan else {
            unreachable!()
        };
        self.lower_scan(variable, label, None, layout)
    }

    /// A scan of `variable`'s nodes; `seek` for a label-less one a filter narrows to
    /// `property = value`.
    fn lower_scan(
        &mut self,
        variable: &str,
        label: &Option<String>,
        seek: Option<NodeSeek>,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        if layout.has(variable) {
            // already bound (a seed): only the label check remains
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            return match label {
                Some(lbl) => self.label_filter(l, variable, lbl),
                None => Ok(l),
            };
        }
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::NodeScan {
            out,
            label: label.clone(),
            seek,
        });
        Ok(Lowered { node, layout })
    }

    /// The seek a label-less scan of `variable` under `predicate` can take: a conjunct
    /// `variable.p = <literal or parameter>` while the label indexes on `p` cover every node. The
    /// scan rechecks the cover when it runs, and the filter stays above it either way.
    fn label_less_seek(
        &mut self,
        variable: &str,
        label: &Option<String>,
        predicate: &Expr,
        layout: &Layout,
    ) -> Result<Option<NodeSeek>, CypherError> {
        if label.is_some() || layout.has(variable) {
            return Ok(None);
        }
        let Some((property, value)) = property_eq_constant(predicate, variable) else {
            return Ok(None);
        };
        if !self.graph.property_seek_covers(property) {
            return Ok(None);
        }
        Ok(Some(NodeSeek {
            property: property.to_string(),
            value: self.compile(value, layout)?,
        }))
    }

    fn arm_node_by_id(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::NodeById {
            variable,
            label,
            id_expr,
        } = plan
        else {
            unreachable!()
        };
        if layout.has(variable) {
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            let pred = eq_expr(
                Expr::FunctionCall {
                    name: "id".into(),
                    distinct: false,
                    args: vec![Expr::Variable(variable.clone())],
                },
                id_expr.clone(),
            );
            let l = self.lower_filter(l, &pred)?;
            return match label {
                Some(lbl) => self.label_filter(l, variable, lbl),
                None => Ok(l),
            };
        }
        let id = self.compile(id_expr, &layout)?;
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::NodeByIdSeek {
            out,
            label: label.clone(),
            id,
        });
        Ok(Lowered { node, layout })
    }

    fn arm_node_by_id_range(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::NodeByIdRange {
            variable,
            label,
            lower,
            upper,
        } = plan
        else {
            unreachable!()
        };
        if layout.has(variable) {
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            let idf = Expr::FunctionCall {
                name: "id".into(),
                distinct: false,
                args: vec![Expr::Variable(variable.clone())],
            };
            let mut l = l;
            if let Some(lo) = lower {
                let pred = Expr::Comparison(
                    Box::new(idf.clone()),
                    vec![(crate::ast::CmpOp::Gte, lo.clone())],
                );
                l = self.lower_filter(l, &pred)?;
            }
            if let Some(hi) = upper {
                let pred = Expr::Comparison(
                    Box::new(idf.clone()),
                    vec![(crate::ast::CmpOp::Lt, hi.clone())],
                );
                l = self.lower_filter(l, &pred)?;
            }
            return match label {
                Some(lbl) => self.label_filter(l, variable, lbl),
                None => Ok(l),
            };
        }
        let lower_c = match lower {
            Some(e) => Some(self.compile(e, &layout)?),
            None => None,
        };
        let upper_c = match upper {
            Some(e) => Some(self.compile(e, &layout)?),
            None => None,
        };
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::NodeByIdRange {
            out,
            label: label.clone(),
            lower: lower_c,
            upper: upper_c,
        });
        Ok(Lowered { node, layout })
    }

    fn arm_index_scan(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::IndexScan {
            variable,
            label,
            property,
            value,
        } = plan
        else {
            unreachable!()
        };
        if layout.has(variable) {
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            let pred = eq_expr(
                Expr::Property(Box::new(Expr::Variable(variable.clone())), property.clone()),
                value.clone(),
            );
            let l = self.lower_filter(l, &pred)?;
            return self.label_filter(l, variable, label);
        }
        let v = self.compile(value, &layout)?;
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::IndexSeek {
            out,
            label: label.clone(),
            property: property.clone(),
            value: v,
        });
        Ok(Lowered { node, layout })
    }

    fn arm_index_range_scan(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::IndexRangeScan {
            variable,
            label,
            property,
            op,
            value,
        } = plan
        else {
            unreachable!()
        };
        if layout.has(variable) {
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            let pred = Expr::Comparison(
                Box::new(Expr::Property(
                    Box::new(Expr::Variable(variable.clone())),
                    property.clone(),
                )),
                vec![(*op, value.clone())],
            );
            let l = self.lower_filter(l, &pred)?;
            return self.label_filter(l, variable, label);
        }
        let v = self.compile(value, &layout)?;
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::IndexRange {
            out,
            label: label.clone(),
            property: property.clone(),
            op: *op,
            value: v,
        });
        Ok(Lowered { node, layout })
    }

    fn arm_text_index_scan(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::TextIndexScan {
            variable,
            label,
            property,
            kind,
            pattern,
        } = plan
        else {
            unreachable!()
        };
        if layout.has(variable) {
            // the enclosing filter keeps the exact predicate; only the label remains
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            return self.label_filter(l, variable, label);
        }
        let p = self.compile(pattern, &layout)?;
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::TextIndexScan {
            out,
            label: label.clone(),
            property: property.clone(),
            kind: *kind,
            pattern: p,
        });
        Ok(Lowered { node, layout })
    }

    fn arm_point_index_scan(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::PointIndexScan {
            variable,
            label,
            property,
            query,
        } = plan
        else {
            unreachable!()
        };
        if layout.has(variable) {
            let l = Lowered {
                node: self.source_or_base(layout.clone()),
                layout,
            };
            return self.label_filter(l, variable, label);
        }
        let q = match query {
            SpatialQuery::Distance { center, radius } => PhysSpatialQuery::Distance {
                center: self.compile(center, &layout)?,
                radius: self.compile(radius, &layout)?,
            },
            SpatialQuery::BBox { ll, ur } => PhysSpatialQuery::BBox {
                ll: self.compile(ll, &layout)?,
                ur: self.compile(ur, &layout)?,
            },
            SpatialQuery::Knn { center, k } => PhysSpatialQuery::Knn {
                center: self.compile(center, &layout)?,
                k: self.compile(k, &layout)?,
            },
        };
        let g = self.alloc.new_group();
        let mut layout = layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let node = self.source(PhysNode::PointIndexScan {
            out,
            label: label.clone(),
            property: property.clone(),
            query: q,
        });
        Ok(Lowered { node, layout })
    }

    fn arm_edge_type_scan(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
        } = plan
        else {
            unreachable!()
        };
        let g = self.alloc.new_group();
        let mut layout = layout;
        // bound endpoints (seeds) constrain through equality filters on fresh columns
        let mut checks: Vec<(String, String)> = Vec::new();
        let name_for = |layout: &Layout, v: &str, checks: &mut Vec<(String, String)>| {
            if layout.has(v) {
                let tmp = format!("__ets_{v}");
                checks.push((v.to_string(), tmp.clone()));
                tmp
            } else {
                v.to_string()
            }
        };
        let from_name = name_for(&layout, from_var, &mut checks);
        let to_name = name_for(&layout, to_var, &mut checks);
        let from = layout.bind_in(&mut self.alloc, &from_name, g, SlotType::Node);
        let rel = if rel_var.is_empty() {
            None
        } else {
            let rel_name = name_for(&layout, rel_var, &mut checks);
            Some(layout.bind_in(&mut self.alloc, &rel_name, g, SlotType::Edge))
        };
        let to = layout.bind_in(&mut self.alloc, &to_name, g, SlotType::Node);
        let node = self.source(PhysNode::EdgeTypeScan {
            from,
            rel,
            to,
            edge_types: edge_types.clone(),
            direction: *direction,
        });
        let mut l = Lowered { node, layout };
        for (bound, tmp) in checks {
            let pred = eq_expr(Expr::Variable(bound), Expr::Variable(tmp.clone()));
            l = self.lower_filter(l, &pred)?;
            l.layout.unbind(&tmp);
        }
        Ok(l)
    }

    fn arm_scan_filter_project(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::ScanFilterProject {
            variable,
            label,
            predicate,
            items,
            distinct,
            limit,
        } = plan
        else {
            unreachable!()
        };
        let seek = match predicate {
            Some(p) => self.label_less_seek(variable, label, p, &layout)?,
            None => None,
        };
        let mut l = self.lower_scan(variable, label, seek, layout)?;
        if let Some(p) = predicate {
            l = self.lower_filter(l, p)?;
        }
        let mut l = self.lower_project(l, items, *distinct, None)?;
        if let Some(n) = limit {
            let count = Expr::Literal(Literal::Integer(*n as i64));
            l = self.lower_limit(l, &count, false)?;
        }
        Ok(l)
    }

    fn arm_filter(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Filter { input, predicate } = plan else {
            unreachable!()
        };
        if let LogicalPlan::Scan { variable, label } = &**input
            && let Some(seek) = self.label_less_seek(variable, label, predicate, &layout)?
        {
            let l = self.lower_scan(variable, label, Some(seek), layout)?;
            return self.lower_filter(l, predicate);
        }
        let l = self.lower(input, layout)?;
        self.lower_filter(l, predicate)
    }

    fn arm_project(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        self.lower_project(l, items, *distinct, where_clause.as_ref())
    }

    fn arm_aggregate(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::Aggregate { input, keys, aggs } = plan else {
            unreachable!()
        };
        if let Some(l) = self.try_count_source(input, keys, aggs, &layout)? {
            return Ok(l);
        }
        // a whole chain of counted hops before the last hop alone
        if let Some(l) = self.try_chain_weights(input, keys, aggs, &layout)? {
            return Ok(l);
        }
        if let Some(l) = self.try_degree_sum(input, keys, aggs, &layout)? {
            return Ok(l);
        }
        let l = self.lower(input, layout)?;
        self.lower_aggregate(l, keys, aggs)
    }

    /// Counts over a chain of two or more hops from a label scan where nothing is read but,
    /// at most, one end of the chain (label checks on the nodes aside): the chain's path counts
    /// are carried hop by hop and the aggregate sums them. A chain read from its first node
    /// runs reversed.
    fn try_chain_weights(
        &mut self,
        input: &LogicalPlan,
        keys: &[ProjectionItem],
        aggs: &[ProjectionItem],
        layout: &Layout,
    ) -> Result<Option<Lowered>, CypherError> {
        if aggs.is_empty() || self.pending_base.is_some() {
            return Ok(None);
        }
        // walk down: [Filter(label checks on the hop's end)]? Expand ... Scan
        struct Hop<'a> {
            from: &'a str,
            rel_var: &'a str,
            to: &'a str,
            dir: Direction,
            edge_types: &'a [String],
            to_label: Option<String>,
        }
        let mut hops_down: Vec<Hop<'_>> = Vec::new();
        let mut pending: Option<&Expr> = None;
        let mut cur = input;
        let (source_var, source_label) = loop {
            match cur {
                LogicalPlan::Filter {
                    input: inner,
                    predicate,
                } => {
                    if pending.is_some() {
                        return Ok(None);
                    }
                    pending = Some(predicate);
                    cur = inner;
                }
                LogicalPlan::ExpandOut {
                    input: inner,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    ..
                }
                | LogicalPlan::ExpandIn {
                    input: inner,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    ..
                }
                | LogicalPlan::ExpandBoth {
                    input: inner,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    ..
                } => {
                    if range.is_some() || from == to {
                        return Ok(None);
                    }
                    let dir = match cur {
                        LogicalPlan::ExpandOut { .. } => Direction::Outgoing,
                        LogicalPlan::ExpandIn { .. } => Direction::Incoming,
                        _ => Direction::Both,
                    };
                    let mut to_label = None;
                    if let Some(pred) = pending.take() {
                        let mut labels = Vec::new();
                        if !collect_label_checks(pred, to, &mut labels) || labels.len() != 1 {
                            return Ok(None);
                        }
                        to_label = labels.pop();
                    }
                    hops_down.push(Hop {
                        from,
                        rel_var,
                        to,
                        dir,
                        edge_types,
                        to_label,
                    });
                    cur = inner;
                }
                LogicalPlan::Scan { variable, label } => {
                    if pending.is_some() {
                        return Ok(None);
                    }
                    break (variable.as_str(), label.clone());
                }
                _ => return Ok(None),
            }
        };
        if hops_down.len() < 2 {
            return Ok(None);
        }
        hops_down.reverse();
        // the chain must be bound here and nowhere before
        let first = source_var;
        let last = hops_down.last().unwrap().to;
        let mut pattern_vars: Vec<&str> = vec![first];
        for h in &hops_down {
            if h.from != pattern_vars[pattern_vars.len() - 1] {
                return Ok(None);
            }
            if !h.rel_var.is_empty() {
                pattern_vars.push(h.rel_var);
            }
            pattern_vars.push(h.to);
        }
        if pattern_vars.iter().any(|v| layout.has(v)) {
            return Ok(None);
        }
        let inner_vars: Vec<&str> = pattern_vars
            .iter()
            .copied()
            .filter(|v| *v != first && *v != last)
            .collect();
        let reads_inner = |v: &str| inner_vars.contains(&v);
        if keys
            .iter()
            .any(|k| mentions_variable(&k.expr, &reads_inner))
        {
            return Ok(None);
        }
        let reads_first = keys
            .iter()
            .any(|k| mentions_variable(&k.expr, &|v| v == first));
        let reads_last = keys
            .iter()
            .any(|k| mentions_variable(&k.expr, &|v| v == last));
        if reads_first && reads_last {
            return Ok(None);
        }
        for a in aggs {
            match &a.expr {
                Expr::FunctionCall {
                    name,
                    distinct,
                    args,
                } if name.eq_ignore_ascii_case("count") && !*distinct => match args.first() {
                    None => {}
                    Some(Expr::Variable(v))
                        if (*v == first && !reads_last) || (*v == last && !reads_first) => {}
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
        }
        // the exact plan: the chain as it is, grouped by the end that is read
        let endpoint: Option<&str> = if reads_first {
            Some(first)
        } else if reads_last {
            Some(last)
        } else {
            None
        };
        let exact_keys: Vec<ProjectionItem> = endpoint
            .iter()
            .map(|v| ProjectionItem {
                expr: Expr::Variable(v.to_string()),
                alias: Some("__cw_node".to_string()),
                source_text: None,
            })
            .collect();
        let exact_aggs = vec![ProjectionItem {
            expr: Expr::FunctionCall {
                name: "count".to_string(),
                distinct: false,
                args: Vec::new(),
            },
            alias: Some("__cw_count".to_string()),
            source_text: None,
        }];
        let exact_in = self.lower(input, layout.clone())?;
        let exact = self.lower_aggregate(exact_in, &exact_keys, &exact_aggs)?;
        let count_pos = exact
            .layout
            .pos("__cw_count")
            .ok_or_else(|| unbound("__cw_count"))?;
        let node_pos = exact.layout.pos("__cw_node").unwrap_or(count_pos);
        // the carried chain, reversed when the first node is the one read
        let mut hops: Vec<ChainHop> = Vec::new();
        let from_label;
        if reads_first {
            from_label = hops_down.last().unwrap().to_label.clone();
            for (i, h) in hops_down.iter().enumerate().rev() {
                let to_label = if i == 0 {
                    source_label.clone()
                } else {
                    hops_down[i - 1].to_label.clone()
                };
                let dir = match h.dir {
                    Direction::Outgoing => Direction::Incoming,
                    Direction::Incoming => Direction::Outgoing,
                    Direction::Both => Direction::Both,
                };
                hops.push(ChainHop {
                    dir,
                    edge_types: h.edge_types.to_vec(),
                    to_label,
                });
            }
        } else {
            from_label = source_label.clone();
            for h in &hops_down {
                hops.push(ChainHop {
                    dir: h.dir,
                    edge_types: h.edge_types.to_vec(),
                    to_label: h.to_label.clone(),
                });
            }
        }
        let out_group = self.alloc.new_group();
        let mut out_layout = layout.clone();
        let out = out_layout.bind_in(
            &mut self.alloc,
            endpoint.unwrap_or("__cw_end"),
            out_group,
            SlotType::Node,
        );
        let weight = out_layout.bind_in(&mut self.alloc, "__cw_weight", out_group, SlotType::Int);
        let node = PhysNode::ChainWeights {
            out,
            weight,
            out_group,
            from_label,
            hops,
            count_only: endpoint.is_none(),
            exact: Box::new(exact.node),
            exact_cols: (node_pos, count_pos),
        };
        let sums: Vec<ProjectionItem> = aggs
            .iter()
            .map(|a| ProjectionItem {
                expr: Expr::FunctionCall {
                    name: "sum".to_string(),
                    distinct: false,
                    args: vec![Expr::Variable("__cw_weight".to_string())],
                },
                alias: a.alias.clone(),
                source_text: Some(crate::executor::projection_name(a)),
            })
            .collect();
        let l = Lowered {
            node,
            layout: out_layout,
        };
        self.lower_aggregate(l, keys, &sums).map(Some)
    }

    /// Counts over a single hop whose relationship and far end nothing reads (at most a label
    /// check on the far end): the hop becomes a per-source edge count and every count a sum of
    /// it, so no row of the expansion is built. Keys may read anything bound before the hop.
    fn try_degree_sum(
        &mut self,
        input: &LogicalPlan,
        keys: &[ProjectionItem],
        aggs: &[ProjectionItem],
        layout: &Layout,
    ) -> Result<Option<Lowered>, CypherError> {
        if aggs.is_empty() || self.pending_base.is_some() {
            return Ok(None);
        }
        let is_hop = |p: &LogicalPlan| {
            matches!(
                p,
                LogicalPlan::ExpandOut { .. }
                    | LogicalPlan::ExpandIn { .. }
                    | LogicalPlan::ExpandBoth { .. }
            )
        };
        let (hop, predicate) = match input {
            LogicalPlan::Filter {
                input: hop,
                predicate,
            } if is_hop(hop) => (&**hop, Some(predicate)),
            hop if is_hop(hop) => (hop, None),
            _ => return Ok(None),
        };
        let (hop_input, from, rel_var, to, edge_types, range, prior, dir) = match hop {
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Outgoing,
            ),
            LogicalPlan::ExpandIn {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Incoming,
            ),
            LogicalPlan::ExpandBoth {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Both,
            ),
            _ => unreachable!("a hop"),
        };
        // one hop, both ends free of earlier bindings; the edges bound earlier in the row are
        // taken off the count
        let below = crate::optimizer::rewrite::plan_output_variables(hop_input);
        let bound = |v: &str| layout.has(v) || below.contains(v);
        if range.is_some() || from == to || bound(to) || (!rel_var.is_empty() && bound(rel_var)) {
            return Ok(None);
        }
        let hidden = |v: &str| v == to || (!rel_var.is_empty() && v == rel_var);
        // count(x): the hop's own variables are never null in its rows
        for a in aggs {
            match &a.expr {
                Expr::FunctionCall {
                    name,
                    distinct,
                    args,
                } if name.eq_ignore_ascii_case("count") && !*distinct => match args.first() {
                    None | Some(Expr::Variable(_)) => {}
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
        }
        let mut to_label: Option<String> = None;
        if let Some(pred) = predicate {
            let mut labels = Vec::new();
            if !collect_label_checks(pred, to, &mut labels) || labels.len() != 1 {
                return Ok(None);
            }
            to_label = labels.pop();
        }
        // the keys must not read the hop's bindings
        if keys.iter().any(|k| mentions_variable(&k.expr, &hidden)) {
            return Ok(None);
        }
        let l = self.lower(hop_input, layout.clone())?;
        let from_pos = l.layout.pos(from).ok_or_else(|| unbound(from))?;
        let prior_edges: Vec<DataPos> = prior.iter().filter_map(|v| l.layout.pos(v)).collect();
        // the count is per source lane: an edge kept off from another group must be one value
        let off_source: Vec<DataPos> = prior_edges
            .iter()
            .copied()
            .filter(|p| p.group != from_pos.group)
            .collect();
        let mut l = self.flatten_for(l, &off_source);
        let hidden_name = format!("__deg_{from}");
        let out = l
            .layout
            .bind_in(&mut self.alloc, &hidden_name, from_pos.group, SlotType::Int);
        l.node = PhysNode::ExpandCount {
            input: Box::new(l.node),
            from: from_pos,
            out,
            dir,
            edge_types: edge_types.clone(),
            to_label,
            prior_edges,
        };
        let sums: Vec<ProjectionItem> = aggs
            .iter()
            .map(|a| ProjectionItem {
                expr: Expr::FunctionCall {
                    name: "sum".to_string(),
                    distinct: false,
                    args: vec![Expr::Variable(hidden_name.clone())],
                },
                alias: a.alias.clone(),
                source_text: Some(crate::executor::projection_name(a)),
            })
            .collect();
        self.lower_aggregate(l, keys, &sums).map(Some)
    }

    fn arm_sort(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Sort { input, items } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let mut keys = Vec::with_capacity(items.len());
        for it in items {
            keys.push((self.compile(&it.expr, &l.layout)?, it.ascending));
        }
        let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::Sort {
                input: Box::new(l.node),
                keys,
                in_groups,
                out_group,
                moves,
                limit: None,
            },
            layout: out_layout,
        })
    }

    fn arm_limit(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Limit { input, count } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let materialize = crate::executor::plan_is_mutating(input);
        self.lower_limit(l, count, materialize)
    }

    fn arm_skip(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Skip { input, count } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let count_c = self.compile(count, &l.layout)?;
        let in_groups = l.layout.groups();
        if l.layout.unflat_groups().len() <= 1 {
            return Ok(Lowered {
                node: PhysNode::Skip {
                    input: Box::new(l.node),
                    count: count_c,
                    in_groups,
                    consume: None,
                },
                layout: l.layout,
            });
        }
        let (_, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::Skip {
                input: Box::new(l.node),
                count: count_c,
                in_groups,
                consume: Some(Consume { out_group, moves }),
            },
            layout: out_layout,
        })
    }

    fn arm_eager(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Eager { input } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::Eager {
                input: Box::new(l.node),
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_union(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Union { left, right, all } = plan else {
            unreachable!()
        };
        // a seeded body that is a UNION: the left branch's first source nests under the
        // seed; the right branch reads the seed group, which stays active
        let l = self.lower(left, layout.clone())?;
        let r = self.lower(right, layout.clone())?;
        let out_group = self.alloc.new_group();
        let mut out_layout = Layout::new();
        let mut names: Vec<String> = l.layout.names().map(str::to_string).collect();
        for n in r.layout.names() {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
        let mut moves_left = Vec::new();
        let mut moves_right = Vec::new();
        for name in &names {
            let ty = l
                .layout
                .ty(name)
                .or_else(|| r.layout.ty(name))
                .unwrap_or(SlotType::Any);
            let out = out_layout.bind_in(&mut self.alloc, name, out_group, ty);
            if let Some(p) = l.layout.pos(name) {
                moves_left.push((p, out));
            }
            if let Some(p) = r.layout.pos(name) {
                moves_right.push((p, out));
            }
        }
        Ok(Lowered {
            node: PhysNode::Union {
                left: Box::new(l.node),
                right: Box::new(r.node),
                all: *all,
                out_group,
                moves_left,
                moves_right,
                left_groups: l.layout.groups(),
                right_groups: r.layout.groups(),
            },
            layout: out_layout,
        })
    }

    fn arm_cross_product(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::CrossProduct { left, right } = plan else {
            unreachable!()
        };
        let l = self.lower(left, layout.clone())?;
        let r = self.lower(right, layout)?;
        let left_out = self.alloc.new_group();
        let right_out = self.alloc.new_group();
        let (mut out_layout, moves_left) = l.layout.flattened_into(&mut self.alloc, left_out);
        let mut moves_right = Vec::new();
        for (name, pos, ty) in r.layout.bindings() {
            // the sides bind disjoint variables; a seed bound on both keeps the left copy
            if out_layout.has(name) {
                continue;
            }
            let out = out_layout.bind_in(&mut self.alloc, name, right_out, ty);
            moves_right.push((pos, out));
        }
        Ok(Lowered {
            node: PhysNode::CrossProduct {
                left: Box::new(l.node),
                right: Box::new(r.node),
                left_groups: l.layout.groups(),
                right_groups: r.layout.groups(),
                left_out,
                moves_left,
                right_out,
                moves_right,
            },
            layout: out_layout,
        })
    }

    fn arm_hash_join(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::HashJoin {
            left,
            right,
            left_keys,
            right_keys,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(left, layout.clone())?;
        let r = self.lower(right, layout)?;
        let mut lk = Vec::with_capacity(left_keys.len());
        for k in left_keys {
            lk.push(self.compile(k, &l.layout)?);
        }
        let mut rk = Vec::with_capacity(right_keys.len());
        for k in right_keys {
            rk.push(self.compile(k, &r.layout)?);
        }
        let out_group = self.alloc.new_group();
        let (mut out_layout, moves_left) = l.layout.flattened_into(&mut self.alloc, out_group);
        let mut moves_right = Vec::new();
        for (name, pos, ty) in r.layout.bindings() {
            // a name bound on both sides takes the right side's value
            let out = out_layout.bind_in(&mut self.alloc, name, out_group, ty);
            moves_right.push((pos, out));
        }
        Ok(Lowered {
            node: PhysNode::HashJoin {
                left: Box::new(l.node),
                right: Box::new(r.node),
                left_keys: lk,
                right_keys: rk,
                left_groups: l.layout.groups(),
                right_groups: r.layout.groups(),
                out_group,
                moves_left,
                moves_right,
            },
            layout: out_layout,
        })
    }

    fn arm_optional_match(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::OptionalMatch {
            input,
            match_plan,
            new_variables,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let body = self.lower_subplan(match_plan, &l.layout, &[])?;
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let mut body_out = Vec::new();
        // variables bound before the OPTIONAL MATCH keep their outer value; the rest come
        // from the body (null when it matched nothing)
        for (name, _) in &body.out {
            if !l.layout.has(name) {
                let ty = body_layout_ty(&body, name);
                let out = out_layout.bind_in(&mut self.alloc, name, out_group, ty);
                body_out.push((body_pos(&body, name), out));
            }
        }
        for v in new_variables {
            if !out_layout.has(v) {
                out_layout.bind_in(&mut self.alloc, v, out_group, SlotType::Any);
            }
        }
        Ok(Lowered {
            node: PhysNode::OptionalMatch {
                input: Box::new(l.node),
                body,
                in_groups,
                out_group,
                moves,
                body_out,
            },
            layout: out_layout,
        })
    }

    fn arm_distinct_expand(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::DistinctExpand {
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
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let from_pos = l.layout.pos(from).ok_or_else(|| unbound(from))?;
        // every source at once when nothing past the expansion reads the input row: a global
        // dedup (the fusion projects the target alone) and a target filter on the target alone
        let multi = *global_dedup
            && target_filter.as_ref().is_none_or(|e| {
                let mut vars = HashSet::new();
                crate::planner::collect_expr_variables(e, &mut vars);
                vars.iter().all(|v| v == to)
            });
        let (l, multi_source) = if multi {
            let groups = l.layout.groups();
            (l, Some(groups))
        } else {
            // the source flat, and whatever else the target filter reads (a bound end the
            // fusion keeps a fresh one to, another pattern's node): one input row per evaluation
            let mut flat = vec![from_pos];
            if let Some(e) = target_filter {
                let mut vars = HashSet::new();
                crate::planner::collect_expr_variables(e, &mut vars);
                flat.extend(vars.iter().filter_map(|v| l.layout.pos(v)));
            }
            (self.flatten_for(l, &flat), None)
        };
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let to_pos = layout.bind_in(&mut self.alloc, to, g, SlotType::Node);
        let tf = match target_filter {
            Some(e) => Some(self.compile(e, &layout)?),
            None => None,
        };
        Ok(Lowered {
            node: PhysNode::DistinctExpand {
                input: Box::new(l.node),
                from: from_pos,
                to: to_pos,
                out_group: g,
                min: *min_hops,
                max: *max_hops,
                dir: *direction,
                edge_types: edge_types.clone(),
                target_label: target_label.clone(),
                global_dedup: *global_dedup,
                multi_source,
                target_filter: tf,
            },
            layout,
        })
    }

    fn arm_unwind(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Unwind {
            input, expr, alias, ..
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let list = self.compile(expr, &l.layout)?;
        let l = self.flatten_for(l, &list.positions());
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let out = layout.bind_in(&mut self.alloc, alias, g, SlotType::Any);
        Ok(Lowered {
            node: PhysNode::Unwind {
                input: Box::new(l.node),
                list,
                out,
                out_group: g,
            },
            layout,
        })
    }

    fn arm_index_lookup_join(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::IndexLookupJoin {
            input,
            variable,
            label,
            property,
            key,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let key_c = self.compile(key, &l.layout)?;
        let l = self.flatten_for(l, &key_c.positions());
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        Ok(Lowered {
            node: PhysNode::IndexLookupJoin {
                input: Box::new(l.node),
                key: key_c,
                out,
                out_group: g,
                label: label.clone(),
                property: property.clone(),
            },
            layout,
        })
    }

    fn arm_vector_search_scan(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::VectorSearchScan {
            input,
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let query = self.compile(query_vector, &l.layout)?;
        let limit_c = self.compile(limit, &l.layout)?;
        let mut positions = query.positions();
        positions.extend(limit_c.positions());
        let l = self.flatten_for(l, &positions);
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Node);
        let score = score_var
            .as_ref()
            .map(|s| layout.bind_in(&mut self.alloc, s, g, SlotType::Float));
        let index_where_c = match index_where {
            Some(e) => Some(self.compile(e, &layout)?),
            None => None,
        };
        Ok(Lowered {
            node: PhysNode::VectorSearch {
                input: Box::new(l.node),
                out,
                score,
                out_group: g,
                index_name: index_name.clone(),
                query,
                index_where: index_where_c,
                limit: limit_c,
            },
            layout,
        })
    }

    fn arm_bind_path(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let nodes: Vec<DataPos> = node_vars.iter().filter_map(|v| l.layout.pos(v)).collect();
        let edges: Vec<DataPos> = edge_vars.iter().filter_map(|v| l.layout.pos(v)).collect();
        let mut positions = nodes.clone();
        positions.extend(edges.iter().copied());
        let (l, group) = self.eval_group_for(l, &positions);
        let g = match group {
            Some(g) => g,
            None => self.scalar_group(),
        };
        let mut layout = l.layout;
        let out = layout.bind_in(&mut self.alloc, variable, g, SlotType::Path);
        Ok(Lowered {
            node: PhysNode::BindPath {
                input: Box::new(l.node),
                out,
                node_vars: nodes,
                edge_vars: edges,
            },
            layout,
        })
    }

    fn arm_bind_qpp_groups(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::BindQppGroups {
            input,
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let anchor = l
            .layout
            .pos(anchor_var)
            .ok_or_else(|| unbound(anchor_var))?;
        let rel = l.layout.pos(rel_var).ok_or_else(|| unbound(rel_var))?;
        let (l, group) = self.eval_group_for(l, &[anchor, rel]);
        let g = match group {
            Some(g) => g,
            None => self.scalar_group(),
        };
        let mut layout = l.layout;
        let src = src_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::List));
        let dst = dst_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::List));
        Ok(Lowered {
            node: PhysNode::BindQppGroups {
                input: Box::new(l.node),
                anchor,
                rel,
                src,
                dst,
            },
            layout,
        })
    }

    fn arm_bind_var_lists(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::BindVarLists { input, groups } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let mut positions = Vec::new();
        for (_, members) in groups {
            for m in members {
                if let Some(p) = l.layout.pos(m) {
                    positions.push(p);
                }
            }
        }
        let (l, group) = self.eval_group_for(l, &positions);
        let g = match group {
            Some(g) => g,
            None => self.scalar_group(),
        };
        let mut layout = l.layout;
        let mut outs = Vec::new();
        for (target, members) in groups {
            let ms: Vec<Option<DataPos>> = members.iter().map(|m| layout.pos(m)).collect();
            let out = layout.bind_in(&mut self.alloc, target, g, SlotType::List);
            outs.push((out, ms));
        }
        Ok(Lowered {
            node: PhysNode::BindVarLists {
                input: Box::new(l.node),
                groups: outs,
            },
            layout,
        })
    }

    fn arm_bind_path_endpoints(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::BindPathEndpoints {
            input,
            rel_var,
            from_var,
            to_var,
            direction,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let rel = l.layout.pos(rel_var).ok_or_else(|| unbound(rel_var))?;
        let from_bound = l.layout.pos(from_var);
        let to_bound = l.layout.pos(to_var);
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let from = out_layout.bind_in(&mut self.alloc, from_var, out_group, SlotType::Node);
        let to = out_layout.bind_in(&mut self.alloc, to_var, out_group, SlotType::Node);
        Ok(Lowered {
            node: PhysNode::BindPathEndpoints {
                input: Box::new(l.node),
                rel,
                from,
                to,
                direction: direction.clone(),
                out_group,
                in_groups,
                moves,
                from_bound,
                to_bound,
            },
            layout: out_layout,
        })
    }

    fn arm_bind_edge_endpoints(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::BindEdgeEndpoints {
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let edge = l.layout.pos(edge_var).ok_or_else(|| unbound(edge_var))?;
        let from_bound = l.layout.pos(from_var);
        let to_bound = l.layout.pos(to_var);
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let from = out_layout.bind_in(&mut self.alloc, from_var, out_group, SlotType::Node);
        let to = out_layout.bind_in(&mut self.alloc, to_var, out_group, SlotType::Node);
        Ok(Lowered {
            node: PhysNode::BindEdgeEndpoints {
                input: Box::new(l.node),
                edge,
                from,
                to,
                direction: direction.clone(),
                edge_types: edge_types.clone(),
                out_group,
                in_groups,
                moves,
                from_bound,
                to_bound,
            },
            layout: out_layout,
        })
    }

    fn arm_call_procedure(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::CallProcedure {
            input,
            procedure_name,
            args,
            yield_columns,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let (in_groups, out_group, mut out_layout, _moves) = self.consume(&l.layout);
        // the procedure's outputs: YIELD targets, else every output column
        let outs: Vec<String> = if yield_columns.is_empty() {
            if let Some(p) = crate::procedure::get_procedure(procedure_name) {
                p.outputs.iter().map(|o| o.name.clone()).collect()
            } else if let Some(cols) = crate::procedure::builtin_schema_proc_columns(procedure_name)
            {
                cols.iter().map(|c| (*c).to_string()).collect()
            } else {
                Vec::new()
            }
        } else {
            yield_columns.iter().map(|(_, b)| b.clone()).collect()
        };
        for name in &outs {
            if !out_layout.has(name) {
                out_layout.bind_in(&mut self.alloc, name, out_group, SlotType::Any);
            }
        }
        Ok(Lowered {
            node: PhysNode::CallProcedure {
                input: Box::new(l.node),
                name: procedure_name.clone(),
                args: args.clone(),
                yield_columns: yield_columns.clone(),
                in_groups,
                out_group,
                in_layout: l.layout.clone(),
                out_layout: out_layout.clone(),
            },
            layout: out_layout,
        })
    }

    fn arm_call_subquery(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::CallSubquery {
            input,
            imports,
            body,
            return_aliases,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let seed_layout = match imports {
            Some(names) => {
                let mut s = Layout::new();
                for n in names {
                    if let (Some(p), Some(t)) = (l.layout.pos(n), l.layout.ty(n)) {
                        s.bind(n, p, t);
                    }
                }
                s
            }
            None => l.layout.clone(),
        };
        let body_plan = self.lower_subplan(body, &seed_layout, &[])?;
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let mut body_out = Vec::new();
        for (name, _) in &body_plan.out {
            if return_aliases.contains(name) || !out_layout.has(name) {
                let ty = body_layout_ty(&body_plan, name);
                let out = out_layout.bind_in(&mut self.alloc, name, out_group, ty);
                body_out.push((body_pos(&body_plan, name), out));
            }
        }
        Ok(Lowered {
            node: PhysNode::CallSubquery {
                input: Box::new(l.node),
                body: body_plan,
                in_groups,
                out_group,
                moves,
                body_out,
            },
            layout: out_layout,
        })
    }

    fn arm_call_in_transactions(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::CallInTransactions {
            input,
            body,
            batch_size,
            on_error,
            report_status,
            return_aliases,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let batch_c = match batch_size {
            Some(e) => Some(self.compile(e, &l.layout)?),
            None => None,
        };
        let body_plan = self.lower_subplan(body, &l.layout, &[])?;
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let mut body_out = Vec::new();
        for (name, _) in &body_plan.out {
            if return_aliases.contains(name) {
                let ty = body_layout_ty(&body_plan, name);
                let out = out_layout.bind_in(&mut self.alloc, name, out_group, ty);
                body_out.push((body_pos(&body_plan, name), out));
            }
        }
        let report = report_status
            .as_ref()
            .map(|v| out_layout.bind_in(&mut self.alloc, v, out_group, SlotType::Any));
        Ok(Lowered {
            node: PhysNode::CallInTransactions {
                input: Box::new(l.node),
                body: body_plan,
                batch_size: batch_c,
                on_error: *on_error,
                report_status: report,
                in_groups,
                out_group,
                moves,
                body_out,
            },
            layout: out_layout,
        })
    }

    fn arm_for_each(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::ForEach {
            input,
            variable,
            list,
            body,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let list_c = self.compile(list, &l.layout)?;
        let mut seed_layout = l.layout.clone();
        // the loop variable rides in the seed; its position is assigned by the sub-plan
        seed_layout.bind(variable, DataPos::new(u16::MAX, u16::MAX), SlotType::Any);
        let body_plan = self.lower_subplan(body, &seed_layout, &[])?;
        let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::ForEach {
                input: Box::new(l.node),
                list: list_c,
                body: body_plan,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_shortest_path(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::ShortestPath {
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
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let from_pos = l.layout.pos(from).ok_or_else(|| unbound(from))?;
        let to_pos = l.layout.pos(to).ok_or_else(|| unbound(to))?;
        let l = self.flatten_for(l, &[from_pos, to_pos]);
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let path = path_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::Path));
        let mut rel = rel_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::List));
        // nothing named: a hidden edge list keeps the group in the row product, so every path
        // still counts as a row
        if path.is_none() && rel.is_none() {
            let name = format!("__sp_rel_{g}");
            rel = Some(layout.bind_in(&mut self.alloc, &name, g, SlotType::List));
        }
        Ok(Lowered {
            node: PhysNode::ShortestPath {
                input: Box::new(l.node),
                from: from_pos,
                to: to_pos,
                edge_types: edge_types.clone(),
                direction: direction.clone(),
                min_depth: *min_depth,
                max_depth: *max_depth,
                path,
                rel,
                all: *all,
                k: *k,
                k_groups: *k_groups,
                out_group: g,
            },
            layout,
        })
    }

    fn arm_weighted_shortest_path(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::WeightedShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path_var,
            rel_var,
            cost_var,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let from_pos = l.layout.pos(from).ok_or_else(|| unbound(from))?;
        let to_pos = l.layout.pos(to).ok_or_else(|| unbound(to))?;
        let l = self.flatten_for(l, &[from_pos, to_pos]);
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let path = path_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::Path));
        let rel = rel_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::List));
        let mut cost = cost_var
            .as_ref()
            .map(|v| layout.bind_in(&mut self.alloc, v, g, SlotType::Float));
        if path.is_none() && rel.is_none() && cost.is_none() {
            let name = format!("__sp_cost_{g}");
            cost = Some(layout.bind_in(&mut self.alloc, &name, g, SlotType::Float));
        }
        Ok(Lowered {
            node: PhysNode::WeightedShortestPath {
                input: Box::new(l.node),
                from: from_pos,
                to: to_pos,
                edge_types: edge_types.clone(),
                direction: direction.clone(),
                weight_property: weight_property.clone(),
                path,
                rel,
                cost,
                out_group: g,
            },
            layout,
        })
    }

    fn arm_load_csv(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::LoadCsv {
            input,
            url,
            with_headers,
            row_alias,
            field_terminator,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let url_c = self.compile(url, &l.layout)?;
        let l = self.flatten_for(l, &url_c.positions());
        let g = self.alloc.new_group();
        let mut layout = l.layout;
        let row = layout.bind_in(&mut self.alloc, row_alias, g, SlotType::Any);
        let ctx = layout.bind_in(&mut self.alloc, CSV_CTX_SLOT, g, SlotType::Int);
        Ok(Lowered {
            node: PhysNode::LoadCsv {
                input: Box::new(l.node),
                url: url_c,
                with_headers: *with_headers,
                row,
                ctx,
                out_group: g,
                field_terminator: *field_terminator,
            },
            layout,
        })
    }

    fn arm_create_node(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::CreateNode {
            input,
            variable,
            labels,
            dynamic_labels,
            properties,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let mut dyn_c = Vec::with_capacity(dynamic_labels.len());
        for e in dynamic_labels {
            dyn_c.push(self.compile(e, &l.layout)?);
        }
        let props = match properties {
            Some(e) => Some(self.compile(e, &l.layout)?),
            None => None,
        };
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let out = variable
            .as_ref()
            .map(|v| out_layout.bind_in(&mut self.alloc, v, out_group, SlotType::Node));
        Ok(Lowered {
            node: PhysNode::CreateNode {
                input: Box::new(l.node),
                out,
                labels: labels.clone(),
                dynamic_labels: dyn_c,
                properties: props,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_create_rel(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::CreateRel {
            input,
            variable,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let from_pos = l.layout.pos(from).ok_or_else(|| unbound(from))?;
        let to_pos = l.layout.pos(to).ok_or_else(|| unbound(to))?;
        let dyn_c = match dynamic_edge_type {
            Some(e) => Some(self.compile(e, &l.layout)?),
            None => None,
        };
        let props = match properties {
            Some(e) => Some(self.compile(e, &l.layout)?),
            None => None,
        };
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let out = variable
            .as_ref()
            .map(|v| out_layout.bind_in(&mut self.alloc, v, out_group, SlotType::Edge));
        Ok(Lowered {
            node: PhysNode::CreateRel {
                input: Box::new(l.node),
                out,
                from: from_pos,
                to: to_pos,
                edge_type: edge_type.clone(),
                dynamic_edge_type: dyn_c,
                properties: props,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_set_property(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::SetProperty { input, items } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let actions = self.set_actions(items, &l.layout)?;
        let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::SetProps {
                input: Box::new(l.node),
                items: actions,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_remove_op(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::RemoveOp { input, items } = plan else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let mut actions = Vec::with_capacity(items.len());
        for item in items {
            actions.push(match item {
                crate::ast::RemoveItem::Property(access) => match l.layout.pos(&access.expr_or_var)
                {
                    Some(entity) => RemoveAction::Property {
                        entity,
                        key: access.key.clone(),
                    },
                    None => RemoveAction::Unbound,
                },
                crate::ast::RemoveItem::Label(var, labels, dynamic) => match l.layout.pos(var) {
                    Some(entity) => {
                        let mut d = Vec::with_capacity(dynamic.len());
                        for e in dynamic {
                            d.push(self.compile(e, &l.layout)?);
                        }
                        RemoveAction::Labels {
                            entity,
                            labels: labels.clone(),
                            dynamic: d,
                        }
                    }
                    None => RemoveAction::Unbound,
                },
            });
        }
        let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::Remove {
                input: Box::new(l.node),
                items: actions,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_delete(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Delete {
            input,
            exprs,
            detach,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let mut es = Vec::with_capacity(exprs.len());
        for e in exprs {
            es.push(self.compile(e, &l.layout)?);
        }
        let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::Delete {
                input: Box::new(l.node),
                exprs: es,
                detach: *detach,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_merge(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let LogicalPlan::Merge {
            input,
            pattern,
            on_create,
            on_match,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        // the pattern's variables, bound after the merge (fresh or already bound)
        let mut binds = Vec::new();
        for part in &pattern.parts {
            // a variable bound before the MERGE keeps its (moved) position: the merge reads it as
            // a bound endpoint; a fresh one gets a column of its own
            let bind = |out_layout: &mut Layout,
                        alloc: &mut GroupAlloc,
                        v: &String,
                        ty: SlotType| match out_layout.pos(v) {
                Some(p) => p,
                None => out_layout.bind_in(alloc, v, out_group, ty),
            };
            if let Some(v) = &part.element.node.variable {
                let out = bind(&mut out_layout, &mut self.alloc, v, SlotType::Node);
                binds.push((v.clone(), out));
            }
            for chain in &part.element.chain {
                if let Some(v) = &chain.rel.variable {
                    let out = bind(&mut out_layout, &mut self.alloc, v, SlotType::Edge);
                    binds.push((v.clone(), out));
                }
                if let Some(v) = &chain.node.variable {
                    let out = bind(&mut out_layout, &mut self.alloc, v, SlotType::Node);
                    binds.push((v.clone(), out));
                }
            }
        }
        let on_create_a = match on_create {
            Some(sc) => Some(self.set_actions(&sc.items, &out_layout)?),
            None => None,
        };
        let on_match_a = match on_match {
            Some(sc) => Some(self.set_actions(&sc.items, &out_layout)?),
            None => None,
        };
        Ok(Lowered {
            node: PhysNode::Merge {
                input: Box::new(l.node),
                pattern: pattern.clone(),
                on_create: on_create_a,
                on_match: on_match_a,
                binds,
                out_layout: out_layout.clone(),
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    fn arm_drop_index(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::DropIndex { name } = plan else {
            unreachable!()
        };
        self.schema(SchemaOp::DropIndex { name: name.clone() }, layout)
    }

    fn arm_drop_constraint(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::DropConstraint { name } = plan else {
            unreachable!()
        };
        self.schema(SchemaOp::DropConstraint { name: name.clone() }, layout)
    }

    fn arm_drop_edge_index(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::DropEdgeIndex { name } = plan else {
            unreachable!()
        };
        self.schema(SchemaOp::DropEdgeIndex { name: name.clone() }, layout)
    }

    fn arm_transaction_command(
        &mut self,
        plan: &LogicalPlan,
        layout: Layout,
    ) -> Result<Lowered, CypherError> {
        let LogicalPlan::TransactionCommand {
            input,
            command,
            ids,
        } = plan
        else {
            unreachable!()
        };
        let l = self.lower(input, layout)?;
        let ids_c = match ids {
            Some(e) => Some(self.compile(e, &l.layout)?),
            None => None,
        };
        let (in_groups, out_group, mut out_layout, moves) = self.consume(&l.layout);
        let cols: Vec<DataPos> = command
            .columns()
            .iter()
            .map(|c| out_layout.bind_in(&mut self.alloc, c, out_group, SlotType::Any))
            .collect();
        Ok(Lowered {
            node: PhysNode::TransactionCommand {
                input: Box::new(l.node),
                command: *command,
                ids: ids_c,
                cols,
                in_groups,
                out_group,
                moves,
            },
            layout: out_layout,
        })
    }

    /// The pending seed base as the node of a bound-variable leaf, or an empty row.
    fn source_or_base(&mut self, _layout: Layout) -> PhysNode {
        match self.pending_base.take() {
            Some(base) => base,
            None => PhysNode::EmptyRow {
                group: self.alloc.new_group(),
            },
        }
    }

    /// A fresh one-lane group for a column computed from flat inputs only.
    fn scalar_group(&mut self) -> GroupId {
        self.alloc.new_group()
    }

    fn schema(&mut self, op: SchemaOp, layout: Layout) -> Result<Lowered, CypherError> {
        let group = self.alloc.new_group();
        Ok(Lowered {
            node: PhysNode::Schema { op, group },
            layout,
        })
    }

    fn show(&mut self, op: ShowOp, layout: Layout) -> Result<Lowered, CypherError> {
        let names: &[&str] = match op {
            ShowOp::Indexes | ShowOp::Constraints => {
                &["name", "type", "entityType", "labelsOrTypes", "properties"]
            }
            ShowOp::Functions => &[
                "name",
                "category",
                "description",
                "signature",
                "aggregating",
            ],
            ShowOp::Procedures => &["name", "description", "signature", "mode"],
            ShowOp::Settings => &["name", "value", "isDynamic", "defaultValue", "description"],
        };
        let g = self.alloc.new_group();
        let mut layout = layout;
        let cols: Vec<DataPos> = names
            .iter()
            .map(|n| layout.bind_in(&mut self.alloc, n, g, SlotType::Any))
            .collect();
        Ok(Lowered {
            node: self.source(PhysNode::Show {
                op,
                cols,
                out_group: g,
            }),
            layout,
        })
    }

    fn set_actions(
        &mut self,
        items: &[crate::ast::SetItem],
        layout: &Layout,
    ) -> Result<Vec<SetAction>, CypherError> {
        use crate::ast::SetItem;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(match item {
                SetItem::Property(access, expr) => match layout.pos(&access.expr_or_var) {
                    Some(entity) => SetAction::Property {
                        entity,
                        key: access.key.clone(),
                        value: self.compile(expr, layout)?,
                    },
                    None => SetAction::Unbound,
                },
                SetItem::AllProperties(var, expr) => match layout.pos(var) {
                    Some(entity) => SetAction::AllProperties {
                        entity,
                        value: self.compile(expr, layout)?,
                    },
                    None => SetAction::Unbound,
                },
                SetItem::MergeProperties(var, expr) => match layout.pos(var) {
                    Some(entity) => SetAction::MergeProperties {
                        entity,
                        value: self.compile(expr, layout)?,
                    },
                    None => SetAction::Unbound,
                },
                SetItem::Label(var, labels, dynamic) => match layout.pos(var) {
                    Some(entity) => {
                        let mut d = Vec::with_capacity(dynamic.len());
                        for e in dynamic {
                            d.push(self.compile(e, layout)?);
                        }
                        SetAction::Labels {
                            entity,
                            labels: labels.clone(),
                            dynamic: d,
                        }
                    }
                    None => SetAction::Unbound,
                },
                SetItem::DynamicProperty {
                    variable,
                    key,
                    value,
                } => match layout.pos(variable) {
                    Some(entity) => SetAction::DynamicProperty {
                        entity,
                        key: self.compile(key, layout)?,
                        value: self.compile(value, layout)?,
                    },
                    None => SetAction::Unbound,
                },
            });
        }
        Ok(out)
    }

    fn lower_limit(
        &mut self,
        l: Lowered,
        count: &Expr,
        materialize: bool,
    ) -> Result<Lowered, CypherError> {
        let count_c = self.compile(count, &l.layout)?;
        // a sort right below keeps only the limit's rows as it goes
        let Lowered { node, layout } = l;
        let node = match node {
            PhysNode::Sort {
                input,
                keys,
                in_groups,
                out_group,
                moves,
                limit: None,
            } if !materialize => PhysNode::Sort {
                input,
                keys,
                in_groups,
                out_group,
                moves,
                limit: Some(self.compile(count, &layout)?),
            },
            node => node,
        };
        let l = Lowered { node, layout };
        let in_groups = l.layout.groups();
        if !materialize && l.layout.unflat_groups().len() <= 1 {
            return Ok(Lowered {
                node: PhysNode::Limit {
                    input: Box::new(l.node),
                    count: count_c,
                    in_groups,
                    materialize: false,
                    consume: None,
                },
                layout: l.layout,
            });
        }
        let (_, out_group, out_layout, moves) = self.consume(&l.layout);
        Ok(Lowered {
            node: PhysNode::Limit {
                input: Box::new(l.node),
                count: count_c,
                in_groups,
                materialize,
                consume: Some(Consume { out_group, moves }),
            },
            layout: out_layout,
        })
    }

    /// Projection: a renaming when every item is a bound variable and nothing filters or
    /// deduplicates; otherwise a row consumer into a fresh flat group.
    fn lower_project(
        &mut self,
        l: Lowered,
        items: &[ProjectionItem],
        distinct: bool,
        where_clause: Option<&Expr>,
    ) -> Result<Lowered, CypherError> {
        if items.is_empty() {
            // RETURN * / WITH *: the scope is unchanged
            let mut l = l;
            if let Some(w) = where_clause {
                l = self.lower_filter(l, w)?;
            }
            if distinct {
                let (in_groups, out_group, out_layout, moves) = self.consume(&l.layout);
                return Ok(Lowered {
                    node: PhysNode::Project {
                        input: Box::new(l.node),
                        items: Vec::new(),
                        moves,
                        in_groups,
                        out_group,
                        distinct: true,
                        where_: None,
                    },
                    layout: out_layout,
                });
            }
            return Ok(l);
        }
        let names: Vec<String> = items.iter().map(crate::executor::projection_name).collect();
        let pure_rename = !distinct
            && where_clause.is_none()
            && items
                .iter()
                .all(|it| matches!(&it.expr, Expr::Variable(v) if l.layout.has(v)));
        if pure_rename {
            let l = self.capture(l);
            let mut out_layout = Layout::new();
            for (it, name) in items.iter().zip(&names) {
                let Expr::Variable(v) = &it.expr else {
                    unreachable!()
                };
                let pos = l.layout.pos(v).unwrap();
                let ty = l.layout.ty(v).unwrap_or(SlotType::Any);
                out_layout.bind(name, pos, ty);
            }
            for g in l.layout.groups() {
                if l.layout.is_flat(g) {
                    out_layout.mark_flat(g);
                }
            }
            return Ok(Lowered {
                node: l.node,
                layout: out_layout,
            });
        }
        let l = self.capture(l);
        let mut compiled = Vec::with_capacity(items.len());
        for it in items {
            compiled.push(self.compile(&it.expr, &l.layout)?);
        }
        let in_groups = l.layout.groups();
        let out_group = self.alloc.new_group();
        let mut out_layout = Layout::new();
        let mut outs = Vec::with_capacity(items.len());
        for (it, name) in items.iter().zip(&names) {
            let ty = match &it.expr {
                Expr::Variable(v) => l.layout.ty(v).unwrap_or(SlotType::Any),
                _ => SlotType::Any,
            };
            outs.push(out_layout.bind_in(&mut self.alloc, name, out_group, ty));
        }
        let items_c: Vec<(CompiledExpr, DataPos)> = compiled.into_iter().zip(outs).collect();
        // WITH ... WHERE sees the old bindings and the new ones; the new names shadow old ones
        let where_c = match where_clause {
            Some(w) => {
                let mut merged = l.layout.clone();
                for (name, pos, ty) in out_layout.bindings() {
                    merged.bind(name, pos, ty);
                }
                Some(self.compile(w, &merged)?)
            }
            None => None,
        };
        Ok(Lowered {
            node: PhysNode::Project {
                input: Box::new(l.node),
                items: items_c,
                moves: Vec::new(),
                in_groups,
                out_group,
                distinct,
                where_: where_c,
            },
            layout: out_layout,
        })
    }

    fn lower_aggregate(
        &mut self,
        l: Lowered,
        keys: &[ProjectionItem],
        aggs: &[ProjectionItem],
    ) -> Result<Lowered, CypherError> {
        let l = self.capture(l);
        let in_groups = l.layout.groups();
        let out_group = self.alloc.new_group();
        let mut out_layout = Layout::new();
        let mut key_c = Vec::with_capacity(keys.len());
        for k in keys {
            let name = crate::executor::projection_name(k);
            let expr = self.compile(&k.expr, &l.layout)?;
            let ty = match &k.expr {
                Expr::Variable(v) => l.layout.ty(v).unwrap_or(SlotType::Any),
                _ => SlotType::Any,
            };
            let out = out_layout.bind_in(&mut self.alloc, &name, out_group, ty);
            key_c.push((expr, out));
        }
        let mut specs = Vec::new();
        let mut post = Vec::new();
        let mut hidden = 0usize;
        for a in aggs {
            let name = crate::executor::projection_name(a);
            match &a.expr {
                Expr::FunctionCall {
                    name: fname,
                    distinct,
                    args,
                } if is_aggregate_function(fname) => {
                    let out = out_layout.bind_in(&mut self.alloc, &name, out_group, SlotType::Any);
                    let arg = match args.first() {
                        Some(e) => Some(self.compile(e, &l.layout)?),
                        None => None,
                    };
                    let mut extra = Vec::new();
                    for e in args.iter().skip(1) {
                        extra.push(self.compile(e, &l.layout)?);
                    }
                    specs.push(AggSpec {
                        func: fname.to_ascii_lowercase(),
                        distinct: *distinct,
                        arg,
                        extra,
                        out,
                    });
                }
                other => {
                    // aggregates nested in an expression: each into a hidden column, the
                    // expression over the finished group
                    let mut rewritten = other.clone();
                    let mut calls = Vec::new();
                    extract_agg_calls(&mut rewritten, &mut calls, &mut hidden);
                    for (var, call) in calls {
                        let Expr::FunctionCall {
                            name: fname,
                            distinct,
                            args,
                        } = &call
                        else {
                            unreachable!()
                        };
                        let out =
                            out_layout.bind_in(&mut self.alloc, &var, out_group, SlotType::Any);
                        let arg = match args.first() {
                            Some(e) => Some(self.compile(e, &l.layout)?),
                            None => None,
                        };
                        let mut extra = Vec::new();
                        for e in args.iter().skip(1) {
                            extra.push(self.compile(e, &l.layout)?);
                        }
                        specs.push(AggSpec {
                            func: fname.to_ascii_lowercase(),
                            distinct: *distinct,
                            arg,
                            extra,
                            out,
                        });
                    }
                    let out = out_layout.bind_in(&mut self.alloc, &name, out_group, SlotType::Any);
                    // the post expression reads the group's keys and hidden columns (both in the
                    // output layout) and, for its first row, the input bindings
                    let mut merged = l.layout.clone();
                    for (n, p, t) in out_layout.bindings() {
                        merged.bind(n, p, t);
                    }
                    let expr = self.compile(&rewritten, &merged)?;
                    post.push(PostAgg { expr, out });
                }
            }
        }
        Ok(Lowered {
            node: PhysNode::Aggregate {
                input: Box::new(l.node),
                keys: key_c,
                aggs: specs,
                post,
                in_groups,
                out_group,
            },
            layout: out_layout,
        })
    }

    /// A global count straight from a storage structure, when the aggregate is nothing but
    /// counts over a bare scan, edge scan or index range.
    fn try_count_source(
        &mut self,
        input: &LogicalPlan,
        keys: &[ProjectionItem],
        aggs: &[ProjectionItem],
        layout: &Layout,
    ) -> Result<Option<Lowered>, CypherError> {
        if !keys.is_empty() || aggs.is_empty() || self.pending_base.is_some() {
            return Ok(None);
        }
        let counts_only = |vars: &[&str]| {
            aggs.iter().all(|a| match &a.expr {
                Expr::FunctionCall {
                    name,
                    distinct,
                    args,
                } if name == "count" && !*distinct => {
                    args.is_empty()
                        || matches!(&args[0], Expr::Variable(v) if vars.contains(&v.as_str()))
                }
                _ => false,
            })
        };
        let kind = match self.filtered_hop_count(input, layout, &counts_only)? {
            Some(kind) => kind,
            None => match input {
                LogicalPlan::Scan { variable, label }
                    if !layout.has(variable) && counts_only(&[variable]) =>
                {
                    CountKind::Nodes {
                        label: label.clone(),
                    }
                }
                LogicalPlan::EdgeTypeScan {
                    from_var,
                    rel_var,
                    to_var,
                    edge_types,
                    direction,
                } if !layout.has(from_var)
                    && !layout.has(to_var)
                    && (rel_var.is_empty() || !layout.has(rel_var))
                    && counts_only(&[from_var, rel_var, to_var]) =>
                {
                    CountKind::Edges {
                        edge_types: edge_types.clone(),
                        direction: *direction,
                        from_label: None,
                        to_label: None,
                    }
                }
                // a label scan filtered on one property: the storage column counts it
                LogicalPlan::Filter {
                    input: scan,
                    predicate,
                } if matches!(**scan, LogicalPlan::Scan { label: Some(_), .. }) => {
                    let LogicalPlan::Scan { variable, label } = &**scan else {
                        unreachable!("a labelled scan");
                    };
                    let Some((property, op, value)) = simple_range_predicate(predicate, variable)
                    else {
                        return Ok(None);
                    };
                    if layout.has(variable) || !counts_only(&[variable]) {
                        return Ok(None);
                    }
                    CountKind::ColumnRange {
                        label: label.clone().unwrap_or_default(),
                        property,
                        op,
                        value: self.compile(value, layout)?,
                    }
                }
                // a single hop from a label scan, with at most a label check on the far end
                LogicalPlan::ExpandOut { .. }
                | LogicalPlan::ExpandIn { .. }
                | LogicalPlan::ExpandBoth { .. } => {
                    match labelled_hop(input, None, layout, &counts_only) {
                        Some(kind) => kind,
                        None => return Ok(None),
                    }
                }
                LogicalPlan::Filter {
                    input: hop,
                    predicate,
                } if matches!(
                    **hop,
                    LogicalPlan::ExpandOut { .. }
                        | LogicalPlan::ExpandIn { .. }
                        | LogicalPlan::ExpandBoth { .. }
                ) =>
                {
                    match labelled_hop(hop, Some(predicate), layout, &counts_only) {
                        Some(kind) => kind,
                        None => return Ok(None),
                    }
                }
                LogicalPlan::IndexRangeScan {
                    variable,
                    label,
                    property,
                    op,
                    value,
                } if !layout.has(variable) && counts_only(&[variable]) => CountKind::IndexRange {
                    label: label.clone(),
                    property: property.clone(),
                    op: *op,
                    value: self.compile(value, layout)?,
                },
                LogicalPlan::IndexScan {
                    variable,
                    label,
                    property,
                    value,
                } if !layout.has(variable) && counts_only(&[variable]) => CountKind::IndexEq {
                    label: label.clone(),
                    property: property.clone(),
                    value: self.compile(value, layout)?,
                },
                _ => return Ok(None),
            },
        };
        let out_group = self.alloc.new_group();
        let mut out_layout = Layout::new();
        let outs: Vec<DataPos> = aggs
            .iter()
            .map(|a| {
                let name = crate::executor::projection_name(a);
                out_layout.bind_in(&mut self.alloc, &name, out_group, SlotType::Int)
            })
            .collect();
        Ok(Some(Lowered {
            node: PhysNode::CountSource {
                kind,
                outs,
                out_group,
            },
            layout: out_layout,
        }))
    }

    /// A single hop from a label scan filtered on one property, nothing read but counts (and
    /// at most a label check on the far end): a [`CountKind::DegreeSum`].
    fn filtered_hop_count(
        &mut self,
        input: &LogicalPlan,
        layout: &Layout,
        counts_only: &dyn Fn(&[&str]) -> bool,
    ) -> Result<Option<CountKind>, CypherError> {
        let is_hop = |p: &LogicalPlan| {
            matches!(
                p,
                LogicalPlan::ExpandOut { .. }
                    | LogicalPlan::ExpandIn { .. }
                    | LogicalPlan::ExpandBoth { .. }
            )
        };
        let (hop, predicate) = match input {
            LogicalPlan::Filter {
                input: hop,
                predicate,
            } if is_hop(hop) => (&**hop, Some(predicate)),
            hop if is_hop(hop) => (hop, None),
            _ => return Ok(None),
        };
        let (hop_input, from, rel_var, to, edge_types, range, dir) = match hop {
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                ..
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                Direction::Outgoing,
            ),
            LogicalPlan::ExpandIn {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                ..
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                Direction::Incoming,
            ),
            LogicalPlan::ExpandBoth {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                ..
            } => (input, from, rel_var, to, edge_types, range, Direction::Both),
            _ => unreachable!("a hop"),
        };
        if range.is_some() || from == to {
            return Ok(None);
        }
        let LogicalPlan::Filter {
            input: scan,
            predicate: src_pred,
        } = &**hop_input
        else {
            return Ok(None);
        };
        let LogicalPlan::Scan {
            variable,
            label: Some(label),
        } = &**scan
        else {
            return Ok(None);
        };
        if variable != from
            || layout.has(from)
            || layout.has(to)
            || (!rel_var.is_empty() && layout.has(rel_var))
            || !counts_only(&[from, rel_var, to])
        {
            return Ok(None);
        }
        let Some((property, op, value)) = simple_range_predicate(src_pred, variable) else {
            return Ok(None);
        };
        let mut to_label: Option<String> = None;
        if let Some(pred) = predicate {
            let mut labels = Vec::new();
            if !collect_label_checks(pred, to, &mut labels) || labels.len() != 1 {
                return Ok(None);
            }
            to_label = labels.pop();
        }
        Ok(Some(CountKind::DegreeSum {
            label: label.clone(),
            property,
            op,
            value: self.compile(value, layout)?,
            dir,
            edge_types: edge_types.clone(),
            to_label,
        }))
    }

    fn lower_expand(&mut self, plan: &LogicalPlan, layout: Layout) -> Result<Lowered, CypherError> {
        let (input, from, rel_var, to, edge_types, range, prior_edge_vars, dir) = match plan {
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Outgoing,
            ),
            LogicalPlan::ExpandIn {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Incoming,
            ),
            LogicalPlan::ExpandBoth {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Both,
            ),
            _ => unreachable!(),
        };
        let l = self.lower(input, layout)?;
        let from_pos = l.layout.pos(from).ok_or_else(|| unbound(from))?;
        let bound_to = l.layout.pos(to);
        let prior: Vec<DataPos> = prior_edge_vars
            .iter()
            .filter_map(|v| l.layout.pos(v))
            .collect();
        let mut positions = vec![from_pos];
        positions.extend(prior.iter().copied());
        if let Some(b) = bound_to {
            positions.push(b);
        }
        // a single hop from an unflat group fills that group in place (a chunk of sources
        // expands in a few chunks instead of one per lane) when everything else read here is
        // flat
        let in_place = range.is_none()
            && !l.layout.is_flat(from_pos.group)
            && l.layout.unflat_among(&positions) == [from_pos.group];
        let l = if in_place {
            l
        } else {
            self.flatten_for(l, &positions)
        };
        let g = if in_place {
            from_pos.group
        } else {
            self.alloc.new_group()
        };
        let mut layout = l.layout;
        let is_var = range.is_some();
        let rel = layout.bind_in(
            &mut self.alloc,
            rel_var,
            g,
            if is_var {
                SlotType::List
            } else {
                SlotType::Edge
            },
        );
        let to_pos = layout.bind_in(&mut self.alloc, to, g, SlotType::Node);
        let node = if let Some((min_opt, max_opt)) = range {
            PhysNode::VarExpand {
                input: Box::new(l.node),
                from: from_pos,
                rel,
                to: to_pos,
                out_group: g,
                dir,
                edge_types: edge_types.clone(),
                min: min_opt.unwrap_or(1) as usize,
                max: max_opt.unwrap_or(30) as usize,
                prior_edges: prior,
                bound_to,
            }
        } else {
            PhysNode::Expand {
                input: Box::new(l.node),
                from: from_pos,
                rel,
                to: to_pos,
                out_group: g,
                dir,
                edge_types: edge_types.clone(),
                prior_edges: prior,
                bound_to,
                in_place,
            }
        };
        Ok(Lowered { node, layout })
    }

    /// Lowers a body run per outer row, seeded with `seed` (and the locals, which follow the
    /// seed's variables in the seed row).
    fn lower_subplan(
        &mut self,
        body: &LogicalPlan,
        seed: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        let mut inner = Lowerer::new(self.graph);
        let g = inner.alloc.new_group();
        let mut layout = Layout::new();
        let mut cols = Vec::new();
        let mut seeds = Vec::new();
        for (name, pos, ty) in seed.bindings() {
            let p = layout.bind_in(&mut inner.alloc, name, g, ty);
            cols.push(p);
            seeds.push((SeedSource::Outer(pos), p));
        }
        for (i, name) in locals.iter().enumerate() {
            let p = layout.bind_in(&mut inner.alloc, name, g, SlotType::Any);
            cols.push(p);
            seeds.push((SeedSource::Local(i as u32), p));
        }
        inner.pending_base = Some(PhysNode::Seed { group: g, cols });
        let out = inner.lower(body, layout)?;
        let out_names: Vec<(String, DataPos)> = out
            .layout
            .bindings()
            .map(|(n, p, _)| (n.to_string(), p))
            .collect();
        let out_groups = out.layout.groups();
        Ok(Arc::new(SubPlan {
            root: out.node,
            widths: inner.alloc.widths().to_vec(),
            seed_group: g,
            seeds,
            out: out_names,
            out_groups,
            layout: out.layout,
        }))
    }
}

/// `count(*)` over `(:L1)-[:T]->(:L2)`: one hop out of a label scan, the far end checked only
/// for a label, nothing bound before. `None` when the shape is anything else.
fn labelled_hop(
    hop: &LogicalPlan,
    predicate: Option<&Expr>,
    layout: &Layout,
    counts_only: &dyn Fn(&[&str]) -> bool,
) -> Option<CountKind> {
    let (input, from, rel_var, to, edge_types, range, direction) = match hop {
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => (
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            Direction::Outgoing,
        ),
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => (
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            Direction::Incoming,
        ),
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => (input, from, rel_var, to, edge_types, range, Direction::Both),
        _ => return None,
    };
    if range.is_some() || from == to {
        return None;
    }
    let LogicalPlan::Scan {
        variable,
        label: from_label,
    } = &**input
    else {
        return None;
    };
    if variable != from
        || layout.has(from)
        || layout.has(to)
        || (!rel_var.is_empty() && layout.has(rel_var))
        || !counts_only(&[from, rel_var, to])
    {
        return None;
    }
    // the predicate: label checks on the far end only
    let mut to_label: Option<String> = None;
    if let Some(pred) = predicate {
        let mut labels = Vec::new();
        if !collect_label_checks(pred, to, &mut labels) || labels.len() != 1 {
            return None;
        }
        to_label = labels.pop();
    }
    Some(CountKind::Edges {
        edge_types: edge_types.clone(),
        direction,
        from_label: from_label.clone(),
        to_label,
    })
}

/// Collects the labels `pred` checks on `var` when it is a conjunction of such checks.
fn collect_label_checks(pred: &Expr, var: &str, out: &mut Vec<String>) -> bool {
    match pred {
        Expr::BinaryOp(l, crate::ast::BinOp::And, r) => {
            collect_label_checks(l, var, out) && collect_label_checks(r, var, out)
        }
        Expr::FunctionCall { name, args, .. } if name == "__has_label" => match args.as_slice() {
            [Expr::Variable(v), Expr::Literal(Literal::String(label))] if v == var => {
                out.push(label.clone());
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// The position of `name` in a sub-plan's output.
fn body_pos(sp: &SubPlan, name: &str) -> DataPos {
    sp.layout
        .pos(name)
        .unwrap_or(DataPos::new(u16::MAX, u16::MAX))
}

fn body_layout_ty(sp: &SubPlan, name: &str) -> SlotType {
    sp.layout.ty(name).unwrap_or(SlotType::Any)
}

/// `var.prop <op> <literal or parameter>` (either way round) as `(prop, op, bound)`.
fn simple_range_predicate<'a>(pred: &'a Expr, var: &str) -> Option<(String, CmpOp, &'a Expr)> {
    let Expr::Comparison(lhs, pairs) = pred else {
        return None;
    };
    if pairs.len() != 1 {
        return None;
    }
    let (op, rhs) = (&pairs[0].0, &pairs[0].1);
    let prop_of = |e: &Expr| match e {
        Expr::Property(base, key) if matches!(&**base, Expr::Variable(v) if v == var) => {
            Some(key.clone())
        }
        _ => None,
    };
    let is_bound = |e: &Expr| matches!(e, Expr::Literal(_) | Expr::Parameter(_));
    if let Some(p) = prop_of(lhs)
        && is_bound(rhs)
    {
        return Some((p, *op, rhs));
    }
    if let Some(p) = prop_of(rhs)
        && is_bound(lhs)
    {
        let flipped = match op {
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Lte => CmpOp::Gte,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Gte => CmpOp::Lte,
            other => *other,
        };
        return Some((p, flipped, lhs));
    }
    None
}

/// Does `e` read a variable `pred` accepts? Anything the walk doesn't know counts as a read.
fn mentions_variable(e: &Expr, pred: &dyn Fn(&str) -> bool) -> bool {
    match e {
        Expr::Literal(_) | Expr::Parameter(_) => false,
        Expr::Variable(v) => pred(v),
        Expr::Property(inner, _)
        | Expr::UnaryOp(_, inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => mentions_variable(inner, pred),
        Expr::BinaryOp(l, _, r)
        | Expr::In(l, r)
        | Expr::StringOp(l, _, r)
        | Expr::ListIndex(l, r) => mentions_variable(l, pred) || mentions_variable(r, pred),
        Expr::Comparison(first, rest) => {
            mentions_variable(first, pred) || rest.iter().any(|(_, e)| mentions_variable(e, pred))
        }
        Expr::FunctionCall { args, .. } => args.iter().any(|a| mentions_variable(a, pred)),
        Expr::ListLiteral(items) => items.iter().any(|a| mentions_variable(a, pred)),
        Expr::MapLiteral(items) => items.iter().any(|(_, a)| mentions_variable(a, pred)),
        _ => true,
    }
}

fn unbound(name: &str) -> CypherError {
    CypherError::semantic(format!("variable `{name}` is not bound here"))
        .with_status(crate::status::GqlStatus::VariableNotDefined)
}

/// A conjunct `variable.p = c` (either side) of `pred` with `c` a literal or parameter:
/// `(p, c)`.
fn property_eq_constant<'e>(pred: &'e Expr, variable: &str) -> Option<(&'e str, &'e Expr)> {
    match pred {
        Expr::BinaryOp(l, crate::ast::BinOp::And, r) => {
            property_eq_constant(l, variable).or_else(|| property_eq_constant(r, variable))
        }
        Expr::Comparison(lhs, ops) if ops.len() == 1 && ops[0].0 == crate::ast::CmpOp::Eq => {
            let rhs = &ops[0].1;
            let prop_of = |e: &'e Expr| match e {
                Expr::Property(base, p) => {
                    matches!(&**base, Expr::Variable(v) if v == variable).then_some(p.as_str())
                }
                _ => None,
            };
            let constant = |e: &Expr| matches!(e, Expr::Literal(_) | Expr::Parameter(_));
            if let Some(p) = prop_of(lhs)
                && constant(rhs)
            {
                Some((p, rhs))
            } else if let Some(p) = prop_of(rhs)
                && constant(lhs)
            {
                Some((p, &**lhs))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn eq_expr(l: Expr, r: Expr) -> Expr {
    Expr::Comparison(Box::new(l), vec![(crate::ast::CmpOp::Eq, r)])
}

/// Replaces every aggregate call in `expr` with a hidden variable, collecting the calls.
#[recursive::recursive]
fn extract_agg_calls(expr: &mut Expr, calls: &mut Vec<(String, Expr)>, counter: &mut usize) {
    if let Expr::FunctionCall { name, .. } = expr
        && is_aggregate_function(name)
    {
        let var = format!("__agg_{counter}");
        *counter += 1;
        let call = std::mem::replace(expr, Expr::Variable(var.clone()));
        calls.push((var, call));
        return;
    }
    match expr {
        Expr::BinaryOp(l, _, r) => {
            extract_agg_calls(l, calls, counter);
            extract_agg_calls(r, calls, counter);
        }
        Expr::UnaryOp(_, e) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
            extract_agg_calls(e, calls, counter)
        }
        Expr::Property(b, _) => extract_agg_calls(b, calls, counter),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                extract_agg_calls(a, calls, counter);
            }
        }
        Expr::MapLiteral(pairs) => {
            for (_, e) in pairs {
                extract_agg_calls(e, calls, counter);
            }
        }
        Expr::ListLiteral(items) => {
            for e in items {
                extract_agg_calls(e, calls, counter);
            }
        }
        Expr::Comparison(b, pairs) => {
            extract_agg_calls(b, calls, counter);
            for (_, e) in pairs {
                extract_agg_calls(e, calls, counter);
            }
        }
        Expr::In(l, r) | Expr::StringOp(l, _, r) | Expr::ListIndex(l, r) => {
            extract_agg_calls(l, calls, counter);
            extract_agg_calls(r, calls, counter);
        }
        Expr::ListSlice(b, f, t) => {
            extract_agg_calls(b, calls, counter);
            if let Some(f) = f {
                extract_agg_calls(f, calls, counter);
            }
            if let Some(t) = t {
                extract_agg_calls(t, calls, counter);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                extract_agg_calls(o, calls, counter);
            }
            for (w, t) in whens {
                extract_agg_calls(w, calls, counter);
                extract_agg_calls(t, calls, counter);
            }
            if let Some(e) = else_expr {
                extract_agg_calls(e, calls, counter);
            }
        }
        Expr::ListComprehension {
            list_expr,
            filter,
            map_expr,
            ..
        } => {
            extract_agg_calls(list_expr, calls, counter);
            if let Some(f) = filter {
                extract_agg_calls(f, calls, counter);
            }
            if let Some(m) = map_expr {
                extract_agg_calls(m, calls, counter);
            }
        }
        Expr::QuantifierExpr {
            list_expr,
            predicate,
            ..
        } => {
            extract_agg_calls(list_expr, calls, counter);
            extract_agg_calls(predicate, calls, counter);
        }
        Expr::IsTyped { expr, .. } => extract_agg_calls(expr, calls, counter),
        _ => {}
    }
}

/// Subquery lowering for expressions: builds the logical plan of the body and lowers it with
/// the outer variables it references as seeds.
struct SubLowerer<'g> {
    graph: &'g dyn GraphView,
    hints: &'g IndexHints,
}

impl SubLowerer<'_> {
    fn seeds_for(
        &self,
        refs: &HashSet<String>,
        outer: &Layout,
        locals: &[String],
    ) -> (Layout, Vec<String>) {
        let mut seed = Layout::new();
        let mut local_seeds = Vec::new();
        for name in refs {
            if let Some(i) = locals.iter().rposition(|l| l == name) {
                let _ = i;
                local_seeds.push(name.clone());
            } else if let (Some(p), Some(t)) = (outer.pos(name), outer.ty(name)) {
                seed.bind(name, p, t);
            }
        }
        (seed, local_seeds)
    }

    fn lower_seeded(
        &mut self,
        plan: &LogicalPlan,
        seed: &Layout,
        local_seeds: &[String],
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        let mut inner = Lowerer::new(self.graph);
        inner.hints = self.hints.clone();
        let g = inner.alloc.new_group();
        let mut layout = Layout::new();
        let mut cols = Vec::new();
        let mut seeds = Vec::new();
        for (name, pos, ty) in seed.bindings() {
            let p = layout.bind_in(&mut inner.alloc, name, g, ty);
            cols.push(p);
            seeds.push((SeedSource::Outer(pos), p));
        }
        for name in local_seeds {
            let idx = locals.iter().rposition(|l| l == name).unwrap_or(0);
            let p = layout.bind_in(&mut inner.alloc, name, g, SlotType::Any);
            cols.push(p);
            seeds.push((SeedSource::Local(idx as u32), p));
        }
        inner.pending_base = Some(PhysNode::Seed { group: g, cols });
        let out = inner.lower(plan, layout)?;
        let out_names: Vec<(String, DataPos)> = out
            .layout
            .bindings()
            .map(|(n, p, _)| (n.to_string(), p))
            .collect();
        let out_groups = out.layout.groups();
        Ok(Arc::new(SubPlan {
            root: out.node,
            widths: inner.alloc.widths().to_vec(),
            seed_group: g,
            seeds,
            out: out_names,
            out_groups,
            layout: out.layout,
        }))
    }
}

impl SubqueryLowerer for SubLowerer<'_> {
    fn lower_pattern_subquery(
        &mut self,
        mc: &MatchClause,
        outer: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        let plan = super::subquery::pattern_subquery_plan(mc)?;
        let refs = super::subquery::pattern_subquery_refs(mc, None);
        let (seed, local_seeds) = self.seeds_for(&refs, outer, locals);
        self.lower_seeded(&plan, &seed, &local_seeds, locals)
    }

    fn lower_full_subquery(
        &mut self,
        sq: &SingleQuery,
        outer: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        let plan = super::subquery::full_subquery_plan(sq)?;
        let refs = super::subquery::full_subquery_refs(sq);
        let (seed, local_seeds) = self.seeds_for(&refs, outer, locals);
        let sp = self.lower_seeded(&plan, &seed, &local_seeds, locals)?;
        // COLLECT / EXISTS over a full query: the body's RETURN columns are the output, the seeds
        // are not
        let columns = crate::executor::plan_columns(&plan);
        if !columns.is_empty() {
            let out: Vec<(String, DataPos)> = columns
                .iter()
                .filter_map(|c| sp.layout.pos(c).map(|p| (c.clone(), p)))
                .collect();
            let mut sp = Arc::try_unwrap(sp).unwrap_or_else(|_| unreachable!());
            sp.out = out;
            return Ok(Arc::new(sp));
        }
        Ok(sp)
    }

    fn lower_pattern_comprehension(
        &mut self,
        path_var: Option<&str>,
        mc: &MatchClause,
        projection: &Expr,
        outer: &Layout,
        locals: &[String],
    ) -> Result<Arc<SubPlan>, CypherError> {
        let plan =
            super::subquery::pattern_comprehension_plan(path_var, mc, projection, self.graph)?;
        let refs = super::subquery::pattern_subquery_refs(mc, Some(projection));
        let (seed, local_seeds) = self.seeds_for(&refs, outer, locals);
        let sp = self.lower_seeded(&plan, &seed, &local_seeds, locals)?;
        let out = sp
            .layout
            .pos(super::subquery::PC_OUT)
            .map(|p| vec![(super::subquery::PC_OUT.to_string(), p)])
            .unwrap_or_default();
        let mut sp = Arc::try_unwrap(sp).unwrap_or_else(|_| unreachable!());
        sp.out = out;
        Ok(Arc::new(sp))
    }
}

/// Direction of an undirected pattern written as `RelDirection`.
pub fn rel_direction(d: &RelDirection) -> Direction {
    match d {
        RelDirection::Outgoing => Direction::Outgoing,
        RelDirection::Incoming => Direction::Incoming,
        RelDirection::Both => Direction::Both,
    }
}
