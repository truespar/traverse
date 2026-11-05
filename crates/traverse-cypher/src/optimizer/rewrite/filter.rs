//! Filters: split conjunctions, push them down, fuse them back, and OR into union seeks.

use std::collections::HashSet;

use crate::ast::{BinOp, CmpOp, Expr, Literal, UnaryOp};
use crate::planner::{IndexHints, LogicalPlan, collect_expr_variables};

use super::expand::{replace_expand_input, reverse_expand};
use super::join::{extract_eq_property_check_any, split_and_chain};
use super::walk::{expr_variables, plan_output_variables, recurse_transform};

// -- Pass: split conjunctions -------------------------------------------

/// `Filter(input, A AND B)` -> `Filter(Filter(input, A), B)`.
pub fn split_conjunctions(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &split_conjunction_inner)
}

fn split_conjunction_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter { input, predicate } = plan {
        let mut conjuncts = Vec::new();
        collect_conjuncts(predicate, &mut conjuncts);

        if conjuncts.len() > 1 {
            let mut result = *input;
            for c in conjuncts {
                result = LogicalPlan::Filter {
                    input: Box::new(result),
                    predicate: c,
                };
            }
            return result;
        }

        return LogicalPlan::Filter {
            input,
            predicate: conjuncts.into_iter().next().unwrap(),
        };
    }
    plan
}

/// Nested ANDs -> list of conjuncts.
#[recursive::recursive]
fn collect_conjuncts(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            collect_conjuncts(*left, out);
            collect_conjuncts(*right, out);
        }
        other => out.push(other),
    }
}

// -- Pass: push filters down -------------------------------------------

/// Pushes `Filter`s below `CrossProduct` and `Expand*` when one child covers all predicate
/// variables.
pub fn push_filters_down(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &push_filter_down_inner)
}

#[recursive::recursive]
fn push_filter_down_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter { input, predicate } = plan {
        let pred_vars = expr_variables(&predicate);

        match *input {
            LogicalPlan::CrossProduct { left, right } => {
                let left_vars = plan_output_variables(&left);
                let right_vars = plan_output_variables(&right);

                if pred_vars.is_subset(&left_vars) {
                    let pushed = push_filter_down_inner(LogicalPlan::Filter {
                        input: left,
                        predicate,
                    });
                    return LogicalPlan::CrossProduct {
                        left: Box::new(pushed),
                        right,
                    };
                }
                if pred_vars.is_subset(&right_vars) {
                    let pushed = push_filter_down_inner(LogicalPlan::Filter {
                        input: right,
                        predicate,
                    });
                    return LogicalPlan::CrossProduct {
                        left,
                        right: Box::new(pushed),
                    };
                }

                // can't push: keep
                return LogicalPlan::Filter {
                    input: Box::new(LogicalPlan::CrossProduct { left, right }),
                    predicate,
                };
            }

            LogicalPlan::ExpandOut {
                input: expand_input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => {
                let input_vars = plan_output_variables(&expand_input);
                if pred_vars.is_subset(&input_vars) {
                    // recurse so the filter keeps sinking through nested Expand/CrossProduct in one
                    // pass; else `WHERE id(a)=0` over three `(a)->(b)->(c)->(a)` expands stops one
                    // short of the Scan and NodeById can't fire
                    let pushed = push_filter_down_inner(LogicalPlan::Filter {
                        input: expand_input,
                        predicate,
                    });
                    return LogicalPlan::ExpandOut {
                        input: Box::new(pushed),
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    };
                }
                return LogicalPlan::Filter {
                    input: Box::new(LogicalPlan::ExpandOut {
                        input: expand_input,
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    }),
                    predicate,
                };
            }

            LogicalPlan::ExpandIn {
                input: expand_input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => {
                let input_vars = plan_output_variables(&expand_input);
                if pred_vars.is_subset(&input_vars) {
                    let pushed = push_filter_down_inner(LogicalPlan::Filter {
                        input: expand_input,
                        predicate,
                    });
                    return LogicalPlan::ExpandIn {
                        input: Box::new(pushed),
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    };
                }
                return LogicalPlan::Filter {
                    input: Box::new(LogicalPlan::ExpandIn {
                        input: expand_input,
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    }),
                    predicate,
                };
            }

            LogicalPlan::ExpandBoth {
                input: expand_input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => {
                let input_vars = plan_output_variables(&expand_input);
                if pred_vars.is_subset(&input_vars) {
                    let pushed = push_filter_down_inner(LogicalPlan::Filter {
                        input: expand_input,
                        predicate,
                    });
                    return LogicalPlan::ExpandBoth {
                        input: Box::new(pushed),
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    };
                }
                return LogicalPlan::Filter {
                    input: Box::new(LogicalPlan::ExpandBoth {
                        input: expand_input,
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    }),
                    predicate,
                };
            }

            // filters commute: one that can't sink would otherwise hold every filter above it
            // (the WHERE of `MATCH (a)-[r]->(b), (c)-[s]->(d)` over the check that keeps `r` and
            // `s` apart, which reads both sides), so sink this one past it when it goes further
            LogicalPlan::Filter {
                input: inner_input,
                predicate: inner_predicate,
            } if sinks_below(&pred_vars, &inner_input) => {
                let pushed = push_filter_down_inner(LogicalPlan::Filter {
                    input: inner_input,
                    predicate,
                });
                return LogicalPlan::Filter {
                    input: Box::new(pushed),
                    predicate: inner_predicate,
                };
            }

            other => {
                return LogicalPlan::Filter {
                    input: Box::new(other),
                    predicate,
                };
            }
        }
    }
    plan
}

/// A filter reading `vars` goes below `plan`'s top in `push_filter_down_inner`: into one side of
/// a CrossProduct, below an expand, or past a filter it goes below.
fn sinks_below(vars: &HashSet<String>, plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::CrossProduct { left, right } => {
            vars.is_subset(&plan_output_variables(left))
                || vars.is_subset(&plan_output_variables(right))
        }
        LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. } => vars.is_subset(&plan_output_variables(input)),
        LogicalPlan::Filter { input, .. } => sinks_below(vars, input),
        _ => false,
    }
}

// -- Pass: fuse filters -----------------------------------------------

/// `Filter(Filter(input, p1), p2)` -> `Filter(input, p1 AND p2)`.
pub fn fuse_filters(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &fuse_filter_inner)
}

fn fuse_filter_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter {
        input,
        predicate: outer_pred,
    } = plan
    {
        if let LogicalPlan::Filter {
            input: inner_input,
            predicate: inner_pred,
        } = *input
        {
            return LogicalPlan::Filter {
                input: inner_input,
                predicate: Expr::BinaryOp(Box::new(inner_pred), BinOp::And, Box::new(outer_pred)),
            };
        }
        return LogicalPlan::Filter {
            input,
            predicate: outer_pred,
        };
    }
    plan
}

/// Splits `Filter(... OR ...)` over `Expand over Scan` into a UNION ALL of two
/// INDEX-SEEK-rooted branches when the OR's two disjuncts pin the pattern's two
/// ENDPOINTS to indexed properties:
///
/// ```text
/// MATCH (a:LA)-[...*]->(b) WHERE a.p = K1 OR b.q = K2      (b:LB known
///                                                          from a
///                                                          __has_label
///                                                          conjunct)
/// ```
///
/// becomes
///
/// ```text
///   Filter(pred)                 over Expand  over IndexScan(a.p=K1)
/// union all
///   Filter(pred AND NOT a.p=K1
///          AND __has_label(a))   over Expand' over IndexScan(b.q=K2)
/// ```
///
/// `Expand'` traverses the same types/range from `b` towards `a`. The branches
/// are DISJOINT (`NOT a.p = K1`), so UNION ALL keeps exact per-path
/// multiplicity, and the full predicate stays on both (pure access-path
/// change). A full-label var-length scan becomes two seeks.
pub fn split_or_filter_into_union_seeks(plan: LogicalPlan, hints: &IndexHints) -> LogicalPlan {
    recurse_transform(plan, &|p| split_or_inner(p, hints))
}

fn split_or_inner(plan: LogicalPlan, hints: &IndexHints) -> LogicalPlan {
    let LogicalPlan::Filter { input, predicate } = plan else {
        return plan;
    };
    // Expand over Scan with an anonymous rel var (a named edge list would show reversed edge order)
    let (expand, scan_var, scan_label) = match &*input {
        LogicalPlan::ExpandOut {
            input: ei,
            from,
            rel_var,
            ..
        }
        | LogicalPlan::ExpandIn {
            input: ei,
            from,
            rel_var,
            ..
        }
        | LogicalPlan::ExpandBoth {
            input: ei,
            from,
            rel_var,
            ..
        } => match &**ei {
            LogicalPlan::Scan {
                variable,
                label: Some(l),
            } if variable == from && rel_var.starts_with("__anon") => {
                (&*input, variable.clone(), l.clone())
            }
            _ => {
                return LogicalPlan::Filter { input, predicate };
            }
        },
        _ => {
            return LogicalPlan::Filter { input, predicate };
        }
    };
    let to_var = match expand {
        LogicalPlan::ExpandOut { to, .. }
        | LogicalPlan::ExpandIn { to, .. }
        | LogicalPlan::ExpandBoth { to, .. } => to.clone(),
        _ => unreachable!(),
    };

    // among the AND conjuncts: (1) the two-armed OR pinning {scan_var, to_var}; (2) a
    // __has_label(to_var, 'LB')
    let mut conjuncts = Vec::new();
    split_and_chain(&predicate, &mut conjuncts);
    let mut or_parts: Option<((String, Expr), (String, Expr), Expr)> = None;
    let mut to_label: Option<String> = None;
    for c in &conjuncts {
        if let Expr::FunctionCall { name, args, .. } = c
            && name == "__has_label"
            && args.len() == 2
            && let (Expr::Variable(v), Expr::Literal(Literal::String(l))) = (&args[0], &args[1])
            && *v == to_var
        {
            to_label = Some(l.clone());
        }
        if or_parts.is_none()
            && let Expr::BinaryOp(l, BinOp::Or, r) = c
        {
            let scan_eq = extract_eq_property_check_any(l, &scan_var)
                .map(|pe| (pe, (**r).clone()))
                .or_else(|| {
                    extract_eq_property_check_any(r, &scan_var).map(|pe| (pe, (**l).clone()))
                });
            if let Some(((sp, sk), other)) = scan_eq
                && let Some((tp, tk)) = extract_eq_property_check_any(&other, &to_var)
            {
                // keys must be variable-free (params/literals)
                let mut kv = HashSet::new();
                collect_expr_variables(&sk, &mut kv);
                collect_expr_variables(&tk, &mut kv);
                if kv.is_empty() {
                    let scan_eq_expr = Expr::Comparison(
                        Box::new(Expr::Property(
                            Box::new(Expr::Variable(scan_var.clone())),
                            sp.clone(),
                        )),
                        vec![(CmpOp::Eq, sk.clone())],
                    );
                    or_parts = Some(((sp, sk), (tp, tk), scan_eq_expr));
                }
            }
        }
    }
    let (Some(((sp, sk), (tp, tk), scan_eq_expr)), Some(lb)) = (or_parts, to_label) else {
        return LogicalPlan::Filter { input, predicate };
    };
    if !hints.contains(&(scan_label.clone(), sp.clone()))
        || !hints.contains(&(lb.clone(), tp.clone()))
    {
        return LogicalPlan::Filter { input, predicate };
    }

    // branch A: seek the scan side
    let seek_a = LogicalPlan::IndexScan {
        variable: scan_var.clone(),
        label: scan_label.clone(),
        property: sp,
        value: sk,
    };
    let expand_a = replace_expand_input(expand.clone(), seek_a);
    let branch_a = LogicalPlan::Filter {
        input: Box::new(expand_a),
        predicate: predicate.clone(),
    };

    // branch B: seek the target side, traverse towards the scan var, exclude branch A's rows,
    // re-check the scan label
    let seek_b = LogicalPlan::IndexScan {
        variable: to_var.clone(),
        label: lb,
        property: tp,
        value: tk,
    };
    let expand_b = reverse_expand(expand.clone(), seek_b);
    let not_a = Expr::UnaryOp(UnaryOp::Not, Box::new(scan_eq_expr));
    let label_a = Expr::FunctionCall {
        name: "__has_label".to_string(),
        distinct: false,
        args: vec![
            Expr::Variable(scan_var),
            Expr::Literal(Literal::String(scan_label)),
        ],
    };
    let pred_b = Expr::BinaryOp(
        Box::new(Expr::BinaryOp(
            Box::new(predicate),
            BinOp::And,
            Box::new(not_a),
        )),
        BinOp::And,
        Box::new(label_a),
    );
    let branch_b = LogicalPlan::Filter {
        input: Box::new(expand_b),
        predicate: pred_b,
    };

    LogicalPlan::Union {
        left: Box::new(branch_a),
        right: Box::new(branch_b),
        all: true,
    }
}
