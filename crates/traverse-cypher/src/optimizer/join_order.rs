//! Join order by dynamic programming (System R): flattens `CrossProduct` chains and runs bottom-up
//! DP over subsets for the cheapest order; greedy above 8 relations.

use std::collections::{HashMap, HashSet};

use traverse_core::tx::view::GraphView;

use super::cost::estimate_cost;
use super::rewrite::plan_output_variables;
use crate::ast::*;
use crate::planner::{LogicalPlan, collect_expr_variables};

/// Max relations for full DP.
const DP_LIMIT: usize = 8;

/// Picks the cheapest ordering of each CrossProduct chain.
pub fn optimize_join_order(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    // bottom-up; recurse_join_order already runs try_reorder_cross_products at each CrossProduct,
    // so not again here (it would re-flatten the DP output and rerun DP)
    recurse_join_order(plan, graph)
}

#[recursive::recursive]
fn recurse_join_order(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    match plan {
        // CrossProduct: DON'T recurse into children; try_reorder_cross_products flattens the whole
        // chain, runs DP once, and recurses into non-CP base relations afterwards
        LogicalPlan::CrossProduct { left, right } => {
            try_reorder_cross_products(LogicalPlan::CrossProduct { left, right }, graph)
        }
        LogicalPlan::Filter { input, predicate } => {
            // filter directly above a CrossProduct chain: try_reorder_cross_products takes it (it
            // collects filters)
            if has_cross_product_at_top(&input) {
                try_reorder_cross_products(LogicalPlan::Filter { input, predicate }, graph)
            } else {
                LogicalPlan::Filter {
                    input: Box::new(recurse_join_order(*input, graph)),
                    predicate,
                }
            }
        }
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandOut {
            input: Box::new(recurse_join_order(*input, graph)),
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
            input: Box::new(recurse_join_order(*input, graph)),
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
            input: Box::new(recurse_join_order(*input, graph)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } => LogicalPlan::Project {
            input: Box::new(recurse_join_order(*input, graph)),
            items,
            distinct,
            where_clause,
        },
        LogicalPlan::Sort { input, items } => LogicalPlan::Sort {
            input: Box::new(recurse_join_order(*input, graph)),
            items,
        },
        LogicalPlan::Limit { input, count } => LogicalPlan::Limit {
            input: Box::new(recurse_join_order(*input, graph)),
            count,
        },
        LogicalPlan::Skip { input, count } => LogicalPlan::Skip {
            input: Box::new(recurse_join_order(*input, graph)),
            count,
        },
        LogicalPlan::Eager { input } => LogicalPlan::Eager {
            input: Box::new(recurse_join_order(*input, graph)),
        },
        LogicalPlan::Aggregate { input, keys, aggs } => LogicalPlan::Aggregate {
            input: Box::new(recurse_join_order(*input, graph)),
            keys,
            aggs,
        },
        LogicalPlan::Union { left, right, all } => LogicalPlan::Union {
            left: Box::new(recurse_join_order(*left, graph)),
            right: Box::new(recurse_join_order(*right, graph)),
            all,
        },
        LogicalPlan::OptionalMatch {
            input,
            match_plan,
            new_variables,
        } => LogicalPlan::OptionalMatch {
            input: Box::new(recurse_join_order(*input, graph)),
            match_plan: Box::new(recurse_join_order(*match_plan, graph)),
            new_variables,
        },
        LogicalPlan::DistinctExpand {
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
        } => LogicalPlan::DistinctExpand {
            input: Box::new(recurse_join_order(*input, graph)),
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
        LogicalPlan::HashJoin {
            left,
            right,
            left_keys,
            right_keys,
        } => LogicalPlan::HashJoin {
            left: Box::new(recurse_join_order(*left, graph)),
            right: Box::new(recurse_join_order(*right, graph)),
            left_keys,
            right_keys,
        },
        // everything else passes through
        other => other,
    }
}

/// CrossProduct, or a Filter-over-CrossProduct chain?
#[recursive::recursive]
fn has_cross_product_at_top(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::CrossProduct { .. } => true,
        LogicalPlan::Filter { input, .. } => has_cross_product_at_top(input),
        _ => false,
    }
}

fn try_reorder_cross_products(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    // flatten the CrossProduct chain + collect the filters above it
    let mut relations = Vec::new();
    let mut filters = Vec::new();
    if !flatten_cross_products(&plan, &mut relations, &mut filters) {
        return plan;
    }

    if relations.len() < 2 {
        return plan;
    }

    // optimize non-CP base relations first (e.g. CPs nested in OptionalMatch), then DP this level
    let relations: Vec<LogicalPlan> = relations
        .into_iter()
        .map(|r| recurse_join_order(r, graph))
        .collect();

    if relations.len() > DP_LIMIT || filters.len() > 64 {
        return greedy_join_order(relations, filters, graph);
    }

    dp_join_order(relations, filters, graph)
}

/// Flattens nested CrossProduct and Filter nodes into base relations and predicates; true if the
/// plan was a CrossProduct chain (possibly with Filters on top).
#[recursive::recursive]
fn flatten_cross_products(
    plan: &LogicalPlan,
    relations: &mut Vec<LogicalPlan>,
    filters: &mut Vec<Expr>,
) -> bool {
    match plan {
        LogicalPlan::CrossProduct { left, right } => {
            flatten_cross_products(left, relations, filters);
            flatten_cross_products(right, relations, filters);
            true
        }
        LogicalPlan::Filter { input, predicate } => {
            let prev_rel = relations.len();
            let prev_filt = filters.len();
            if flatten_cross_products(input, relations, filters) {
                filters.push(predicate.clone());
                true
            } else {
                relations.truncate(prev_rel);
                filters.truncate(prev_filt);
                relations.push(plan.clone());
                false
            }
        }
        _ => {
            relations.push(plan.clone());
            false
        }
    }
}

/// DP over subsets (System R). Each entry carries a bitmask of filters already applied, so a filter
/// isn't repeated at every superset level.
fn dp_join_order(
    relations: Vec<LogicalPlan>,
    filters: Vec<Expr>,
    graph: &dyn GraphView,
) -> LogicalPlan {
    let n = relations.len();
    let total_mask: u64 = (1 << n) - 1;

    // variables per relation
    let rel_vars: Vec<HashSet<String>> = relations.iter().map(plan_output_variables).collect();

    // variables per filter
    let filter_vars: Vec<HashSet<String>> = filters
        .iter()
        .map(|f| {
            let mut vars = HashSet::new();
            collect_expr_variables(f, &mut vars);
            vars
        })
        .collect();

    // subset_mask -> (plan, cost, applied_filter_bitmask)
    let mut dp: HashMap<u64, (LogicalPlan, f64, u64)> = HashMap::new();

    // base case: single relations
    for i in 0..n {
        let mask = 1u64 << i;
        let plan = relations[i].clone();

        // applicable filters (none applied yet)
        let (plan, applied) = apply_filters(plan, mask, &rel_vars, &filters, &filter_vars, 0);
        let (_, cost) = estimate_cost(&plan, graph);
        dp.insert(mask, (plan, cost, applied));
    }

    // subsets by increasing size
    for size in 2..=n {
        for mask in 1..=total_mask {
            if (mask as u32).count_ones() as usize != size {
                continue;
            }

            let mut best: Option<(LogicalPlan, f64, u64)> = None;

            // every non-empty proper subset
            let mut sub = (mask - 1) & mask;
            while sub > 0 {
                let complement = mask & !sub;
                if complement > 0 && sub < complement {
                    // only sub < complement, no duplicates
                    if let (
                        Some((left_plan, _, left_applied)),
                        Some((right_plan, _, right_applied)),
                    ) = (dp.get(&sub), dp.get(&complement))
                    {
                        let joined = LogicalPlan::CrossProduct {
                            left: Box::new(left_plan.clone()),
                            right: Box::new(right_plan.clone()),
                        };

                        // only filters the children haven't applied
                        let parent_applied = left_applied | right_applied;
                        let (joined, applied) = apply_filters(
                            joined,
                            mask,
                            &rel_vars,
                            &filters,
                            &filter_vars,
                            parent_applied,
                        );

                        let (_, cost) = estimate_cost(&joined, graph);

                        if best
                            .as_ref()
                            .is_none_or(|(_, best_cost, _)| cost < *best_cost)
                        {
                            best = Some((joined, cost, applied));
                        }
                    }
                }
                sub = (sub - 1) & mask;
            }

            if let Some(entry) = best {
                match dp.get(&mask) {
                    Some((_, existing_cost, _)) if entry.1 >= *existing_cost => {}
                    _ => {
                        dp.insert(mask, entry);
                    }
                }
            }
        }
    }

    // best plan for the full set
    dp.remove(&total_mask)
        .map(|(plan, _, _)| plan)
        .unwrap_or_else(|| {
            // fallback: plain chain
            build_chain(relations, filters)
        })
}

/// Greedy join order beyond DP_LIMIT relations.
fn greedy_join_order(
    relations: Vec<LogicalPlan>,
    filters: Vec<Expr>,
    graph: &dyn GraphView,
) -> LogicalPlan {
    let n = relations.len();
    let rel_vars: Vec<HashSet<String>> = relations.iter().map(plan_output_variables).collect();
    let filter_vars: Vec<HashSet<String>> = filters
        .iter()
        .map(|f| {
            let mut vars = HashSet::new();
            collect_expr_variables(f, &mut vars);
            vars
        })
        .collect();

    let mut used = vec![false; n];
    let mut applied_filters = vec![false; filters.len()];

    // start with the cheapest single relation
    let mut best_idx = 0;
    let mut best_cost = f64::INFINITY;
    for (i, rel) in relations.iter().enumerate() {
        let (_, cost) = estimate_cost(rel, graph);
        if cost < best_cost {
            best_cost = cost;
            best_idx = i;
        }
    }
    used[best_idx] = true;
    let mut current = relations[best_idx].clone();
    let mut current_vars: HashSet<String> = rel_vars[best_idx].clone();

    // applicable filters
    for (fi, f) in filters.iter().enumerate() {
        if !applied_filters[fi] && filter_vars[fi].is_subset(&current_vars) {
            current = LogicalPlan::Filter {
                input: Box::new(current),
                predicate: f.clone(),
            };
            applied_filters[fi] = true;
        }
    }

    // greedily add the cheapest next join
    for _ in 1..n {
        let mut next_best: Option<(usize, LogicalPlan, f64)> = None;

        for (i, _rel) in relations.iter().enumerate() {
            if used[i] {
                continue;
            }
            let joined = LogicalPlan::CrossProduct {
                left: Box::new(current.clone()),
                right: Box::new(relations[i].clone()),
            };
            let (_, cost) = estimate_cost(&joined, graph);
            if next_best.as_ref().is_none_or(|(_, _, bc)| cost < *bc) {
                next_best = Some((i, joined, cost));
            }
        }

        if let Some((idx, joined, _)) = next_best {
            used[idx] = true;
            current = joined;
            for v in &rel_vars[idx] {
                current_vars.insert(v.clone());
            }

            // newly applicable filters
            for (fi, f) in filters.iter().enumerate() {
                if !applied_filters[fi] && filter_vars[fi].is_subset(&current_vars) {
                    current = LogicalPlan::Filter {
                        input: Box::new(current),
                        predicate: f.clone(),
                    };
                    applied_filters[fi] = true;
                }
            }
        }
    }

    // any remaining filters
    for (fi, f) in filters.iter().enumerate() {
        if !applied_filters[fi] {
            current = LogicalPlan::Filter {
                input: Box::new(current),
                predicate: f.clone(),
            };
        }
    }

    current
}

/// Applies filters fully covered by `mask`, skipping those children already applied (bitmask);
/// returns the plan and the new bitmask.
fn apply_filters(
    mut plan: LogicalPlan,
    mask: u64,
    rel_vars: &[HashSet<String>],
    filters: &[Expr],
    filter_vars: &[HashSet<String>],
    already_applied: u64,
) -> (LogicalPlan, u64) {
    // variables covered by this mask
    let mut covered = HashSet::new();
    for (i, vars) in rel_vars.iter().enumerate() {
        if mask & (1u64 << i) != 0 {
            for v in vars {
                covered.insert(v.clone());
            }
        }
    }

    let mut applied = already_applied;
    for (fi, f) in filters.iter().enumerate() {
        // already applied by a child, or past the bitmask range
        if fi >= 64 || (applied & (1u64 << fi)) != 0 {
            continue;
        }
        if filter_vars[fi].is_subset(&covered) {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: f.clone(),
            };
            applied |= 1u64 << fi;
        }
    }

    (plan, applied)
}

/// Plain chain join (fallback).
fn build_chain(relations: Vec<LogicalPlan>, filters: Vec<Expr>) -> LogicalPlan {
    let mut plan = relations
        .into_iter()
        .reduce(|left, right| LogicalPlan::CrossProduct {
            left: Box::new(left),
            right: Box::new(right),
        })
        .unwrap_or(LogicalPlan::EmptyRow);

    for f in filters {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: f,
        };
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_relation_unchanged() {
        let plan = LogicalPlan::Scan {
            variable: "n".to_string(),
            label: None,
        };
        let mut g = traverse_core::graph::Graph::new();
        g.create_node(&[], std::collections::BTreeMap::new());
        let result = optimize_join_order(plan.clone(), &g);
        // unchanged: no CrossProduct
        assert!(matches!(result, LogicalPlan::Scan { .. }));
    }
}
