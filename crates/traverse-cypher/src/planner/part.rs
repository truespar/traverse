//! One pattern part: scans, expands, variable-length and quantified paths, shortest paths, and
//! the label / type / property filters on them.

use crate::ast::{
    BinOp, CmpOp, Expr, Literal, NodePattern, PathMode, PatternElement, PatternPart,
    QuantifierKind, RelDirection,
};
use crate::error::CypherError;

use super::IndexHints;
use super::plan::LogicalPlan;
use super::vars::{collect_expr_variables, is_indexable_value};

/// One planned pattern part.
pub(super) struct PlannedPart {
    pub plan: LogicalPlan,
    /// Recheck variable -> original bound variable, for equality filters once the part joins the
    /// plan.
    pub renames: Vec<(String, String)>,
    /// Chain on top of the existing plan (no CrossProduct): the part starts from a bound node or
    /// a bound relationship's endpoints.
    pub chain_directly: bool,
    /// Filters on variables outside this part, applied once it joins the plan.
    pub deferred: Vec<Expr>,
    /// The relationships the part binds, in pattern order (none in a WALK part, which may reuse
    /// them).
    pub rels: Vec<PartRel>,
}

/// A relationship a pattern part binds.
pub(super) struct PartRel {
    /// Its variable in the plan (a `__recheck_` alias for one an earlier clause bound).
    pub var: String,
    /// A relationship list (variable-length or quantified), not a single relationship.
    pub list: bool,
    /// Taken from its endpoints' binding, not expanded: no expand kept it off the relationships
    /// before it.
    pub bound: bool,
}

/// Plans one pattern part (one node + chain of rels). `outer_rels` are the relationships the
/// clause's earlier parts bound: a part that chains onto them keeps every expand off them, so
/// one MATCH never binds a relationship twice.
pub(super) fn plan_single_pattern_part(
    part: &PatternPart,
    bound_vars: &std::collections::HashSet<String>,
    hints: &IndexHints,
    outer_rels: &[String],
) -> Result<PlannedPart, CypherError> {
    // WALK parts (`REPEATABLE ELEMENTS`) may reuse relationships
    let unique = part.path_mode != PathMode::Walk;
    let mut renames: Vec<(String, String)> = Vec::new();
    let elem = &part.element;
    let node = &elem.node;

    // first chain is a pre-bound var-length path: no Scan, endpoints come from the bound list
    if let Some(first_chain) = elem.chain.first() {
        let rel = &first_chain.rel;
        let is_bound_varlen = rel.range.is_some()
            && rel
                .variable
                .as_ref()
                .is_some_and(|v| bound_vars.contains(v));
        if is_bound_varlen && elem.chain.len() == 1 {
            let rv = rel.variable.as_ref().unwrap().clone();
            let from_var = node
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_{}", rand_id()));
            let to_var = first_chain
                .node
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_{}", rand_id()));
            let plan = LogicalPlan::BindPathEndpoints {
                input: Box::new(LogicalPlan::EmptyRow),
                rel_var: rv.clone(),
                from_var,
                to_var,
                direction: rel.direction.clone(),
            };
            let plan = wrap_qpp_list_groups(plan, part);
            return Ok(PlannedPart {
                plan,
                renames,
                chain_directly: true,
                deferred: collect_pattern_where_preds(&part.element),
                rels: unique
                    .then_some(PartRel {
                        var: rv,
                        list: true,
                        bound: true,
                    })
                    .into_iter()
                    .collect(),
            });
        }

        // first chain is a pre-bound fixed-length edge: endpoints from the edge, no scan (else
        // `OPTIONAL MATCH (hub)-[r:T]-(other)` with `r` bound scans all nodes per outer row)
        let is_bound_fixed_edge = rel.range.is_none()
            && rel
                .variable
                .as_ref()
                .is_some_and(|v| bound_vars.contains(v));
        if is_bound_fixed_edge && elem.chain.len() == 1 {
            let edge_var = rel.variable.as_ref().unwrap().clone();
            let from_name = node
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_{}", rand_id()));
            let to_name = first_chain
                .node
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_{}", rand_id()));
            let mut plan: LogicalPlan = LogicalPlan::BindEdgeEndpoints {
                input: Box::new(LogicalPlan::EmptyRow),
                edge_var: edge_var.clone(),
                from_var: from_name.clone(),
                to_var: to_name.clone(),
                direction: rel.direction.clone(),
                edge_types: rel.types.clone(),
            };
            // label filters on both endpoints; dynamic labels (`:$param` / `:$(expr)`) go through
            // `eval_has_label` per frame, same filter shape
            for lbl in &node.labels {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: make_has_label_filter(&from_name, lbl),
                };
            }
            for dyn_lbl in &node.dynamic_labels {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: make_dynamic_has_label_filter(&from_name, dyn_lbl.clone()),
                };
            }
            for lbl in &first_chain.node.labels {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: make_has_label_filter(&to_name, lbl),
                };
            }
            for dyn_lbl in &first_chain.node.dynamic_labels {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: make_dynamic_has_label_filter(&to_name, dyn_lbl.clone()),
                };
            }
            // inline property filters on endpoints (local conjuncts)
            let mut deferred_after: Vec<Expr> = Vec::new();
            if let Some(ref props) = node.properties {
                let (local, external) = split_property_filter(&from_name, props);
                if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: local,
                    };
                }
                if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                    deferred_after.push(external);
                }
            }
            if let Some(ref props) = first_chain.node.properties {
                let (local, external) = split_property_filter(&to_name, props);
                if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: local,
                    };
                }
                if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                    deferred_after.push(external);
                }
            }
            // edge inline property filters
            if let Some(ref props) = rel.properties {
                let (local, external) =
                    split_property_filter(rel.variable.as_ref().unwrap(), props);
                if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: local,
                    };
                }
                if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                    deferred_after.push(external);
                }
            }
            let plan = wrap_qpp_list_groups(plan, part);
            deferred_after.extend(collect_pattern_where_preds(&part.element));
            return Ok(PlannedPart {
                plan,
                renames,
                chain_directly: true,
                deferred: deferred_after,
                rels: unique
                    .then_some(PartRel {
                        var: edge_var,
                        list: false,
                        bound: true,
                    })
                    .into_iter()
                    .collect(),
            });
        }
    }

    // first node bound: skip the scan and expand from the binding (no CrossProduct + label scan +
    // equality filter); a lone bound node only checks the binding is a node
    let first_node_bound = node
        .variable
        .as_ref()
        .is_some_and(|v| bound_vars.contains(v));

    let mut deferred_predicates: Vec<Expr> = Vec::new();

    let (var, mut plan) = if first_node_bound {
        let var = node.variable.as_ref().unwrap().clone();
        let mut plan = LogicalPlan::EmptyRow;
        if elem.chain.is_empty() {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: Expr::IsTyped {
                    expr: Box::new(Expr::Variable(var.clone())),
                    type_name: "NODE".to_string(),
                    negated: false,
                    not_null: true,
                },
            };
        }
        // check labels on the bound node
        for lbl in &node.labels {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: make_has_label_filter(&var, lbl),
            };
        }
        for dyn_lbl in &node.dynamic_labels {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: make_dynamic_has_label_filter(&var, dyn_lbl.clone()),
            };
        }
        // check inline properties
        if let Some(ref props) = node.properties {
            let (local, external) = split_property_filter(&var, props);
            if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: local,
                };
            }
            if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                deferred_predicates.push(external);
            }
        }
        (var, plan)
    } else {
        // scan the first node (unbound: a bound one takes the branch above)
        let var = node
            .variable
            .clone()
            .unwrap_or_else(|| format!("__anon_{}", rand_id()));
        let label = node.labels.first().cloned();

        // IndexScan when the node has a label and inline properties matching an index hint
        let plan = if let (Some(lbl), Some(Expr::MapLiteral(entries))) = (&label, &node.properties)
        {
            // first property with an index and a literal/parameter value
            let idx_entry = entries.iter().find(|(key, val)| {
                hints.contains(&(lbl.clone(), key.clone())) && is_indexable_value(val)
            });
            if let Some((idx_key, idx_val)) = idx_entry {
                let mut p = LogicalPlan::IndexScan {
                    variable: var.clone(),
                    label: lbl.clone(),
                    property: idx_key.clone(),
                    value: idx_val.clone(),
                };
                // filter for the non-indexed properties
                let remaining: Vec<Expr> = entries
                    .iter()
                    .filter(|(k, _)| k != idx_key)
                    .map(|(k, v)| {
                        Expr::Comparison(
                            Box::new(Expr::Property(
                                Box::new(Expr::Variable(var.clone())),
                                k.clone(),
                            )),
                            vec![(CmpOp::Eq, v.clone())],
                        )
                    })
                    .collect();
                // split into local and external
                let local_vars: std::collections::HashSet<String> =
                    [var.clone()].into_iter().collect();
                for cmp in remaining {
                    let mut refs = std::collections::HashSet::new();
                    collect_expr_variables(&cmp, &mut refs);
                    if refs.is_subset(&local_vars) {
                        p = LogicalPlan::Filter {
                            input: Box::new(p),
                            predicate: cmp,
                        };
                    } else {
                        deferred_predicates.push(cmp);
                    }
                }
                p
            } else {
                // no index hit: Scan + filter
                let mut p = LogicalPlan::Scan {
                    variable: var.clone(),
                    label: label.clone(),
                };
                let (local, external) =
                    split_property_filter(&var, node.properties.as_ref().unwrap());
                if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                    p = LogicalPlan::Filter {
                        input: Box::new(p),
                        predicate: local,
                    };
                }
                if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                    deferred_predicates.push(external);
                }
                p
            }
        } else {
            let mut p = LogicalPlan::Scan {
                variable: var.clone(),
                label,
            };
            if let Some(ref props) = node.properties {
                let (local, external) = split_property_filter(&var, props);
                if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                    p = LogicalPlan::Filter {
                        input: Box::new(p),
                        predicate: local,
                    };
                }
                if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                    deferred_predicates.push(external);
                }
            }
            p
        };

        // extra labels
        let mut plan = plan;
        if node.labels.len() > 1 {
            for label in &node.labels[1..] {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: make_has_label_filter(&var, label),
                };
            }
        }
        for dyn_lbl in &node.dynamic_labels {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: make_dynamic_has_label_filter(&var, dyn_lbl.clone()),
            };
        }

        (var, plan)
    };

    // node/edge vars for named paths
    let mut node_vars = vec![var.clone()];
    let mut edge_vars = Vec::new();
    let mut seen_node_vars = std::collections::HashSet::new();
    seen_node_vars.insert(var.clone());
    // original name too, for cycle detection
    if let Some(ref v) = node.variable {
        seen_node_vars.insert(v.clone());
    }
    // chaining from a bound node: every outer bound var is in scope (replace_empty_row), so mark
    // them seen and target lookups use edges_between() instead of a neighbor scan
    if first_node_bound {
        for v in bound_vars {
            seen_node_vars.insert(v.clone());
        }
    }

    let mut current_from = var;
    // what each expand keeps off: the part's earlier relationships and, chaining onto the
    // plan, the clause's earlier parts' (a cross-joined part gets filters instead, see
    // `plan_match_pattern_with_hints`)
    let mut prior_edge_vars_acc: Vec<String> = if unique && first_node_bound {
        outer_rels.to_vec()
    } else {
        Vec::new()
    };
    let mut rels: Vec<PartRel> = Vec::new();
    for chain in &elem.chain {
        let rel = &chain.rel;
        let target = &chain.node;

        // pre-bound var-length relationship list: endpoints from the list, no Scan+Expand
        let is_bound_varlen = rel.range.is_some()
            && rel
                .variable
                .as_ref()
                .is_some_and(|v| bound_vars.contains(v));
        if is_bound_varlen {
            let rv = rel.variable.as_ref().unwrap().clone();
            let from_var = current_from.clone();
            let to_var = if let Some(ref v) = target.variable {
                v.clone()
            } else {
                format!("__anon_{}", rand_id())
            };
            plan = LogicalPlan::BindPathEndpoints {
                input: Box::new(plan),
                rel_var: rv.clone(),
                from_var,
                to_var: to_var.clone(),
                direction: rel.direction.clone(),
            };
            if unique {
                prior_edge_vars_acc.push(rv.clone());
                rels.push(PartRel {
                    var: rv.clone(),
                    list: true,
                    bound: true,
                });
            }
            edge_vars.push(rv);
            if let Some(ref v) = target.variable {
                seen_node_vars.insert(v.clone());
            }
            node_vars.push(to_var.clone());
            current_from = to_var;
            continue;
        }

        // rename a relationship variable bound in a prior clause (not var-length ones: their value
        // is a list)
        let rel_var = if let Some(ref v) = rel.variable {
            if bound_vars.contains(v) && rel.range.is_none() {
                let renamed = format!("__recheck_{v}_{}", rand_id());
                renames.push((renamed.clone(), v.clone()));
                renamed
            } else {
                v.clone()
            }
        } else {
            format!("__anon_rel_{}", rand_id())
        };

        // target already bound in this part (cycle)?
        let is_reused = target
            .variable
            .as_ref()
            .is_some_and(|v| seen_node_vars.contains(v));
        let to_var = if is_reused {
            // reuse the original variable: the slot is bound, so exec_expand uses edges_between()
            // instead of a neighbor scan
            target.variable.as_ref().unwrap().clone()
        } else if let Some(ref v) = target.variable {
            if bound_vars.contains(v) {
                let renamed = format!("__recheck_{v}_{}", rand_id());
                renames.push((renamed.clone(), v.clone()));
                renamed
            } else {
                v.clone()
            }
        } else {
            format!("__anon_{}", rand_id())
        };
        let target_label = target.labels.first().cloned();

        // dynamic types: no static push-down (edge_types: []), post-filter on the union of static +
        // dynamic types; Neo4j's `:STATIC|$($dyn)` OR semantics, correct at the cost of type
        // push-down
        let has_dyn_types = !rel.dynamic_types.is_empty();
        let expand_edge_types = if has_dyn_types {
            Vec::new()
        } else {
            rel.types.clone()
        };
        match rel.direction {
            RelDirection::Outgoing => {
                plan = LogicalPlan::ExpandOut {
                    input: Box::new(plan),
                    from: current_from.clone(),
                    rel_var: rel_var.clone(),
                    to: to_var.clone(),
                    edge_types: expand_edge_types,
                    range: rel.range,
                    prior_edge_vars: prior_edge_vars_acc.clone(),
                };
            }
            RelDirection::Incoming => {
                plan = LogicalPlan::ExpandIn {
                    input: Box::new(plan),
                    from: current_from.clone(),
                    rel_var: rel_var.clone(),
                    to: to_var.clone(),
                    edge_types: expand_edge_types,
                    range: rel.range,
                    prior_edge_vars: prior_edge_vars_acc.clone(),
                };
            }
            RelDirection::Both => {
                plan = LogicalPlan::ExpandBoth {
                    input: Box::new(plan),
                    from: current_from.clone(),
                    rel_var: rel_var.clone(),
                    to: to_var.clone(),
                    edge_types: expand_edge_types,
                    range: rel.range,
                    prior_edge_vars: prior_edge_vars_acc.clone(),
                };
            }
        }
        if has_dyn_types {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: make_dynamic_edge_type_filter(&rel_var, &rel.types, &rel.dynamic_types),
            };
        }

        // quantified group `((x)-[r]->(y)){m,n}`: bind per-iteration node lists (ISO section 16.11 group
        // variables) and enforce inner label/property constraints on every iteration
        if let (Some(qpp), true) = (&rel.qpp_group, rel.range.is_some()) {
            let needs_src = qpp.start.variable.is_some()
                || !qpp.start.labels.is_empty()
                || !qpp.start.dynamic_labels.is_empty()
                || qpp.start.properties.is_some();
            let needs_dst = qpp.end.variable.is_some()
                || !qpp.end.labels.is_empty()
                || !qpp.end.dynamic_labels.is_empty()
                || qpp.end.properties.is_some();
            let src_var = needs_src.then(|| {
                qpp.start
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("__qpp_src_{}", rand_id()))
            });
            let dst_var = needs_dst.then(|| {
                qpp.end
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("__qpp_dst_{}", rand_id()))
            });
            if src_var.is_some() || dst_var.is_some() {
                plan = LogicalPlan::BindQppGroups {
                    input: Box::new(plan),
                    anchor_var: current_from.clone(),
                    rel_var: rel_var.clone(),
                    src_var: src_var.clone(),
                    dst_var: dst_var.clone(),
                };
            }
            // per-iteration constraints as all(n IN list WHERE ...); zero iterations -> empty lists ->
            // all() is true (ISO)
            for (list_var, node_pat) in [(&src_var, &qpp.start), (&dst_var, &qpp.end)] {
                let Some(list_var) = list_var else { continue };
                for pred in make_qpp_iteration_predicates(list_var, node_pat) {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: pred,
                    };
                }
            }
        }

        if unique {
            prior_edge_vars_acc.push(rel_var.clone());
            rels.push(PartRel {
                var: rel_var.clone(),
                list: rel.range.is_some(),
                bound: false,
            });
        }

        // relationship inline properties (e.g. [r:KNOWS {name: 'x'}])
        if let Some(ref props) = rel.properties {
            if rel.range.is_none() {
                // fixed length: direct property filter
                let (local, external) = split_property_filter(&rel_var, props);
                if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: local,
                    };
                }
                if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                    deferred_predicates.push(external);
                }
            } else {
                // variable length: ALL(hop IN rel_var WHERE hop.prop = val)
                if let Expr::MapLiteral(pairs) = props {
                    for (key, val_expr) in pairs {
                        let hop_var = format!("__hop_{}", rand_id());
                        plan = LogicalPlan::Filter {
                            input: Box::new(plan),
                            predicate: Expr::QuantifierExpr {
                                kind: QuantifierKind::All,
                                variable: hop_var.clone(),
                                list_expr: Box::new(Expr::Variable(rel_var.clone())),
                                predicate: Box::new(Expr::Comparison(
                                    Box::new(Expr::Property(
                                        Box::new(Expr::Variable(hop_var)),
                                        key.clone(),
                                    )),
                                    vec![(CmpOp::Eq, val_expr.clone())],
                                )),
                            },
                        };
                    }
                }
            }
        }

        edge_vars.push(rel_var);
        node_vars.push(to_var.clone());

        // target label filter
        if let Some(ref lbl) = target_label {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: make_has_label_filter(&to_var, lbl),
            };
        }

        // target property filter
        if let Some(ref props) = target.properties {
            let (local, external) = split_property_filter(&to_var, props);
            if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: local,
                };
            }
            if !matches!(external, Expr::Literal(Literal::Bool(true))) {
                deferred_predicates.push(external);
            }
        }

        // extra target labels
        if target.labels.len() > 1 {
            for label in &target.labels[1..] {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: make_has_label_filter(&to_var, label),
                };
            }
        }
        for dyn_lbl in &target.dynamic_labels {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: make_dynamic_has_label_filter(&to_var, dyn_lbl.clone()),
            };
        }

        if let Some(ref v) = target.variable {
            seen_node_vars.insert(v.clone());
        }
        current_from = to_var;
    }

    // named path variable
    if let Some(ref path_var) = part.variable {
        plan = LogicalPlan::BindPath {
            input: Box::new(plan),
            variable: path_var.clone(),
            node_vars,
            edge_vars,
        };
    }

    let plan = wrap_qpp_list_groups(plan, part);
    deferred_predicates.extend(collect_pattern_where_preds(&part.element));
    Ok(PlannedPart {
        plan,
        renames,
        chain_directly: first_node_bound,
        deferred: deferred_predicates,
        rels,
    })
}

/// Pattern-internal `WHERE` predicates of a part (`(n WHERE ...)` / `[r WHERE ...]`,
/// Cypher 5+) as filter conjuncts:
///
/// * Fixed-length elements: the predicate as is.
/// * NAMED var-length / quantified relationships apply PER ELEMENT:
///   `all(r IN r WHERE pred)`. The self-shadowing quantifier reads the bound edge
///   LIST, then shadows `r` per element (Neo4j's QPP inline-predicate semantics;
///   also accepted on the legacy `*` form, which Neo4j rejects).
/// * QPP-group inner nodes apply per iteration over their group lists.
/// * Anonymous elements can't reference themselves, so their predicate is a
///   plain conjunct.
///
/// The conjuncts join the part's deferred predicates, applied after the whole
/// part (and any cross-part join), so they may use variables bound later in the
/// pattern; `push_filters_down` then places them.
fn collect_pattern_where_preds(elem: &PatternElement) -> Vec<Expr> {
    let all_over_self = |var: &str, pred: Expr| Expr::QuantifierExpr {
        kind: QuantifierKind::All,
        variable: var.to_string(),
        list_expr: Box::new(Expr::Variable(var.to_string())),
        predicate: Box::new(pred),
    };
    let mut out = Vec::new();
    if let Some(p) = &elem.node.where_pred {
        out.push((**p).clone());
    }
    for chain in &elem.chain {
        if let Some(p) = &chain.rel.where_pred {
            let pred = (**p).clone();
            match (&chain.rel.range, &chain.rel.variable) {
                (Some(_), Some(v)) => out.push(all_over_self(v, pred)),
                _ => out.push(pred),
            }
        }
        if let Some(qpp) = &chain.rel.qpp_group {
            for np in [&qpp.start, &qpp.end] {
                if let Some(p) = &np.where_pred {
                    let pred = (**p).clone();
                    match &np.variable {
                        Some(v) => out.push(all_over_self(v, pred)),
                        None => out.push(pred),
                    }
                }
            }
        }
        if let Some(p) = &chain.node.where_pred {
            out.push((**p).clone());
        }
    }
    out
}

/// Wraps a part's plan with its ISO section 16.11 group-variable list bindings, if the GQL multi-step
/// unrolling recorded any.
fn wrap_qpp_list_groups(plan: LogicalPlan, part: &PatternPart) -> LogicalPlan {
    if part.qpp_list_groups.is_empty() {
        return plan;
    }
    LogicalPlan::BindVarLists {
        input: Box::new(plan),
        groups: part.qpp_list_groups.clone(),
    }
}

/// `all(hop IN list_var WHERE ...)` predicates enforcing a QPP group's inner-node label/property
/// constraints on every iteration.
fn make_qpp_iteration_predicates(list_var: &str, node_pat: &NodePattern) -> Vec<Expr> {
    let mut out = Vec::new();
    let all_over = |predicate: Expr, hop: &str| Expr::QuantifierExpr {
        kind: QuantifierKind::All,
        variable: hop.to_string(),
        list_expr: Box::new(Expr::Variable(list_var.to_string())),
        predicate: Box::new(predicate),
    };
    for lbl in &node_pat.labels {
        let hop = format!("__qpp_hop_{}", rand_id());
        let pred = Expr::FunctionCall {
            name: "__has_label".to_string(),
            distinct: false,
            args: vec![
                Expr::Variable(hop.clone()),
                Expr::Literal(Literal::String(lbl.clone())),
            ],
        };
        out.push(all_over(pred, &hop));
    }
    for dyn_lbl in &node_pat.dynamic_labels {
        let hop = format!("__qpp_hop_{}", rand_id());
        let pred = Expr::FunctionCall {
            name: "__has_label".to_string(),
            distinct: false,
            args: vec![Expr::Variable(hop.clone()), dyn_lbl.clone()],
        };
        out.push(all_over(pred, &hop));
    }
    if let Some(Expr::MapLiteral(pairs)) = &node_pat.properties {
        for (key, val) in pairs {
            let hop = format!("__qpp_hop_{}", rand_id());
            let pred = Expr::Comparison(
                Box::new(Expr::Property(
                    Box::new(Expr::Variable(hop.clone())),
                    key.clone(),
                )),
                vec![(CmpOp::Eq, val.clone())],
            );
            out.push(all_over(pred, &hop));
        }
    }
    out
}

/// `has_label` check as an internal function call.
pub(crate) fn make_has_label_filter(var: &str, label: &str) -> Expr {
    Expr::FunctionCall {
        name: "__has_label".to_string(),
        distinct: false,
        args: vec![
            Expr::Variable(var.to_string()),
            Expr::Literal(Literal::String(label.to_string())),
        ],
    }
}

/// `make_has_label_filter` with the label from an expression evaluated per frame (`MATCH
/// (n:$($label))`, `MATCH (n:$paramname)`); `eval_has_label` handles literal and dynamic labels
/// alike.
pub(crate) fn make_dynamic_has_label_filter(var: &str, label_expr: Expr) -> Expr {
    Expr::FunctionCall {
        name: "__has_label".to_string(),
        distinct: false,
        args: vec![Expr::Variable(var.to_string()), label_expr],
    }
}

/// Post-Expand filter constraining `type(rel_var)` to the listed static names
/// *or* dynamic expressions. Used when `rel.dynamic_types` is non-empty: the
/// Expand gets `edge_types: []` (no push-down) and this filter on top, matching
/// Neo4j's `:STATIC|$($dyn)` union semantics.
pub(crate) fn make_dynamic_edge_type_filter(
    rel_var: &str,
    static_types: &[String],
    dynamic_types: &[Expr],
) -> Expr {
    let type_call = || Expr::FunctionCall {
        name: "type".to_string(),
        distinct: false,
        args: vec![Expr::Variable(rel_var.to_string())],
    };
    let mut alternatives: Vec<Expr> = Vec::with_capacity(static_types.len() + dynamic_types.len());
    for s in static_types {
        alternatives.push(Expr::Comparison(
            Box::new(type_call()),
            vec![(CmpOp::Eq, Expr::Literal(Literal::String(s.clone())))],
        ));
    }
    for de in dynamic_types {
        alternatives.push(Expr::Comparison(
            Box::new(type_call()),
            vec![(CmpOp::Eq, de.clone())],
        ));
    }
    // left-leaning OR chain
    let mut iter = alternatives.into_iter();
    let first = iter.next().expect("at least one alternative required");
    iter.fold(first, |acc, next| {
        Expr::BinaryOp(Box::new(acc), BinOp::Or, Box::new(next))
    })
}

/// Splits inline property conditions into local (only the scanned variable,
/// literals or parameters) and external (other variables from an outer scope
/// such as UNWIND): `(local, external)`, each `Bool(true)` when empty.
pub(super) fn split_property_filter(var: &str, props: &Expr) -> (Expr, Expr) {
    if let Expr::MapLiteral(entries) = props {
        let mut local: Vec<Expr> = Vec::new();
        let mut external: Vec<Expr> = Vec::new();
        let local_vars: std::collections::HashSet<String> = [var.to_string()].into_iter().collect();
        for (key, value) in entries {
            let prop_access =
                Expr::Property(Box::new(Expr::Variable(var.to_string())), key.clone());
            let cmp = Expr::Comparison(Box::new(prop_access), vec![(CmpOp::Eq, value.clone())]);
            // only the scanned variable, literals or parameters -> local; else external
            let mut refs = std::collections::HashSet::new();
            collect_expr_variables(value, &mut refs);
            if refs.is_subset(&local_vars) {
                local.push(cmp);
            } else {
                external.push(cmp);
            }
        }
        let local_pred = local
            .into_iter()
            .reduce(|a, b| Expr::BinaryOp(Box::new(a), BinOp::And, Box::new(b)))
            .unwrap_or(Expr::Literal(Literal::Bool(true)));
        let ext_pred = external
            .into_iter()
            .reduce(|a, b| Expr::BinaryOp(Box::new(a), BinOp::And, Box::new(b)))
            .unwrap_or(Expr::Literal(Literal::Bool(true)));
        (local_pred, ext_pred)
    } else {
        (
            Expr::Literal(Literal::Bool(true)),
            Expr::Literal(Literal::Bool(true)),
        )
    }
}

/// Counter for anonymous variable names.
pub(super) fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
