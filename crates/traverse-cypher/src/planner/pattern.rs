//! MATCH: the patterns of a clause, seeded from bound variables, with WHERE placed and SEARCH
//! applied.

use crate::ast::{
    CmpOp, Expr, Literal, Pattern, PatternChain, PatternElement, PatternPart, RelDirection,
    ShortestPathMode,
};
use crate::error::CypherError;

use super::IndexHints;
use super::part::{
    make_dynamic_has_label_filter, make_has_label_filter, plan_single_pattern_part, rand_id,
    split_property_filter,
};
use super::plan::LogicalPlan;
use super::vars::collect_expr_variables;

/// Plans a match pattern (public for subquery evaluation).
pub fn plan_match_pattern_pub(pattern: &Pattern) -> Result<LogicalPlan, CypherError> {
    let empty_vars = std::collections::HashSet::new();
    plan_match_pattern(pattern, LogicalPlan::EmptyRow, &empty_vars)
}

/// Re-roots each pattern part at a bound node when possible. Otherwise
/// `(prev)-[r]->(seller)` with `seller` bound from a prior clause scans every
/// node for `prev`, per outer row under OPTIONAL MATCH or across the whole graph
/// in multi-MATCH queries.
///
/// Picks the leftmost bound node as root and emits up to two parts: the left
/// walk with directions flipped, and the untouched right walk. Both reference
/// the root by its original name, so planning sees it bound, skips the scan and
/// uses `edges_between` / O(1) lookups for other bound endpoints.
pub(super) fn rewrite_pattern_for_seed_binding(
    pattern: &Pattern,
    bound_vars: &std::collections::HashSet<String>,
) -> Pattern {
    // track vars each rewritten part introduces so later parts can re-root on them too
    let mut bv = bound_vars.clone();
    let mut new_parts: Vec<PatternPart> = Vec::with_capacity(pattern.parts.len());
    for part in &pattern.parts {
        let rewritten = rewrite_part_root(part, &bv);
        for p in &rewritten {
            collect_part_variables_into(p, &mut bv);
        }
        new_parts.extend(rewritten);
    }
    Pattern { parts: new_parts }
}

/// Pattern part(s) with the best bound root first; two parts (left + right walk) when the root is
/// in the middle.
fn rewrite_part_root(
    part: &PatternPart,
    bound_vars: &std::collections::HashSet<String>,
) -> Vec<PatternPart> {
    // leave shortest-path and named-path patterns alone: reversal changes the element order the
    // result shows
    if part.shortest_path != ShortestPathMode::None
        || part.variable.is_some()
        || part.element.chain.is_empty()
    {
        return vec![part.clone()];
    }

    // leftmost bound node across positions [0..=k], k = chain.len()
    let mut root_idx: Option<usize> = None;
    if part
        .element
        .node
        .variable
        .as_ref()
        .is_some_and(|v| bound_vars.contains(v))
    {
        root_idx = Some(0);
    } else {
        for (i, ch) in part.element.chain.iter().enumerate() {
            if ch
                .node
                .variable
                .as_ref()
                .is_some_and(|v| bound_vars.contains(v))
            {
                root_idx = Some(i + 1);
                break;
            }
        }
    }
    let root_idx = match root_idx {
        None => return vec![part.clone()],
        Some(0) => return vec![part.clone()], // already optimal
        Some(i) => i,
    };

    let chain = &part.element.chain;
    let k = chain.len();
    debug_assert!(root_idx <= k);

    // root needs a variable: both parts reference it
    let root_node = chain[root_idx - 1].node.clone();
    if root_node.variable.is_none() {
        return vec![part.clone()];
    }

    let mut parts: Vec<PatternPart> = Vec::with_capacity(2);

    // left walk: root -> root-1 -> ... -> 0, each direction flipped
    let mut left_chain = Vec::with_capacity(root_idx);
    for i in (0..root_idx).rev() {
        let mut rel = chain[i].rel.clone();
        rel.direction = match rel.direction {
            RelDirection::Outgoing => RelDirection::Incoming,
            RelDirection::Incoming => RelDirection::Outgoing,
            RelDirection::Both => RelDirection::Both,
        };
        let target_node = if i == 0 {
            part.element.node.clone()
        } else {
            chain[i - 1].node.clone()
        };
        left_chain.push(PatternChain {
            rel,
            node: target_node,
        });
    }
    parts.push(PatternPart {
        variable: None,
        element: PatternElement {
            node: root_node.clone(),
            chain: left_chain,
        },
        shortest_path: ShortestPathMode::None,
        weight_property: None,
        total_cost_var: None,
        path_mode: crate::ast::PathMode::Trail,
        qpp_list_groups: Vec::new(),
        path_selector_limit: None,
    });

    // right walk: root -> root+1 -> ... -> k, chain entries unchanged; the left part already applies the
    // root's labels/properties, so strip them here
    if root_idx < k {
        let right_chain: Vec<PatternChain> = chain[root_idx..].to_vec();
        let mut right_root = root_node.clone();
        right_root.labels = Vec::new();
        right_root.properties = None;
        parts.push(PatternPart {
            variable: None,
            element: PatternElement {
                node: right_root,
                chain: right_chain,
            },
            shortest_path: ShortestPathMode::None,
            weight_property: None,
            total_cost_var: None,
            path_mode: crate::ast::PathMode::Trail,
            qpp_list_groups: Vec::new(),
            path_selector_limit: None,
        });
    }
    parts
}

fn collect_part_variables_into(part: &PatternPart, vars: &mut std::collections::HashSet<String>) {
    if let Some(v) = &part.element.node.variable {
        vars.insert(v.clone());
    }
    for ch in &part.element.chain {
        if let Some(v) = &ch.rel.variable {
            vars.insert(v.clone());
        }
        if let Some(v) = &ch.node.variable {
            vars.insert(v.clone());
        }
    }
    if let Some(v) = &part.variable {
        vars.insert(v.clone());
    }
}

pub(super) fn plan_match_pattern(
    pattern: &Pattern,
    plan: LogicalPlan,
    bound_vars: &std::collections::HashSet<String>,
) -> Result<LogicalPlan, CypherError> {
    plan_match_pattern_with_hints(pattern, plan, bound_vars, &IndexHints::new())
}

pub(super) fn plan_match_pattern_with_hints(
    pattern: &Pattern,
    plan: LogicalPlan,
    bound_vars: &std::collections::HashSet<String>,
    hints: &IndexHints,
) -> Result<LogicalPlan, CypherError> {
    // start from the existing plan (unless EmptyRow)
    let mut result: Option<LogicalPlan> = match plan {
        LogicalPlan::EmptyRow => None,
        other => Some(other),
    };

    // variables bound by earlier parts, so later parts join on shared vars
    let mut all_bound = bound_vars.clone();

    for part in &pattern.parts {
        // shortestPath / allShortestPaths
        if part.shortest_path != ShortestPathMode::None {
            let elem = &part.element;
            let chain = &elem.chain[0]; // parser ensures exactly 1 chain
            let rel = &chain.rel;
            let range = rel.range.as_ref().unwrap(); // parser ensures a range

            let from_var = elem
                .node
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_{}", rand_id()));
            let to_var = chain
                .node
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_{}", rand_id()));

            let min_depth = range.0.unwrap_or(1) as usize;
            let max_depth = range.1.unwrap_or(u32::MAX) as usize;

            // scans for unbound endpoints
            let from_bound = all_bound.contains(&from_var);
            let to_bound = all_bound.contains(&to_var);

            // both endpoints unconstrained (unbound, no label, no inline properties) means an
            // all-pairs N^2 sweep the pipeline would materialize: an OOM, not a query. Neo4j rejects
            // this shape too
            if !from_bound
                && !to_bound
                && elem.node.labels.is_empty()
                && elem.node.properties.is_none()
                && chain.node.labels.is_empty()
                && chain.node.properties.is_none()
            {
                return Err(CypherError::semantic(
                    "shortest-path pattern with both endpoints fully unbound is not \
                     supported: bind or label at least one endpoint, or add inline \
                     property predicates"
                        .to_string(),
                ));
            }

            let mut input = result.take().unwrap_or(LogicalPlan::EmptyRow);

            if !from_bound {
                let from_label = elem.node.labels.first().cloned();
                let from_scan = LogicalPlan::Scan {
                    variable: from_var.clone(),
                    label: from_label,
                };
                input = if matches!(input, LogicalPlan::EmptyRow) {
                    from_scan
                } else {
                    LogicalPlan::CrossProduct {
                        left: Box::new(input),
                        right: Box::new(from_scan),
                    }
                };
                // inline property filters on the from-node
                if let Some(ref props) = elem.node.properties {
                    let (local, _external) = split_property_filter(&from_var, props);
                    if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                        input = LogicalPlan::Filter {
                            input: Box::new(input),
                            predicate: local,
                        };
                    }
                }
            }

            if !to_bound {
                let to_label = chain.node.labels.first().cloned();
                let to_scan = LogicalPlan::Scan {
                    variable: to_var.clone(),
                    label: to_label,
                };
                input = if matches!(input, LogicalPlan::EmptyRow) {
                    to_scan
                } else {
                    LogicalPlan::CrossProduct {
                        left: Box::new(input),
                        right: Box::new(to_scan),
                    }
                };
                // inline property filters on the to-node
                if let Some(ref props) = chain.node.properties {
                    let (local, _external) = split_property_filter(&to_var, props);
                    if !matches!(local, Expr::Literal(Literal::Bool(true))) {
                        input = LogicalPlan::Filter {
                            input: Box::new(input),
                            predicate: local,
                        };
                    }
                }
            }

            let sp_plan = if part.shortest_path == ShortestPathMode::WeightedSingle {
                LogicalPlan::WeightedShortestPath {
                    input: Box::new(input),
                    from: from_var.clone(),
                    to: to_var.clone(),
                    edge_types: rel.types.clone(),
                    direction: rel.direction.clone(),
                    weight_property: part.weight_property.clone().unwrap_or_default(),
                    path_var: part.variable.clone(),
                    rel_var: rel.variable.clone(),
                    cost_var: part.total_cost_var.clone(),
                }
            } else {
                let (k, k_groups) = match part.shortest_path {
                    ShortestPathMode::Counted(n) => (Some(n.max(1)), false),
                    ShortestPathMode::CountedGroups(n) => (Some(n.max(1)), true),
                    _ => (None, false),
                };
                LogicalPlan::ShortestPath {
                    input: Box::new(input),
                    from: from_var.clone(),
                    to: to_var.clone(),
                    edge_types: rel.types.clone(),
                    direction: rel.direction.clone(),
                    min_depth,
                    max_depth,
                    path_var: part.variable.clone(),
                    rel_var: rel.variable.clone(),
                    all: part.shortest_path == ShortestPathMode::All,
                    k,
                    k_groups,
                }
            };

            all_bound.insert(from_var);
            all_bound.insert(to_var);
            if let Some(ref v) = rel.variable {
                all_bound.insert(v.clone());
            }
            if let Some(ref v) = part.variable {
                all_bound.insert(v.clone());
            }
            if let Some(ref v) = part.total_cost_var {
                all_bound.insert(v.clone());
            }

            result = Some(sp_plan);
            continue;
        }

        let (part_plan, renames, chain_directly, deferred) =
            plan_single_pattern_part(part, &all_bound, hints)?;

        // variables this part introduces, for later parts
        let elem = &part.element;
        if let Some(ref v) = elem.node.variable {
            all_bound.insert(v.clone());
        }
        for chain in &elem.chain {
            if let Some(ref v) = chain.rel.variable {
                all_bound.insert(v.clone());
            }
            if let Some(ref v) = chain.node.variable {
                all_bound.insert(v.clone());
            }
        }

        result = Some(if chain_directly {
            // bound first node or pre-bound var-length path: chain directly on the existing plan
            // (it replaces the EmptyRow placeholder)
            let existing = result.unwrap_or(LogicalPlan::EmptyRow);
            let mut plan = replace_empty_row(part_plan, existing);
            for pred in &deferred {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: pred.clone(),
                };
            }
            plan
        } else {
            match result {
                None => {
                    // first part: apply deferred predicates directly
                    let mut plan = part_plan;
                    for pred in &deferred {
                        plan = LogicalPlan::Filter {
                            input: Box::new(plan),
                            predicate: pred.clone(),
                        };
                    }
                    plan
                }
                Some(existing) => {
                    let mut combined = LogicalPlan::CrossProduct {
                        left: Box::new(existing),
                        right: Box::new(part_plan),
                    };
                    // renamed bound variables: equality filter so the fresh scan must match the
                    // bound entity
                    for (recheck_var, original_var) in &renames {
                        combined = LogicalPlan::Filter {
                            input: Box::new(combined),
                            predicate: Expr::Comparison(
                                Box::new(Expr::FunctionCall {
                                    name: "id".to_string(),
                                    args: vec![Expr::Variable(recheck_var.clone())],
                                    distinct: false,
                                }),
                                vec![(
                                    CmpOp::Eq,
                                    Expr::FunctionCall {
                                        name: "id".to_string(),
                                        args: vec![Expr::Variable(original_var.clone())],
                                        distinct: false,
                                    },
                                )],
                            ),
                        };
                    }
                    // deferred predicates (outer variable refs) go AFTER the CrossProduct, where
                    // outer variables exist
                    for pred in &deferred {
                        combined = LogicalPlan::Filter {
                            input: Box::new(combined),
                            predicate: pred.clone(),
                        };
                    }
                    combined
                }
            }
        });
    }

    Ok(result.unwrap_or(LogicalPlan::EmptyRow))
}

/// Replaces `EmptyRow` in a plan tree.
#[recursive::recursive]
fn replace_empty_row(plan: LogicalPlan, replacement: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::EmptyRow => replacement,
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(replace_empty_row(*input, replacement)),
            predicate,
        },
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandOut {
            input: Box::new(replace_empty_row(*input, replacement)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandIn {
            input: Box::new(replace_empty_row(*input, replacement)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandBoth {
            input: Box::new(replace_empty_row(*input, replacement)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } => LogicalPlan::BindPath {
            input: Box::new(replace_empty_row(*input, replacement)),
            variable,
            node_vars,
            edge_vars,
        },
        LogicalPlan::BindQppGroups {
            input,
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        } => LogicalPlan::BindQppGroups {
            input: Box::new(replace_empty_row(*input, replacement)),
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        },
        LogicalPlan::BindVarLists { input, groups } => LogicalPlan::BindVarLists {
            input: Box::new(replace_empty_row(*input, replacement)),
            groups,
        },
        LogicalPlan::BindPathEndpoints {
            input,
            rel_var,
            from_var,
            to_var,
            direction,
        } => LogicalPlan::BindPathEndpoints {
            input: Box::new(replace_empty_row(*input, replacement)),
            rel_var,
            from_var,
            to_var,
            direction,
        },
        LogicalPlan::BindEdgeEndpoints {
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        } => LogicalPlan::BindEdgeEndpoints {
            input: Box::new(replace_empty_row(*input, replacement)),
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        },
        LogicalPlan::ShortestPath {
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
        } => LogicalPlan::ShortestPath {
            input: Box::new(replace_empty_row(*input, replacement)),
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
        LogicalPlan::WeightedShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path_var,
            rel_var,
            cost_var,
        } => LogicalPlan::WeightedShortestPath {
            input: Box::new(replace_empty_row(*input, replacement)),
            from,
            to,
            edge_types,
            direction,
            weight_property,
            path_var,
            rel_var,
            cost_var,
        },
        other => other,
    }
}

/// Plans `MATCH pattern SEARCH var IN (VECTOR INDEX ...)`: a
/// [`LogicalPlan::VectorSearchScan`] over `input` with the pattern's label and
/// property predicates as post-filters (Neo4j: they filter the k results, not
/// what the index returns). Like Neo4j, the pattern must be a single node whose
/// variable is the SEARCH binding.
pub(super) fn plan_match_search(
    mc: &crate::ast::MatchClause,
    search: &crate::ast::SearchClause,
    input: LogicalPlan,
) -> Result<LogicalPlan, CypherError> {
    if mc.pattern.parts.len() != 1 || !mc.pattern.parts[0].element.chain.is_empty() {
        return Err(CypherError::semantic(
            "SEARCH requires a single-node MATCH pattern (no relationships or extra pattern parts)"
                .to_string(),
        ));
    }
    let node = &mc.pattern.parts[0].element.node;
    match node.variable.as_deref() {
        Some(v) if v == search.binding => {}
        Some(other) => {
            return Err(CypherError::semantic(format!(
                "SEARCH binding variable `{}` must be the variable bound by the MATCH pattern \
                 (found `{other}`)",
                search.binding
            )));
        }
        None => {
            return Err(CypherError::semantic(format!(
                "SEARCH binding variable `{}` is not defined by the MATCH pattern",
                search.binding
            ))
            .with_status(crate::status::GqlStatus::VariableNotDefined));
        }
    }
    // the query vector may not depend on the binding variable (42I75)
    let mut qv_vars = std::collections::HashSet::new();
    collect_expr_variables(&search.query_vector, &mut qv_vars);
    if qv_vars.contains(&search.binding) {
        return Err(CypherError::semantic(format!(
            "the query vector expression in the SEARCH clause may not depend on the search \
             clause binding variable `{}`",
            search.binding
        )));
    }

    let mut plan = LogicalPlan::VectorSearchScan {
        input: Box::new(input),
        variable: search.binding.clone(),
        index_name: search.index_name.clone(),
        query_vector: search.query_vector.clone(),
        index_where: search.index_where.clone(),
        limit: search.limit.clone(),
        score_var: search.score_alias.clone(),
    };

    // label / property predicates post-filter the k results
    for label in &node.labels {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: make_has_label_filter(&search.binding, label),
        };
    }
    for dl in &node.dynamic_labels {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: make_dynamic_has_label_filter(&search.binding, dl.clone()),
        };
    }
    if let Some(ref props) = node.properties
        && let Expr::MapLiteral(entries) = props
    {
        for (key, value) in entries {
            let cmp = Expr::Comparison(
                Box::new(Expr::Property(
                    Box::new(Expr::Variable(search.binding.clone())),
                    key.clone(),
                )),
                vec![(CmpOp::Eq, value.clone())],
            );
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: cmp,
            };
        }
    }
    Ok(plan)
}
