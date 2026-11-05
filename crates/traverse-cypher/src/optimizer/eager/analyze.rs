//! The plan walk: summarizes each operator's reads and writes and places `Eager` barriers
//! where a later read could see an earlier write.

use crate::ast::{Expr, ProjectionItem};
use crate::planner::LogicalPlan;

use super::reads::{
    expr_kind, expr_reads, merge_pattern_effects, projection_items_reads, remove_item_effects,
    set_clause_effects, set_item_effects,
};
use super::{Kind, PropKind, Reads, VarKinds, Writes, conflicts, eager};

/// What's still in flight when a row leaves a subtree.
#[derive(Default)]
pub(super) struct Summary {
    reads: Reads,
    writes: Writes,
}

impl Summary {
    fn settled() -> Self {
        Self::default()
    }
}

/// A nested update body (FOREACH, CALL { ... } IN TRANSACTIONS) as a unit: its own barriers plus its
/// total effect outside.
#[allow(clippy::boxed_local)] // the callers' plan fields are boxed (flagged only on 32-bit targets)
fn analyze_body(body: Box<LogicalPlan>, ctx: &mut VarKinds) -> (Box<LogicalPlan>, Summary) {
    let (plan, summary) = analyze(*body, ctx);
    (Box::new(plan), summary)
}

/// Bottom-up pass: rewritten plan + what's in flight at its output.
#[recursive::recursive]
pub(super) fn analyze(plan: LogicalPlan, ctx: &mut VarKinds) -> (LogicalPlan, Summary) {
    use LogicalPlan as P;
    match plan {
        // -- leaves: scans bind their variable and read their label --
        P::Scan { variable, label } => {
            let mut s = Summary::default();
            match &label {
                Some(l) => {
                    s.reads.node_labels.insert(l.clone());
                }
                None => s.reads.all_nodes = true,
            }
            ctx.bind(&variable, Kind::Node(label.iter().cloned().collect()));
            (P::Scan { variable, label }, s)
        }
        P::ScanFilterProject {
            variable,
            label,
            predicate,
            items,
            distinct,
            limit,
        } => {
            let mut s = Summary::default();
            match &label {
                Some(l) => {
                    s.reads.node_labels.insert(l.clone());
                }
                None => s.reads.all_nodes = true,
            }
            ctx.bind(&variable, Kind::Node(label.iter().cloned().collect()));
            if let Some(p) = &predicate {
                expr_reads(p, ctx, &mut s.reads);
            }
            projection_items_reads(&items, ctx, &mut s.reads);
            bind_projection(&items, ctx);
            (
                P::ScanFilterProject {
                    variable,
                    label,
                    predicate,
                    items,
                    distinct,
                    limit,
                },
                s,
            )
        }
        P::IndexScan {
            variable,
            label,
            property,
            value,
        } => {
            let mut s = Summary::default();
            s.reads.node_labels.insert(label.clone());
            let kind = Kind::Node(vec![label.clone()]);
            s.reads.read_prop(&property, &kind);
            expr_reads(&value, ctx, &mut s.reads);
            ctx.bind(&variable, kind);
            (
                P::IndexScan {
                    variable,
                    label,
                    property,
                    value,
                },
                s,
            )
        }
        P::IndexRangeScan {
            variable,
            label,
            property,
            op,
            value,
        } => {
            let mut s = Summary::default();
            s.reads.node_labels.insert(label.clone());
            let kind = Kind::Node(vec![label.clone()]);
            s.reads.read_prop(&property, &kind);
            expr_reads(&value, ctx, &mut s.reads);
            ctx.bind(&variable, kind);
            (
                P::IndexRangeScan {
                    variable,
                    label,
                    property,
                    op,
                    value,
                },
                s,
            )
        }
        P::TextIndexScan {
            variable,
            label,
            property,
            kind,
            pattern,
        } => {
            let mut s = Summary::default();
            s.reads.node_labels.insert(label.clone());
            let k = Kind::Node(vec![label.clone()]);
            s.reads.read_prop(&property, &k);
            expr_reads(&pattern, ctx, &mut s.reads);
            ctx.bind(&variable, k);
            (
                P::TextIndexScan {
                    variable,
                    label,
                    property,
                    kind,
                    pattern,
                },
                s,
            )
        }
        P::PointIndexScan {
            variable,
            label,
            property,
            query,
        } => {
            let mut s = Summary::default();
            s.reads.node_labels.insert(label.clone());
            let k = Kind::Node(vec![label.clone()]);
            s.reads.read_prop(&property, &k);
            ctx.bind(&variable, k);
            (
                P::PointIndexScan {
                    variable,
                    label,
                    property,
                    query,
                },
                s,
            )
        }
        P::NodeById {
            variable,
            label,
            id_expr,
        } => {
            let mut s = Summary::default();
            if let Some(l) = &label {
                s.reads.node_labels.insert(l.clone());
            }
            expr_reads(&id_expr, ctx, &mut s.reads);
            ctx.bind(&variable, Kind::Node(label.iter().cloned().collect()));
            (
                P::NodeById {
                    variable,
                    label,
                    id_expr,
                },
                s,
            )
        }
        P::NodeByIdRange {
            variable,
            label,
            lower,
            upper,
        } => {
            let mut s = Summary::default();
            if let Some(l) = &label {
                s.reads.node_labels.insert(l.clone());
            }
            ctx.bind(&variable, Kind::Node(label.iter().cloned().collect()));
            (
                P::NodeByIdRange {
                    variable,
                    label,
                    lower,
                    upper,
                },
                s,
            )
        }
        P::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
        } => {
            let mut s = Summary::default();
            s.reads.read_rel_types(&edge_types);
            ctx.bind(&from_var, Kind::Node(Vec::new()));
            ctx.bind(&to_var, Kind::Node(Vec::new()));
            ctx.bind(&rel_var, Kind::Rel(edge_types.clone()));
            (
                P::EdgeTypeScan {
                    from_var,
                    rel_var,
                    to_var,
                    edge_types,
                    direction,
                },
                s,
            )
        }
        P::EmptyRow | P::ContextSeed => (plan, Summary::settled()),

        // -- streaming unary readers --
        P::Filter { input, predicate } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            expr_reads(&predicate, ctx, &mut own);
            refine_labels_from_predicate(&predicate, ctx);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (P::Filter { input, predicate }, s)
        }
        P::Project {
            input,
            items,
            distinct,
            where_clause,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            projection_items_reads(&items, ctx, &mut own);
            if let Some(w) = &where_clause {
                expr_reads(w, ctx, &mut own);
            }
            bind_projection(&items, ctx);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::Project {
                    input,
                    items,
                    distinct,
                    where_clause,
                },
                s,
            )
        }
        P::Unwind {
            input,
            expr,
            alias,
            clear_source,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            expr_reads(&expr, ctx, &mut own);
            ctx.bind(&alias, unwind_kind(&expr, ctx));
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::Unwind {
                    input,
                    expr,
                    alias,
                    clear_source,
                },
                s,
            )
        }
        P::Skip { input, count } => {
            let (input, s) = analyze_boxed(input, ctx);
            (P::Skip { input, count }, s)
        }
        P::Limit { input, count } => {
            // over a mutating input the executor materializes (LIMIT mustn't cut writes short),
            // settling everything below
            let mutating = crate::optimizer::is_mutating_plan(&input);
            let (input, s) = analyze_boxed(input, ctx);
            let s = if mutating { Summary::settled() } else { s };
            (P::Limit { input, count }, s)
        }
        P::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let own = expand_reads(&rel_var, &to, &edge_types, ctx);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::ExpandOut {
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                },
                s,
            )
        }
        P::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let own = expand_reads(&rel_var, &to, &edge_types, ctx);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::ExpandIn {
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                },
                s,
            )
        }
        P::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let own = expand_reads(&rel_var, &to, &edge_types, ctx);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::ExpandBoth {
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                },
                s,
            )
        }
        P::DistinctExpand {
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
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            own.read_rel_types(&edge_types);
            if let Some(l) = &target_label {
                own.node_labels.insert(l.clone());
            }
            ctx.bind_if_new(&to, Kind::Node(target_label.iter().cloned().collect()));
            if let Some(f) = &target_filter {
                expr_reads(f, ctx, &mut own);
            }
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::DistinctExpand {
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
                },
                s,
            )
        }
        P::IndexLookupJoin {
            input,
            variable,
            label,
            property,
            key,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            own.node_labels.insert(label.clone());
            let kind = Kind::Node(vec![label.clone()]);
            own.read_prop(&property, &kind);
            expr_reads(&key, ctx, &mut own);
            ctx.bind(&variable, kind);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::IndexLookupJoin {
                    input,
                    variable,
                    label,
                    property,
                    key,
                },
                s,
            )
        }
        P::ShortestPath {
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
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            own.read_rel_types(&edge_types);
            if let Some(v) = &rel_var {
                ctx.bind(v, Kind::Unknown);
            }
            if let Some(v) = &path_var {
                ctx.bind(v, Kind::Unknown);
            }
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::ShortestPath {
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
                },
                s,
            )
        }
        P::WeightedShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path_var,
            rel_var,
            cost_var,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            own.read_rel_types(&edge_types);
            own.read_prop(&weight_property, &Kind::Rel(edge_types.clone()));
            if let Some(v) = &rel_var {
                ctx.bind(v, Kind::Unknown);
            }
            if let Some(v) = &path_var {
                ctx.bind(v, Kind::Unknown);
            }
            if let Some(v) = &cost_var {
                ctx.bind(v, Kind::Value);
            }
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::WeightedShortestPath {
                    input,
                    from,
                    to,
                    edge_types,
                    direction,
                    weight_property,
                    path_var,
                    rel_var,
                    cost_var,
                },
                s,
            )
        }
        P::VectorSearchScan {
            input,
            variable,
            index_name,
            query_vector,
            index_where,
            limit,
            score_var,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let own = Reads {
                everything: true,
                ..Default::default()
            };
            ctx.bind(&variable, Kind::Node(Vec::new()));
            if let Some(v) = &score_var {
                ctx.bind(v, Kind::Value);
            }
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::VectorSearchScan {
                    input,
                    variable,
                    index_name,
                    query_vector,
                    index_where,
                    limit,
                    score_var,
                },
                s,
            )
        }
        P::CallProcedure {
            input,
            procedure_name,
            args,
            yield_columns,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads {
                everything: true,
                ..Default::default()
            };
            if let Some(args) = &args {
                for a in args {
                    expr_reads(a, ctx, &mut own);
                }
            }
            for (_, var) in &yield_columns {
                ctx.bind(var, Kind::Unknown);
            }
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            if procedure_writes(&procedure_name) {
                s.writes.everything = true;
            }
            (
                P::CallProcedure {
                    input,
                    procedure_name,
                    args,
                    yield_columns,
                },
                s,
            )
        }
        // binders read structure only (endpoints of bound edges, path assembly); DELETE, the only
        // write that changes it, is a barrier itself
        P::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } => {
            let (input, s) = analyze_boxed(input, ctx);
            ctx.bind(&variable, Kind::Unknown);
            (
                P::BindPath {
                    input,
                    variable,
                    node_vars,
                    edge_vars,
                },
                s,
            )
        }
        P::BindVarLists { input, groups } => {
            let (input, s) = analyze_boxed(input, ctx);
            (P::BindVarLists { input, groups }, s)
        }
        P::BindQppGroups {
            input,
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        } => {
            let (input, s) = analyze_boxed(input, ctx);
            if let Some(v) = &src_var {
                ctx.bind(v, Kind::Unknown);
            }
            if let Some(v) = &dst_var {
                ctx.bind(v, Kind::Unknown);
            }
            (
                P::BindQppGroups {
                    input,
                    anchor_var,
                    rel_var,
                    src_var,
                    dst_var,
                },
                s,
            )
        }
        P::BindPathEndpoints {
            input,
            rel_var,
            from_var,
            to_var,
            direction,
        } => {
            let (input, s) = analyze_boxed(input, ctx);
            ctx.bind_if_new(&from_var, Kind::Node(Vec::new()));
            ctx.bind_if_new(&to_var, Kind::Node(Vec::new()));
            (
                P::BindPathEndpoints {
                    input,
                    rel_var,
                    from_var,
                    to_var,
                    direction,
                },
                s,
            )
        }
        P::BindEdgeEndpoints {
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        } => {
            let (input, s) = analyze_boxed(input, ctx);
            ctx.bind_if_new(&from_var, Kind::Node(Vec::new()));
            ctx.bind_if_new(&to_var, Kind::Node(Vec::new()));
            (
                P::BindEdgeEndpoints {
                    input,
                    edge_var,
                    from_var,
                    to_var,
                    direction,
                    edge_types,
                },
                s,
            )
        }
        P::LoadCsv {
            input,
            url,
            with_headers,
            row_alias,
            field_terminator,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            expr_reads(&url, ctx, &mut own);
            ctx.bind(&row_alias, Kind::Value);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            s.reads.merge(&own);
            (
                P::LoadCsv {
                    input,
                    url,
                    with_headers,
                    row_alias,
                    field_terminator,
                },
                s,
            )
        }

        // -- materializing readers: input settles before they emit, but their own expressions run
        // while draining --
        P::Sort { input, items } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            for it in &items {
                expr_reads(&it.expr, ctx, &mut own);
            }
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            (P::Sort { input, items }, Summary::settled())
        }
        P::Aggregate { input, keys, aggs } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            projection_items_reads(&keys, ctx, &mut own);
            projection_items_reads(&aggs, ctx, &mut own);
            bind_projection(&keys, ctx);
            bind_projection(&aggs, ctx);
            let input = barrier_if(conflicts(&own, &s.writes), input, &mut s);
            (P::Aggregate { input, keys, aggs }, Summary::settled())
        }
        P::Eager { input } => {
            let (input, _) = analyze_boxed(input, ctx);
            (P::Eager { input }, Summary::settled())
        }
        P::Union { left, right, all } => {
            let (left, _) = analyze_boxed(left, ctx);
            let (right, _) = analyze_boxed(right, ctx);
            (P::Union { left, right, all }, Summary::settled())
        }
        P::CrossProduct { left, right } => {
            // left drains, then right drains, then pairs emit: everything below is settled
            let (left, _) = analyze_boxed(left, ctx);
            let (right, _) = analyze_boxed(right, ctx);
            (P::CrossProduct { left, right }, Summary::settled())
        }
        P::HashJoin {
            left,
            right,
            left_keys,
            right_keys,
        } => {
            // build side (left) drains first with its keys; probe side (right) streams with its
            // keys
            let (left, mut sl) = analyze_boxed(left, ctx);
            let mut lk = Reads::default();
            for k in &left_keys {
                expr_reads(k, ctx, &mut lk);
            }
            let left = barrier_if(conflicts(&lk, &sl.writes), left, &mut sl);
            let (right, mut sr) = analyze_boxed(right, ctx);
            let mut rk = Reads::default();
            for k in &right_keys {
                expr_reads(k, ctx, &mut rk);
            }
            let right = barrier_if(conflicts(&rk, &sr.writes), right, &mut sr);
            sr.reads.merge(&rk);
            (
                P::HashJoin {
                    left,
                    right,
                    left_keys,
                    right_keys,
                },
                sr,
            )
        }
        P::OptionalMatch {
            input,
            match_plan,
            new_variables,
        } => {
            // input drains first; the match runs per row, so all its reads are in flight at the
            // output
            let (input, _) = analyze_boxed(input, ctx);
            let (match_plan, _) = analyze_boxed(match_plan, ctx);
            let mut s = Summary::settled();
            all_reads(&match_plan, ctx, &mut s.reads);
            (
                P::OptionalMatch {
                    input,
                    match_plan,
                    new_variables,
                },
                s,
            )
        }
        P::CallSubquery {
            input,
            imports,
            body,
            return_aliases,
        } => {
            // outer rows drain first; the body runs per outer row as its own plan: leave it
            let (input, _) = analyze_boxed(input, ctx);
            for a in &return_aliases {
                ctx.bind(a, Kind::Unknown);
            }
            (
                P::CallSubquery {
                    input,
                    imports,
                    body,
                    return_aliases,
                },
                Summary::settled(),
            )
        }
        P::CallInTransactions {
            input,
            body,
            batch_size,
            on_error,
            report_status,
            return_aliases,
        } => {
            let (input, _) = analyze_boxed(input, ctx);
            let mut inner = ctx.clone();
            let (body, _) = analyze_body(body, &mut inner);
            for a in &return_aliases {
                ctx.bind(a, Kind::Unknown);
            }
            if let Some(v) = &report_status {
                ctx.bind(v, Kind::Value);
            }
            (
                P::CallInTransactions {
                    input,
                    body,
                    batch_size,
                    on_error,
                    report_status,
                    return_aliases,
                },
                Summary::settled(),
            )
        }
        P::ForEach {
            input,
            variable,
            list,
            body,
        } => {
            // input drains first; the body runs all iterations and its output is dropped, nothing
            // in flight after
            let (input, _) = analyze_boxed(input, ctx);
            let mut inner = ctx.clone();
            inner.bind(&variable, Kind::Unknown);
            let (body, _) = analyze_body(body, &mut inner);
            (
                P::ForEach {
                    input,
                    variable,
                    list,
                    body,
                },
                Summary::settled(),
            )
        }

        // -- streaming writers --
        P::CreateNode {
            input,
            variable,
            labels,
            dynamic_labels,
            properties,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut reads = Reads::default();
            let mut writes = Writes::default();
            if let Some(p) = &properties {
                expr_reads(p, ctx, &mut reads);
            }
            for e in &dynamic_labels {
                expr_reads(e, ctx, &mut reads);
            }
            if dynamic_labels.is_empty() {
                writes.created_nodes.push(labels.clone());
            } else {
                writes.created_any_label = true;
            }
            match &properties {
                Some(Expr::MapLiteral(pairs)) => {
                    for (k, _) in pairs {
                        writes.write_prop(k, PropKind::Node, labels.clone());
                    }
                }
                Some(_) => writes.write_all_props(PropKind::Node),
                None => {}
            }
            if let Some(v) = &variable {
                ctx.bind(v, Kind::Node(labels.clone()));
            }
            let input = write_barrier(&reads, &writes, input, &mut s);
            s.reads.merge(&reads);
            s.writes.merge(&writes);
            (
                P::CreateNode {
                    input,
                    variable,
                    labels,
                    dynamic_labels,
                    properties,
                },
                s,
            )
        }
        P::CreateRel {
            input,
            variable,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut reads = Reads::default();
            let mut writes = Writes::default();
            if let Some(p) = &properties {
                expr_reads(p, ctx, &mut reads);
            }
            match &dynamic_edge_type {
                Some(e) => {
                    expr_reads(e, ctx, &mut reads);
                    writes.created_any_rel_type = true;
                }
                None => {
                    writes.created_rel_types.insert(edge_type.clone());
                }
            }
            match &properties {
                Some(Expr::MapLiteral(pairs)) => {
                    for (k, _) in pairs {
                        writes.write_prop(k, PropKind::Rel, Vec::new());
                    }
                }
                Some(_) => writes.write_all_props(PropKind::Rel),
                None => {}
            }
            if let Some(v) = &variable {
                ctx.bind(v, Kind::Rel(vec![edge_type.clone()]));
            }
            let input = write_barrier(&reads, &writes, input, &mut s);
            s.reads.merge(&reads);
            s.writes.merge(&writes);
            (
                P::CreateRel {
                    input,
                    variable,
                    from,
                    to,
                    edge_type,
                    dynamic_edge_type,
                    properties,
                },
                s,
            )
        }
        P::SetProperty { input, items } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut reads = Reads::default();
            let mut writes = Writes::default();
            for item in &items {
                set_item_effects(item, ctx, &mut reads, &mut writes);
            }
            let input = write_barrier(&reads, &writes, input, &mut s);
            s.reads.merge(&reads);
            s.writes.merge(&writes);
            (P::SetProperty { input, items }, s)
        }
        P::RemoveOp { input, items } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut reads = Reads::default();
            let mut writes = Writes::default();
            for item in &items {
                remove_item_effects(item, ctx, &mut reads, &mut writes);
            }
            let input = write_barrier(&reads, &writes, input, &mut s);
            s.reads.merge(&reads);
            s.writes.merge(&writes);
            (P::RemoveOp { input, items }, s)
        }
        P::Merge {
            input,
            pattern,
            on_create,
            on_match,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut reads = Reads::default();
            let mut writes = Writes::default();
            merge_pattern_effects(&pattern, ctx, &mut reads, &mut writes);
            if let Some(sc) = &on_create {
                set_clause_effects(sc, ctx, &mut reads, &mut writes);
            }
            if let Some(sc) = &on_match {
                set_clause_effects(sc, ctx, &mut reads, &mut writes);
            }
            let input = write_barrier(&reads, &writes, input, &mut s);
            s.reads.merge(&reads);
            s.writes.merge(&writes);
            (
                P::Merge {
                    input,
                    pattern,
                    on_create,
                    on_match,
                },
                s,
            )
        }
        // DELETE drains its input, deletes, then emits: a barrier on both sides
        P::Delete {
            input,
            exprs,
            detach,
        } => {
            let (input, _) = analyze_boxed(input, ctx);
            (
                P::Delete {
                    input,
                    exprs,
                    detach,
                },
                Summary::settled(),
            )
        }

        // -- schema statements: single operators, nothing to interleave --
        P::CreateIndex { .. }
        | P::CreateVectorIndex { .. }
        | P::CreateFulltextIndex { .. }
        | P::CreatePointIndex { .. }
        | P::CreateTextIndex { .. }
        | P::DropIndex { .. }
        | P::AnalyzeGraph { .. }
        | P::CreateConstraint { .. }
        | P::DropConstraint { .. }
        | P::CreateEdgeIndex { .. }
        | P::DropEdgeIndex { .. }
        | P::ShowIndexes
        | P::ShowConstraints
        | P::ShowFunctions
        | P::ShowProcedures
        | P::ShowSettings => (plan, Summary::settled()),
        // reads no graph data and writes none
        P::TransactionCommand {
            input,
            command,
            ids,
        } => {
            let (input, mut s) = analyze_boxed(input, ctx);
            let mut own = Reads::default();
            if let Some(ids) = &ids {
                expr_reads(ids, ctx, &mut own);
            }
            for column in command.columns() {
                ctx.bind(column, Kind::Unknown);
            }
            s.reads.merge(&own);
            (
                P::TransactionCommand {
                    input,
                    command,
                    ids,
                },
                s,
            )
        }
    }
}

#[allow(clippy::boxed_local)] // the callers' plan fields are boxed (flagged only on 32-bit targets)
fn analyze_boxed(input: Box<LogicalPlan>, ctx: &mut VarKinds) -> (Box<LogicalPlan>, Summary) {
    let (plan, s) = analyze(*input, ctx);
    (Box::new(plan), s)
}

/// Wraps `input` in `Eager` when `needed`, settling the summary.
fn barrier_if(needed: bool, input: Box<LogicalPlan>, s: &mut Summary) -> Box<LogicalPlan> {
    if needed {
        *s = Summary::settled();
        eager(input)
    } else {
        input
    }
}

/// A write needs a barrier below it when its effects could reach a read still running below (`MATCH
/// (n:P) CREATE (:P)`), or its own reads could see an earlier clause's in-flight writes (`CREATE
/// (:P {id: 1}) MERGE (:P {id: 1})`).
fn write_barrier(
    reads: &Reads,
    writes: &Writes,
    input: Box<LogicalPlan>,
    s: &mut Summary,
) -> Box<LogicalPlan> {
    let needed = conflicts(&s.reads, writes) || conflicts(reads, &s.writes);
    barrier_if(needed, input, s)
}

fn expand_reads(rel_var: &str, to: &str, edge_types: &[String], ctx: &mut VarKinds) -> Reads {
    let mut own = Reads::default();
    own.read_rel_types(edge_types);
    ctx.bind(rel_var, Kind::Rel(edge_types.to_vec()));
    ctx.bind_if_new(to, Kind::Node(Vec::new()));
    own
}

pub(super) fn bind_projection(items: &[ProjectionItem], ctx: &mut VarKinds) {
    for item in items {
        let kind = expr_kind(&item.expr, ctx);
        let name = match &item.alias {
            Some(a) => a.clone(),
            None => match &item.expr {
                Expr::Variable(v) => v.clone(),
                _ => continue,
            },
        };
        ctx.bind(&name, kind);
    }
}

/// `UNWIND` of a parameter, literal list, range or a plain value's property yields plain values;
/// anything else may yield entities.
fn unwind_kind(expr: &Expr, ctx: &VarKinds) -> Kind {
    match expr {
        Expr::Parameter(_) => Kind::Value,
        Expr::ListLiteral(items) => {
            if items
                .iter()
                .all(|e| matches!(expr_kind(e, ctx), Kind::Value))
            {
                Kind::Value
            } else {
                Kind::Unknown
            }
        }
        Expr::Property(base, _) => match expr_kind(base, ctx) {
            Kind::Value => Kind::Value,
            _ => Kind::Unknown,
        },
        Expr::FunctionCall { name, .. }
            if matches!(name.as_str(), "range" | "split" | "keys" | "labels") =>
        {
            Kind::Value
        }
        Expr::Variable(v) => match ctx.kind(v) {
            Kind::Value => Kind::Value,
            _ => Kind::Unknown,
        },
        _ => Kind::Unknown,
    }
}

/// `WHERE n:Label` refines what's known about `n`.
fn refine_labels_from_predicate(pred: &Expr, ctx: &mut VarKinds) {
    match pred {
        Expr::FunctionCall { name, args, .. } if name == "__has_label" => {
            if let (Some(Expr::Variable(v)), Some(Expr::Literal(crate::ast::Literal::String(l)))) =
                (args.first(), args.get(1))
            {
                ctx.add_label(v, l);
            }
        }
        Expr::BinaryOp(a, crate::ast::BinOp::And, b) => {
            refine_labels_from_predicate(a, ctx);
            refine_labels_from_predicate(b, ctx);
        }
        _ => {}
    }
}

/// Procedures that mutate the graph; everything else reads.
fn procedure_writes(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("apoc.create.")
        || n.starts_with("apoc.merge.")
        || n.starts_with("apoc.refactor.")
        || n.starts_with("apoc.nodes.delete")
        || n.starts_with("apoc.periodic.")
        || n.starts_with("apoc.atomic.")
        || n.starts_with("apoc.trigger.")
        || n.starts_with("apoc.import.")
        || n.starts_with("apoc.load.")
        || n.starts_with("apoc.cypher.do")
        || n.starts_with("apoc.cypher.run")
}

/// Every read in a subtree, settled or not, for plans that run per outer row (OPTIONAL MATCH).
#[recursive::recursive]
fn all_reads(plan: &LogicalPlan, ctx: &mut VarKinds, out: &mut Reads) {
    // reuse the bottom-up analysis on a clone for its reads, dropping any barriers (the subtree is
    // read-only)
    let mut inner = ctx.clone();
    let (_, s) = analyze(plan.clone(), &mut inner);
    out.merge(&s.reads);
    // `analyze` settles reads below materializing operators, but per-row plans count them all:
    // gather the settled ones too
    for child in crate::stack::children(plan) {
        all_reads(child, ctx, out);
    }
}
