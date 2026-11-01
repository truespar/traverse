use super::fold::fold_expr;
use super::*;
use crate::ast::*;
use crate::planner::LogicalPlan;

#[test]
fn split_conjunction_splits_ands() {
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".to_string(),
            label: Some("Person".to_string()),
        }),
        predicate: Expr::BinaryOp(
            Box::new(Expr::Comparison(
                Box::new(Expr::Property(
                    Box::new(Expr::Variable("n".to_string())),
                    "a".to_string(),
                )),
                vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(1)))],
            )),
            BinOp::And,
            Box::new(Expr::Comparison(
                Box::new(Expr::Property(
                    Box::new(Expr::Variable("n".to_string())),
                    "b".to_string(),
                )),
                vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(2)))],
            )),
        ),
    };

    let result = split_conjunctions(plan);
    // should give Filter(Filter(Scan, a=1), b=2)
    if let LogicalPlan::Filter { input, .. } = &result {
        assert!(matches!(input.as_ref(), LogicalPlan::Filter { .. }));
    } else {
        panic!("expected nested Filter");
    }
}

#[test]
fn fuse_filters_merges_adjacent() {
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Scan {
                variable: "n".to_string(),
                label: None,
            }),
            predicate: Expr::Literal(Literal::Bool(true)),
        }),
        predicate: Expr::Literal(Literal::Bool(false)),
    };

    let result = fuse_filters(plan);
    if let LogicalPlan::Filter { predicate, .. } = &result {
        assert!(matches!(predicate, Expr::BinaryOp(_, BinOp::And, _)));
    } else {
        panic!("expected Filter with AND");
    }
}

#[test]
fn plan_output_variables_scan() {
    let plan = LogicalPlan::Scan {
        variable: "n".to_string(),
        label: None,
    };
    let vars = plan_output_variables(&plan);
    assert!(vars.contains("n"));
    assert_eq!(vars.len(), 1);
}

#[test]
fn node_by_id_rewrite_collapses_scan_and_id_eq_filter() {
    // Filter(Scan(:User), id(n) = 0)  ->  NodeById{n, User, 0}
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".to_string(),
            label: Some("User".to_string()),
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".to_string(),
                args: vec![Expr::Variable("n".to_string())],
                distinct: false,
            }),
            vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(0)))],
        ),
    };
    let out = convert_filter_to_node_by_id(plan);
    match out {
        LogicalPlan::NodeById {
            variable,
            label,
            id_expr,
        } => {
            assert_eq!(variable, "n");
            assert_eq!(label.as_deref(), Some("User"));
            assert!(matches!(id_expr, Expr::Literal(Literal::Integer(0))));
        }
        other => panic!("expected NodeById, got {:?}", other),
    }
}

#[test]
fn node_by_id_rewrite_handles_commuted_form() {
    // Filter(Scan(:User), 5 = id(n))  ->  NodeById{n, User, 5}
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".to_string(),
            label: Some("User".to_string()),
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::Literal(Literal::Integer(5))),
            vec![(
                CmpOp::Eq,
                Expr::FunctionCall {
                    name: "id".to_string(),
                    args: vec![Expr::Variable("n".to_string())],
                    distinct: false,
                },
            )],
        ),
    };
    let out = convert_filter_to_node_by_id(plan);
    assert!(matches!(out, LogicalPlan::NodeById { .. }));
}

#[test]
fn node_by_id_rewrite_accepts_parameter() {
    // Filter(Scan, id(n) = $nid)  ->  NodeById{n, _, Parameter}
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".to_string(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".to_string(),
                args: vec![Expr::Variable("n".to_string())],
                distinct: false,
            }),
            vec![(CmpOp::Eq, Expr::Parameter("nid".to_string()))],
        ),
    };
    let out = convert_filter_to_node_by_id(plan);
    match out {
        LogicalPlan::NodeById { id_expr, .. } => {
            assert!(matches!(id_expr, Expr::Parameter(_)));
        }
        other => panic!("expected NodeById, got {:?}", other),
    }
}

#[test]
fn node_by_id_rewrite_rejects_non_constant_rhs() {
    // id(n) = m.age uses another variable: no seek
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".to_string(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".to_string(),
                args: vec![Expr::Variable("n".to_string())],
                distinct: false,
            }),
            vec![(
                CmpOp::Eq,
                Expr::Property(Box::new(Expr::Variable("m".to_string())), "age".to_string()),
            )],
        ),
    };
    let out = convert_filter_to_node_by_id(plan);
    // passes through unchanged
    assert!(matches!(out, LogicalPlan::Filter { .. }));
}

#[test]
fn node_by_id_rewrite_rejects_inequality() {
    // id(n) < 100 is a range, not equality: must NOT become NodeById
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".to_string(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".to_string(),
                args: vec![Expr::Variable("n".to_string())],
                distinct: false,
            }),
            vec![(CmpOp::Lt, Expr::Literal(Literal::Integer(100)))],
        ),
    };
    let out = convert_filter_to_node_by_id(plan);
    assert!(matches!(out, LogicalPlan::Filter { .. }));
}

#[test]
fn fold_constants_collapses_literal_eq() {
    // 1 = 0 -> false
    let e = Expr::Comparison(
        Box::new(Expr::Literal(Literal::Integer(1))),
        vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(0)))],
    );
    let f = fold_expr(e);
    assert!(matches!(f, Expr::Literal(Literal::Bool(false))));

    // 2 < 3 -> true
    let e = Expr::Comparison(
        Box::new(Expr::Literal(Literal::Integer(2))),
        vec![(CmpOp::Lt, Expr::Literal(Literal::Integer(3)))],
    );
    assert!(matches!(fold_expr(e), Expr::Literal(Literal::Bool(true))));

    // 'a' = 'b' -> false
    let e = Expr::Comparison(
        Box::new(Expr::Literal(Literal::String("a".into()))),
        vec![(CmpOp::Eq, Expr::Literal(Literal::String("b".into())))],
    );
    assert!(matches!(fold_expr(e), Expr::Literal(Literal::Bool(false))));
}

#[test]
fn fold_constants_short_circuits_boolean_ops() {
    // false AND x -> false (x kept, never evaluated)
    let e = Expr::BinaryOp(
        Box::new(Expr::Literal(Literal::Bool(false))),
        BinOp::And,
        Box::new(Expr::Variable("x".into())),
    );
    assert!(matches!(fold_expr(e), Expr::Literal(Literal::Bool(false))));

    // true OR x -> true
    let e = Expr::BinaryOp(
        Box::new(Expr::Literal(Literal::Bool(true))),
        BinOp::Or,
        Box::new(Expr::Variable("x".into())),
    );
    assert!(matches!(fold_expr(e), Expr::Literal(Literal::Bool(true))));

    // true AND x -> x
    let e = Expr::BinaryOp(
        Box::new(Expr::Literal(Literal::Bool(true))),
        BinOp::And,
        Box::new(Expr::Variable("x".into())),
    );
    assert!(matches!(fold_expr(e), Expr::Variable(s) if s == "x"));

    // NOT true -> false
    let e = Expr::UnaryOp(UnaryOp::Not, Box::new(Expr::Literal(Literal::Bool(true))));
    assert!(matches!(fold_expr(e), Expr::Literal(Literal::Bool(false))));
}

#[test]
fn fold_constants_leaves_null_comparisons_alone() {
    // null = null must be NULL at runtime, NOT folded to true
    let e = Expr::Comparison(
        Box::new(Expr::Literal(Literal::Null)),
        vec![(CmpOp::Eq, Expr::Literal(Literal::Null))],
    );
    // not a Bool literal: unchanged
    let f = fold_expr(e);
    assert!(!matches!(f, Expr::Literal(Literal::Bool(_))));
}

#[test]
fn fold_constants_drops_always_true_filter() {
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: None,
        }),
        predicate: Expr::Literal(Literal::Bool(true)),
    };
    let out = fold_constants(plan);
    assert!(matches!(out, LogicalPlan::Scan { .. }));
}

#[test]
fn fold_constants_keeps_false_filter_as_marker() {
    // always-false Filter stays so the operator builder can swap in NoRowsOp (schema kept for
    // downstream ops)
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::Literal(Literal::Integer(1))),
            vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(0)))],
        ),
    };
    let out = fold_constants(plan);
    match out {
        LogicalPlan::Filter { predicate, .. } => {
            assert!(matches!(predicate, Expr::Literal(Literal::Bool(false))));
        }
        other => panic!("expected Filter with false predicate, got {other:?}"),
    }
}

#[test]
fn node_by_id_range_rewrite_handles_lt() {
    // id(n) < 100  ->  upper = Some(100), lower = None
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: Some("User".into()),
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".into(),
                args: vec![Expr::Variable("n".into())],
                distinct: false,
            }),
            vec![(CmpOp::Lt, Expr::Literal(Literal::Integer(100)))],
        ),
    };
    match convert_filter_to_node_by_id_range(plan) {
        LogicalPlan::NodeByIdRange {
            variable,
            label,
            lower,
            upper,
        } => {
            assert_eq!(variable, "n");
            assert_eq!(label.as_deref(), Some("User"));
            assert!(lower.is_none());
            assert!(matches!(upper, Some(Expr::Literal(Literal::Integer(100)))));
        }
        other => panic!("expected NodeByIdRange, got {other:?}"),
    }
}

#[test]
fn node_by_id_range_rewrite_handles_gte() {
    // id(n) >= 5  ->  lower = Some(Literal(5)), upper = None
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".into(),
                args: vec![Expr::Variable("n".into())],
                distinct: false,
            }),
            vec![(CmpOp::Gte, Expr::Literal(Literal::Integer(5)))],
        ),
    };
    match convert_filter_to_node_by_id_range(plan) {
        LogicalPlan::NodeByIdRange { lower, upper, .. } => {
            assert!(matches!(lower, Some(Expr::Literal(Literal::Integer(5)))));
            assert!(upper.is_none());
        }
        other => panic!("expected NodeByIdRange, got {other:?}"),
    }
}

#[test]
fn node_by_id_range_rewrite_handles_commuted_lt() {
    // 100 > id(n)  ->  same as id(n) < 100
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::Literal(Literal::Integer(100))),
            vec![(
                CmpOp::Gt,
                Expr::FunctionCall {
                    name: "id".into(),
                    args: vec![Expr::Variable("n".into())],
                    distinct: false,
                },
            )],
        ),
    };
    match convert_filter_to_node_by_id_range(plan) {
        LogicalPlan::NodeByIdRange { lower, upper, .. } => {
            assert!(lower.is_none());
            assert!(matches!(upper, Some(Expr::Literal(Literal::Integer(100)))));
        }
        other => panic!("expected NodeByIdRange, got {other:?}"),
    }
}

#[test]
fn node_by_id_range_rewrite_accepts_parameter() {
    // `id(n) < $bound`: still rewritten, the parameter Expr stored as upper bound and evaluated
    // at execute time, so the plan caches across parameter values
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".into(),
                args: vec![Expr::Variable("n".into())],
                distinct: false,
            }),
            vec![(CmpOp::Lt, Expr::Parameter("__sp0".into()))],
        ),
    };
    match convert_filter_to_node_by_id_range(plan) {
        LogicalPlan::NodeByIdRange { upper, .. } => {
            assert!(matches!(upper, Some(Expr::Parameter(_))));
        }
        other => panic!("expected NodeByIdRange, got {other:?}"),
    }
}

#[test]
fn node_by_id_range_rewrite_rejects_eq() {
    // id(n) = 5 isn't a range: the range rewrite passes it through for the NodeById pass
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            variable: "n".into(),
            label: None,
        }),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".into(),
                args: vec![Expr::Variable("n".into())],
                distinct: false,
            }),
            vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(5)))],
        ),
    };
    let out = convert_filter_to_node_by_id_range(plan);
    assert!(matches!(out, LogicalPlan::Filter { .. }));
}

#[test]
fn push_filter_down_descends_through_multiple_expands() {
    // Filter[id(a)=0] above three nested ExpandOuts above Scan(a) must land right above the
    // Scan in one pass, else NodeById can't fire for triangle / multi-hop patterns with WHERE
    // after the chain
    let scan = LogicalPlan::Scan {
        variable: "a".to_string(),
        label: Some("User".to_string()),
    };
    let mk_expand = |inner: LogicalPlan, from: &str, to: &str| LogicalPlan::ExpandOut {
        input: Box::new(inner),
        from: from.to_string(),
        rel_var: format!("__r_{from}_{to}"),
        to: to.to_string(),
        edge_types: vec!["Friend".to_string()],
        range: None,
        prior_edge_vars: vec![],
    };
    let chain = mk_expand(mk_expand(mk_expand(scan, "a", "b"), "b", "c"), "c", "a2");
    let filtered = LogicalPlan::Filter {
        input: Box::new(chain),
        predicate: Expr::Comparison(
            Box::new(Expr::FunctionCall {
                name: "id".to_string(),
                args: vec![Expr::Variable("a".to_string())],
                distinct: false,
            }),
            vec![(CmpOp::Eq, Expr::Literal(Literal::Integer(0)))],
        ),
    };

    let after_push = push_filters_down(filtered);
    // walk down: ExpandOut -> ExpandOut -> ExpandOut -> Filter -> Scan
    let mut cur = &after_push;
    for _ in 0..3 {
        match cur {
            LogicalPlan::ExpandOut { input, .. } => cur = input.as_ref(),
            other => panic!("expected ExpandOut, got {other:?}"),
        }
    }
    assert!(
        matches!(cur, LogicalPlan::Filter { input, .. }
            if matches!(input.as_ref(), LogicalPlan::Scan { .. })),
        "expected Filter(Scan) at the bottom, got {cur:?}",
    );
}

#[test]
fn plan_output_variables_cross_product() {
    let plan = LogicalPlan::CrossProduct {
        left: Box::new(LogicalPlan::Scan {
            variable: "a".to_string(),
            label: None,
        }),
        right: Box::new(LogicalPlan::Scan {
            variable: "b".to_string(),
            label: None,
        }),
    };
    let vars = plan_output_variables(&plan);
    assert!(vars.contains("a"));
    assert!(vars.contains("b"));
}
