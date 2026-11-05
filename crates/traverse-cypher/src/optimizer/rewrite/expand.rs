//! Expand direction: start from the cheaper end. `Filter(target-only pred) -> Expand -> Scan`
//! becomes `Expand_reversed -> Filter(source_label) -> Filter(target pred) -> Scan(target)` when the
//! target filter is selective enough to scan from that side:
//!
//! ```text
//! MATCH (n:User)-[:Friend]->(m:User) WHERE m.age > 50 RETURN n.id, m.id
//!
//! before: Project -> Filter(m.age>50) -> Filter(has_label(m,User)) -> ExpandOut(n->m) -> Scan(n:User)
//! after:  Project -> Filter(has_label(n,User)) -> ExpandIn(m->n) -> Filter(m.age>50) -> Scan(m:User)
//! ```

use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::planner::LogicalPlan;

use super::fuse::{extract_has_label_filter, split_and_conjuncts};
use super::walk::{expr_variables, recurse_transform};

/// Reverses expand direction when a selective target filter makes scanning from the target cheaper.
pub fn reverse_expand_direction(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    recurse_transform(plan, &|p| reverse_expand_direction_inner(p, graph))
}

fn reverse_expand_direction_inner(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    // match Filter*(Expand+(Filter*(Scan))): peel the filters above the chain, then check whether
    // target-only ones are selective enough

    // step 1: peel filters above the chain, splitting ANDs (fuse_filters may have merged them)
    let mut filters: Vec<Expr> = Vec::new();
    let mut current = &plan;
    while let LogicalPlan::Filter { input, predicate } = current {
        split_and_conjuncts(predicate, &mut filters);
        current = input;
    }

    if filters.is_empty() {
        return plan;
    }

    // step 2: walk down the Expand chain collecting hops: same direction,
    // fixed-length, no ExpandBoth. The chain must end in a plain Scan (step 3), so a
    // hop's prior_edge_vars name hops below it; the reversed chain keeps the same
    // pairs apart (step 6), which is not every pair when the chain spans MATCH
    // clauses or a WALK part.
    struct ExpandHop {
        from: String,
        rel_var: String,
        to: String,
        edge_types: Vec<String>,
        direction: Direction,
        prior_edge_vars: Vec<String>,
    }

    let mut expands: Vec<ExpandHop> = Vec::new();
    let mut chain_bottom = current;

    loop {
        let (input, from, rel_var, to, edge_types, range, prior_edge_vars, direction) =
            match chain_bottom {
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
                _ => break, // not an expand (or ExpandBoth): stop
            };

        // guard: variable length
        if range.is_some() {
            break;
        }

        // guard: same direction
        if !expands.is_empty() && expands[0].direction != direction {
            break;
        }

        // guard: connectivity, previous hop's `from` = this hop's `to`
        if !expands.is_empty() && expands.last().unwrap().from != *to {
            break;
        }

        expands.push(ExpandHop {
            from: from.clone(),
            rel_var: rel_var.clone(),
            to: to.clone(),
            edge_types: edge_types.clone(),
            direction,
            prior_edge_vars: prior_edge_vars.clone(),
        });
        chain_bottom = input;
    }

    if expands.is_empty() {
        return plan;
    }
    // a relationship kept off from outside the chain can't be kept off from the other end
    if expands.iter().any(|h| {
        h.prior_edge_vars
            .iter()
            .any(|p| !expands.iter().any(|e| &e.rel_var == p))
    }) {
        return plan;
    }

    // step 3: the bottom must be Filter*(Scan)
    let mut scan_filters: Vec<Expr> = Vec::new();
    let mut scan_node = chain_bottom;
    while let LogicalPlan::Filter { input, predicate } = scan_node {
        split_and_conjuncts(predicate, &mut scan_filters);
        scan_node = input;
    }

    let (scan_var, scan_label) = match scan_node {
        LogicalPlan::Scan { variable, label } => (variable.clone(), label.clone()),
        _ => return plan, // not a simple scan: bail
    };

    // scan_var must be the innermost expand's from
    if scan_var != expands.last().unwrap().from {
        return plan;
    }

    // step 4: split filters into target-only (only the outermost `to`) vs the rest
    let to_var = &expands[0].to; // outermost target
    let from_var = &expands.last().unwrap().from; // innermost source = scan_var
    let mut target_filters: Vec<Expr> = Vec::new();
    let mut source_label_filter: Option<String> = None;
    let mut other_filters: Vec<Expr> = Vec::new();
    let mut target_label: Option<String> = None;

    for pred in &filters {
        let vars = expr_variables(pred);

        // has_label filter on the target?
        if let Some(lbl) = extract_has_label_filter(pred, to_var) {
            target_label = Some(lbl);
            continue;
        }

        // has_label filter on the source?
        if let Some(lbl) = extract_has_label_filter(pred, from_var) {
            source_label_filter = Some(lbl);
            continue;
        }

        // target-only: uses only `to` (plus literals/params)
        if !vars.is_empty() && vars.iter().all(|v| v == to_var) {
            target_filters.push(pred.clone());
        } else {
            other_filters.push(pred.clone());
        }
    }

    // guard: no target-only predicate and no target label, nothing to gain
    if target_filters.is_empty() && target_label.is_none() {
        return plan;
    }

    // step 5: cost check, reversed_scan_card * target_selectivity < 0.5 * original_scan_card
    let original_scan_card = crate::optimizer::cardinality::estimate_cardinality(
        &LogicalPlan::Scan {
            variable: scan_var.clone(),
            label: scan_label.clone(),
        },
        graph,
    );

    // target scan label from `__has_label(to, L)`, if any
    let target_scan_label = target_label.as_ref().or(None);
    let reversed_scan_card = crate::optimizer::cardinality::estimate_cardinality(
        &LogicalPlan::Scan {
            variable: to_var.clone(),
            label: target_scan_label.cloned(),
        },
        graph,
    );

    let mut target_selectivity: f64 = 1.0;
    for pred in &target_filters {
        target_selectivity *= crate::optimizer::cardinality::estimate_selectivity(pred, graph);
    }

    let reversed_cost = reversed_scan_card * target_selectivity;
    let original_cost = original_scan_card;

    if reversed_cost >= 0.5 * original_cost {
        return plan; // not selective enough: bail
    }

    // step 6: Scan(to_var, target_label) -> target filters -> reversed chain -> source label -> scan
    // filters -> other filters

    let mut new_plan: LogicalPlan = LogicalPlan::Scan {
        variable: to_var.clone(),
        label: target_scan_label.cloned(),
    };

    // target-only filters below the chain
    for pred in target_filters {
        new_plan = LogicalPlan::Filter {
            input: Box::new(new_plan),
            predicate: pred,
        };
    }

    // reversed chain, outermost expand first (closest to the new scan): swap direction and from/to;
    // each hop keeps off the preceding reversed hops it was kept apart from before
    let apart = |a: &ExpandHop, b: &str| {
        a.prior_edge_vars.iter().any(|p| p == b)
            || expands
                .iter()
                .any(|e| e.rel_var == b && e.prior_edge_vars.contains(&a.rel_var))
    };
    let mut preceding: Vec<String> = Vec::new();
    for expand in &expands {
        let reversed_prior: Vec<String> = preceding
            .iter()
            .filter(|p| apart(expand, p))
            .cloned()
            .collect();
        let reversed_direction = match expand.direction {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => unreachable!(),
        };
        new_plan = match reversed_direction {
            Direction::Outgoing => LogicalPlan::ExpandOut {
                input: Box::new(new_plan),
                from: expand.to.clone(),
                rel_var: expand.rel_var.clone(),
                to: expand.from.clone(),
                edge_types: expand.edge_types.clone(),
                range: None,
                prior_edge_vars: reversed_prior,
            },
            Direction::Incoming => LogicalPlan::ExpandIn {
                input: Box::new(new_plan),
                from: expand.to.clone(),
                rel_var: expand.rel_var.clone(),
                to: expand.from.clone(),
                edge_types: expand.edge_types.clone(),
                range: None,
                prior_edge_vars: reversed_prior,
            },
            Direction::Both => unreachable!(),
        };
        preceding.push(expand.rel_var.clone());
    }

    // source label filter (the original scan label becomes a post-expand filter)
    let source_label = source_label_filter.or(scan_label);
    if let Some(ref lbl) = source_label {
        new_plan = LogicalPlan::Filter {
            input: Box::new(new_plan),
            predicate: crate::planner::make_has_label_filter(from_var, lbl),
        };
    }

    // filters that sat between scan and expand
    for pred in scan_filters {
        new_plan = LogicalPlan::Filter {
            input: Box::new(new_plan),
            predicate: pred,
        };
    }

    // non-target filters on top
    for pred in other_filters {
        new_plan = LogicalPlan::Filter {
            input: Box::new(new_plan),
            predicate: pred,
        };
    }

    new_plan
}

pub(super) fn replace_expand_input(expand: LogicalPlan, new_input: LogicalPlan) -> LogicalPlan {
    match expand {
        LogicalPlan::ExpandOut {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => LogicalPlan::ExpandOut {
            input: Box::new(new_input),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandIn {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => LogicalPlan::ExpandIn {
            input: Box::new(new_input),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandBoth {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => LogicalPlan::ExpandBoth {
            input: Box::new(new_input),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        other => other,
    }
}

/// Same traversal from the other anchor: `(a)-OUT->(b)` from `b` is IN.
pub(super) fn reverse_expand(expand: LogicalPlan, new_input: LogicalPlan) -> LogicalPlan {
    match expand {
        LogicalPlan::ExpandOut {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => LogicalPlan::ExpandIn {
            input: Box::new(new_input),
            from: to,
            rel_var,
            to: from,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandIn {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => LogicalPlan::ExpandOut {
            input: Box::new(new_input),
            from: to,
            rel_var,
            to: from,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandBoth {
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            ..
        } => LogicalPlan::ExpandBoth {
            input: Box::new(new_input),
            from: to,
            rel_var,
            to: from,
            edge_types,
            range,
            prior_edge_vars,
        },
        other => other,
    }
}
