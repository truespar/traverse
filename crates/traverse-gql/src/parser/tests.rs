use super::*;
use crate::ast::*;

fn parse_ok(input: &str) -> Program {
    parse(input).unwrap_or_else(|errs| {
        panic!("expected parse of {input:?} to succeed, got errors: {errs:#?}")
    })
}

/// Destructures a `PathElement::Step` into `(edge, node)`; panics on a `Group`.
fn step_of(elem: &crate::ast::PathElement) -> (&EdgePattern, &NodePattern) {
    match elem {
        crate::ast::PathElement::Step { edge, node } => (edge, node),
        crate::ast::PathElement::Group { .. } => {
            panic!("expected Step path element, got Group")
        }
    }
}

fn parse_err(input: &str) -> Vec<ParseError> {
    parse(input).expect_err(&format!("expected {input:?} to fail"))
}

fn one_query(p: &Program) -> &QueryStatement {
    match &p.statements[..] {
        [Statement::Query(q)] => q,
        other => panic!("expected one query statement, got: {other:?}"),
    }
}

fn match_clause(q: &QueryStatement) -> &MatchClause {
    for c in &q.head.clauses {
        if let Clause::Match(m) = c {
            return m;
        }
    }
    panic!("no MATCH clause: {:?}", q.head.clauses);
}

fn return_clause(q: &QueryStatement) -> &ReturnClause {
    for c in &q.head.clauses {
        if let Clause::Return(r) = c {
            return r;
        }
    }
    panic!("no RETURN clause: {:?}", q.head.clauses);
}

// -- trivia --------------------------------------------------

#[test]
fn empty_input_parses_to_empty_program() {
    let p = parse_ok("");
    assert!(p.statements.is_empty());
}

#[test]
fn whitespace_only_parses_to_empty_program() {
    assert!(parse_ok("  \n\t ").statements.is_empty());
}

#[test]
fn comment_only_parses_to_empty_program() {
    assert!(parse_ok("-- hi\n/* block */").statements.is_empty());
}

// -- Smallest legal queries ----------------------------------

#[test]
fn match_node_return_variable() {
    let p = parse_ok("MATCH (n) RETURN n");
    let q = one_query(&p);
    let m = match_clause(q);
    assert!(!m.optional);
    assert_eq!(m.patterns.len(), 1);
    assert_eq!(m.patterns[0].start.variable.as_deref(), Some("n"));
    assert!(m.patterns[0].chain.is_empty());

    let r = return_clause(q);
    assert!(!r.distinct);
    assert_eq!(r.items.len(), 1);
    assert!(matches!(&r.items[0].expr, Expression::Variable { name, .. } if name == "n"));
    assert!(r.items[0].alias.is_none());
}

#[test]
fn match_anonymous_node() {
    let p = parse_ok("MATCH () RETURN 1");
    let m = match_clause(one_query(&p));
    assert!(m.patterns[0].start.variable.is_none());
    assert!(m.patterns[0].start.label_expr.is_none());
}

#[test]
fn match_labeled_node() {
    let p = parse_ok("MATCH (n:Person) RETURN n");
    let m = match_clause(one_query(&p));
    match &m.patterns[0].start.label_expr {
        Some(LabelExpression::Label { name, .. }) => assert_eq!(name, "Person"),
        other => panic!("expected single Label, got {other:?}"),
    }
}

#[test]
fn match_optional_keyword() {
    let p = parse_ok("OPTIONAL MATCH (n) RETURN n");
    let m = match_clause(one_query(&p));
    assert!(m.optional);
}

#[test]
fn case_insensitive_keywords() {
    let p = parse_ok("match (n:person) where n.age > 30 return n");
    let m = match_clause(one_query(&p));
    match &m.patterns[0].start.label_expr {
        Some(LabelExpression::Label { name, .. }) => assert_eq!(name, "person"),
        other => panic!("expected single Label, got {other:?}"),
    }
    assert!(m.where_clause.is_some());
}

// -- Path patterns -------------------------------------------

#[test]
fn outgoing_edge() {
    let p = parse_ok("MATCH (a)-[r:KNOWS]->(b) RETURN r");
    let m = match_clause(one_query(&p));
    let path = &m.patterns[0];
    assert_eq!(path.chain.len(), 1);
    let (edge, target) = step_of(&path.chain[0]);
    assert_eq!(edge.variable.as_deref(), Some("r"));
    match &edge.label_expr {
        Some(LabelExpression::Label { name, .. }) => assert_eq!(name, "KNOWS"),
        other => panic!("expected single Label, got {other:?}"),
    }
    assert_eq!(edge.direction, EdgeDirection::Outgoing);
    assert_eq!(target.variable.as_deref(), Some("b"));
}

#[test]
fn incoming_edge() {
    let p = parse_ok("MATCH (a)<-[:LIKES]-(b) RETURN a");
    let m = match_clause(one_query(&p));
    let (edge, _) = step_of(&m.patterns[0].chain[0]);
    assert_eq!(edge.direction, EdgeDirection::Incoming);
    match &edge.label_expr {
        Some(LabelExpression::Label { name, .. }) => assert_eq!(name, "LIKES"),
        other => panic!("expected single Label, got {other:?}"),
    }
}

#[test]
fn undirected_edge() {
    let p = parse_ok("MATCH (a)-[r]-(b) RETURN r");
    let m = match_clause(one_query(&p));
    assert_eq!(
        step_of(&m.patterns[0].chain[0]).0.direction,
        EdgeDirection::Undirected
    );
}

#[test]
fn multi_hop_chain() {
    let p = parse_ok("MATCH (a)-[]->(b)-[]->(c) RETURN a, b, c");
    let m = match_clause(one_query(&p));
    assert_eq!(m.patterns[0].chain.len(), 2);
}

#[test]
fn edge_without_brackets() {
    // GQL allows the bare-arrow form `(a)-->(b)` and `(a)<--(b)`
    // and `(a)--(b)` for anonymous edges.
    let p = parse_ok("MATCH (a)-->(b) RETURN a");
    let (edge, _) = step_of(&match_clause(one_query(&p)).patterns[0].chain[0]);
    assert_eq!(edge.direction, EdgeDirection::Outgoing);
    assert!(edge.variable.is_none());
    assert!(edge.label_expr.is_none());
}

#[test]
fn multiple_path_patterns_comma_separated() {
    let p = parse_ok("MATCH (a), (b)-[r]->(c) RETURN a, c");
    let m = match_clause(one_query(&p));
    assert_eq!(m.patterns.len(), 2);
}

// -- Property maps -------------------------------------------

#[test]
fn node_property_map() {
    let p = parse_ok("MATCH (n {name: 'Alice', age: 30}) RETURN n");
    let m = match_clause(one_query(&p));
    let props = m.patterns[0].start.properties.as_ref().expect("props");
    assert_eq!(props.entries.len(), 2);
    assert_eq!(props.entries[0].0, "name");
    assert!(matches!(&props.entries[0].1,
        Expression::StringLit { value, .. } if value == "Alice"));
}

#[test]
fn edge_property_map() {
    let p = parse_ok("MATCH (a)-[r {since: 2020}]->(b) RETURN r");
    let m = match_clause(one_query(&p));
    let (edge, _) = step_of(&m.patterns[0].chain[0]);
    let props = edge.properties.as_ref().expect("props");
    assert_eq!(props.entries[0].0, "since");
}

#[test]
fn empty_property_map_is_legal() {
    let p = parse_ok("MATCH (n {}) RETURN n");
    let m = match_clause(one_query(&p));
    let props = m.patterns[0].start.properties.as_ref().expect("props");
    assert!(props.entries.is_empty());
}

// -- WHERE / RETURN extras -----------------------------------

#[test]
fn where_predicate() {
    let p = parse_ok("MATCH (n) WHERE n.age > 30 RETURN n");
    let m = match_clause(one_query(&p));
    let pred = m.where_clause.as_ref().expect("where");
    assert!(matches!(
        pred,
        Expression::Binary {
            op: BinaryOp::Gt,
            ..
        }
    ));
}

#[test]
fn return_distinct() {
    let p = parse_ok("MATCH (n) RETURN DISTINCT n.label");
    let r = return_clause(one_query(&p));
    assert!(r.distinct);
}

#[test]
fn return_with_alias() {
    let p = parse_ok("MATCH (n) RETURN n.name AS user_name");
    let r = return_clause(one_query(&p));
    assert_eq!(r.items[0].alias.as_deref(), Some("user_name"));
}

#[test]
fn return_multiple_items() {
    let p = parse_ok("MATCH (a)-[r]->(b) RETURN a, r, b");
    let r = return_clause(one_query(&p));
    assert_eq!(r.items.len(), 3);
}

#[test]
fn order_by_asc_implicit_and_explicit() {
    let p = parse_ok("MATCH (n) RETURN n ORDER BY n.name, n.age DESC");
    let r = return_clause(one_query(&p));
    assert_eq!(r.order_by.len(), 2);
    assert_eq!(r.order_by[0].direction, SortDirection::Asc);
    assert_eq!(r.order_by[1].direction, SortDirection::Desc);
}

#[test]
fn skip_and_limit() {
    let p = parse_ok("MATCH (n) RETURN n SKIP 5 LIMIT 10");
    let r = return_clause(one_query(&p));
    assert!(matches!(
        r.skip,
        Some(Expression::IntegerLit { value: 5, .. })
    ));
    assert!(matches!(
        r.limit,
        Some(Expression::IntegerLit { value: 10, .. })
    ));
}

#[test]
fn offset_is_alias_for_skip() {
    let p = parse_ok("MATCH (n) RETURN n OFFSET 3");
    let r = return_clause(one_query(&p));
    assert!(matches!(
        r.skip,
        Some(Expression::IntegerLit { value: 3, .. })
    ));
}

// -- Expressions (Pratt) -------------------------------------

fn return_expr(input: &str) -> Expression {
    let p = parse_ok(input);
    return_clause(one_query(&p)).items[0].expr.clone()
}

#[test]
fn literal_integer() {
    assert!(matches!(
        return_expr("MATCH (n) RETURN 42"),
        Expression::IntegerLit { value: 42, .. }
    ));
}

#[test]
fn literal_float() {
    assert!(matches!(
        return_expr("MATCH (n) RETURN 1.25"),
        Expression::FloatLit { value, .. } if (value - 1.25).abs() < 1e-9
    ));
}

#[test]
fn literal_string() {
    assert!(matches!(
        return_expr("MATCH (n) RETURN 'hi'"),
        Expression::StringLit { value, .. } if value == "hi"
    ));
}

#[test]
fn literal_bool_and_null() {
    let p = parse_ok("MATCH (n) RETURN TRUE, FALSE, NULL");
    let r = return_clause(one_query(&p));
    assert!(matches!(
        &r.items[0].expr,
        Expression::BoolLit { value: true, .. }
    ));
    assert!(matches!(
        &r.items[1].expr,
        Expression::BoolLit { value: false, .. }
    ));
    assert!(matches!(&r.items[2].expr, Expression::NullLit { .. }));
}

#[test]
fn parameter_in_expression() {
    let p = parse_ok("MATCH (n) WHERE n.age > $minAge RETURN n");
    let pred = match_clause(one_query(&p)).where_clause.clone().unwrap();
    if let Expression::Binary { rhs, .. } = pred {
        assert!(matches!(*rhs, Expression::Parameter { name, .. } if name == "minAge"));
    } else {
        panic!("expected binary");
    }
}

#[test]
fn property_access_chains() {
    let expr = return_expr("MATCH (n) RETURN n.address.city");
    // n.address.city = (n.address).city
    if let Expression::Property { base, name, .. } = expr {
        assert_eq!(name, "city");
        if let Expression::Property { name: inner, .. } = *base {
            assert_eq!(inner, "address");
        } else {
            panic!("expected nested Property");
        }
    } else {
        panic!("expected Property");
    }
}

#[test]
fn arithmetic_precedence() {
    // 1 + 2 * 3 = 1 + (2 * 3)
    let expr = return_expr("MATCH (n) RETURN 1 + 2 * 3");
    if let Expression::Binary {
        op: BinaryOp::Add,
        lhs,
        rhs,
        ..
    } = expr
    {
        assert!(matches!(*lhs, Expression::IntegerLit { value: 1, .. }));
        assert!(matches!(
            *rhs,
            Expression::Binary {
                op: BinaryOp::Mul,
                ..
            }
        ));
    } else {
        panic!("expected `+` at the top");
    }
}

#[test]
fn arithmetic_left_associative() {
    // 10 - 3 - 2 = (10 - 3) - 2
    let expr = return_expr("MATCH (n) RETURN 10 - 3 - 2");
    if let Expression::Binary {
        op: BinaryOp::Sub,
        lhs,
        rhs,
        ..
    } = expr
    {
        assert!(matches!(
            *lhs,
            Expression::Binary {
                op: BinaryOp::Sub,
                ..
            }
        ));
        assert!(matches!(*rhs, Expression::IntegerLit { value: 2, .. }));
    } else {
        panic!("expected `-` at the top");
    }
}

#[test]
fn comparison_below_logical() {
    // a > 1 AND b < 2 = (a > 1) AND (b < 2)
    let expr = return_expr("MATCH (n) RETURN n.a > 1 AND n.b < 2");
    if let Expression::Binary {
        op: BinaryOp::And,
        lhs,
        rhs,
        ..
    } = expr
    {
        assert!(matches!(
            *lhs,
            Expression::Binary {
                op: BinaryOp::Gt,
                ..
            }
        ));
        assert!(matches!(
            *rhs,
            Expression::Binary {
                op: BinaryOp::Lt,
                ..
            }
        ));
    } else {
        panic!("expected AND at the top");
    }
}

#[test]
fn logical_precedence_or_below_and() {
    // a OR b AND c = a OR (b AND c)
    let expr = return_expr("MATCH (n) RETURN n.a OR n.b AND n.c");
    if let Expression::Binary {
        op: BinaryOp::Or,
        rhs,
        ..
    } = expr
    {
        assert!(matches!(
            *rhs,
            Expression::Binary {
                op: BinaryOp::And,
                ..
            }
        ));
    } else {
        panic!("expected OR at the top");
    }
}

#[test]
fn unary_not_right_associative() {
    let expr = return_expr("MATCH (n) RETURN NOT NOT n.flag");
    if let Expression::Unary {
        op: UnaryOp::Not,
        operand,
        ..
    } = expr
    {
        assert!(matches!(
            *operand,
            Expression::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
    } else {
        panic!("expected NOT");
    }
}

#[test]
fn unary_minus_with_space() {
    let expr = return_expr("MATCH (n) RETURN - 5");
    if let Expression::Unary {
        op: UnaryOp::Neg,
        operand,
        ..
    } = expr
    {
        assert!(matches!(*operand, Expression::IntegerLit { value: 5, .. }));
    } else {
        panic!("expected unary minus");
    }
}

#[test]
fn parens_override_precedence() {
    // (1 + 2) * 3 - top must be Mul.
    let expr = return_expr("MATCH (n) RETURN (1 + 2) * 3");
    if let Expression::Binary {
        op: BinaryOp::Mul,
        lhs,
        ..
    } = expr
    {
        // The lhs is a Paren wrapping the Add.
        assert!(matches!(*lhs, Expression::Paren { .. }));
    } else {
        panic!("expected Mul at the top");
    }
}

#[test]
fn neq_alternatives_both_work() {
    let e1 = return_expr("MATCH (n) RETURN n.a <> 1");
    let e2 = return_expr("MATCH (n) RETURN n.a != 1");
    for e in [e1, e2] {
        assert!(matches!(
            e,
            Expression::Binary {
                op: BinaryOp::Neq,
                ..
            }
        ));
    }
}

// -- Error cases ---------------------------------------------

#[test]
fn missing_return_is_an_error() {
    let errs = parse_err("MATCH (n)");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("RETURN") || e.message.contains("missing"))
    );
}

#[test]
fn match_without_open_paren_is_an_error() {
    let errs = parse_err("MATCH n) RETURN n");
    assert!(errs.iter().any(|e| e.message.contains("`(`")));
}

#[test]
fn return_without_expression_is_an_error() {
    let errs = parse_err("MATCH (n) RETURN");
    assert!(errs.iter().any(|e| e.message.contains("expression")));
}

#[test]
fn unclosed_property_map_is_an_error() {
    let errs = parse_err("MATCH (n {name: 'a') RETURN n");
    assert!(errs.iter().any(|e| e.message.contains("`}`")));
}

#[test]
fn unclosed_paren_in_expression_is_an_error() {
    let errs = parse_err("MATCH (n) RETURN (1 + 2");
    assert!(errs.iter().any(|e| e.message.contains("`)`")));
}

// -- Spans --------------------------------------------------

#[test]
fn match_clause_span_covers_keyword_through_where() {
    let p = parse_ok("MATCH (n) WHERE n.x = 1 RETURN n");
    let m = match_clause(one_query(&p));
    assert_eq!(
        &"MATCH (n) WHERE n.x = 1 RETURN n"[m.span.start..m.span.end],
        "MATCH (n) WHERE n.x = 1"
    );
}

#[test]
fn return_clause_span_covers_keyword_through_limit() {
    let p = parse_ok("MATCH (n) RETURN n LIMIT 10");
    let r = return_clause(one_query(&p));
    assert_eq!(
        &"MATCH (n) RETURN n LIMIT 10"[r.span.start..r.span.end],
        "RETURN n LIMIT 10"
    );
}

#[test]
fn expression_span_covers_full_expression() {
    let p = parse_ok("MATCH (n) RETURN 1 + 2 * 3");
    let r = return_clause(one_query(&p));
    let span = r.items[0].expr.span();
    assert_eq!(
        &"MATCH (n) RETURN 1 + 2 * 3"[span.start..span.end],
        "1 + 2 * 3"
    );
}

// -- Reserved-word usage as a name ---------------------------

#[test]
fn delimited_ident_lets_you_use_reserved_spelling() {
    let p = parse_ok("MATCH (`match`) RETURN `match`");
    let m = match_clause(one_query(&p));
    assert_eq!(m.patterns[0].start.variable.as_deref(), Some("match"));
}

// -- Multi-statement (semicolon-separated) -------------------

#[test]
fn semicolon_separated_statements() {
    let p = parse_ok("MATCH (a) RETURN a; MATCH (b) RETURN b");
    assert_eq!(p.statements.len(), 2);
}

#[test]
fn trailing_semicolon_is_legal() {
    let p = parse_ok("MATCH (n) RETURN n;");
    assert_eq!(p.statements.len(), 1);
}

// -- Function calls ------------------------------------------

fn with_clause(q: &QueryStatement) -> &WithClause {
    for c in &q.head.clauses {
        if let Clause::With(w) = c {
            return w;
        }
    }
    panic!("no WITH clause: {:?}", q.head.clauses);
}

fn unwind_clause(q: &QueryStatement) -> &UnwindClause {
    for c in &q.head.clauses {
        if let Clause::Unwind(u) = c {
            return u;
        }
    }
    panic!("no UNWIND clause: {:?}", q.head.clauses);
}

#[test]
fn function_call_no_args() {
    let expr = return_expr("MATCH (n) RETURN current_timestamp()");
    if let Expression::FunctionCall {
        name,
        args,
        distinct,
        ..
    } = expr
    {
        assert_eq!(name, "current_timestamp");
        assert!(args.is_empty());
        assert!(!distinct);
    } else {
        panic!("expected function call");
    }
}

#[test]
fn function_call_with_args() {
    let expr = return_expr("MATCH (n) RETURN substring(n.name, 0, 3)");
    if let Expression::FunctionCall { name, args, .. } = expr {
        assert_eq!(name, "substring");
        assert_eq!(args.len(), 3);
    } else {
        panic!("expected function call");
    }
}

#[test]
fn function_call_distinct_modifier() {
    let expr = return_expr("MATCH (n) RETURN count(DISTINCT n)");
    if let Expression::FunctionCall {
        name,
        distinct,
        args,
        ..
    } = expr
    {
        assert_eq!(name, "count");
        assert!(distinct);
        assert_eq!(args.len(), 1);
    } else {
        panic!("expected function call");
    }
}

#[test]
fn function_call_nested() {
    let expr = return_expr("MATCH (n) RETURN abs(toFloat(n.score))");
    if let Expression::FunctionCall { name, args, .. } = expr {
        assert_eq!(name, "abs");
        assert!(matches!(args[0], Expression::FunctionCall { .. }));
    } else {
        panic!("expected outer function call");
    }
}

#[test]
fn function_call_in_predicate() {
    let p = parse_ok("MATCH (n) WHERE length(n.name) > 5 RETURN n");
    let pred = match_clause(one_query(&p)).where_clause.clone().unwrap();
    if let Expression::Binary { lhs, .. } = pred {
        assert!(matches!(*lhs, Expression::FunctionCall { .. }));
    } else {
        panic!("expected binary");
    }
}

// -- List literals + indexing --------------------------------

#[test]
fn list_literal_empty() {
    let expr = return_expr("MATCH (n) RETURN []");
    if let Expression::List { items, .. } = expr {
        assert!(items.is_empty());
    } else {
        panic!("expected list");
    }
}

#[test]
fn list_literal_with_items() {
    let expr = return_expr("MATCH (n) RETURN [1, 2, 3]");
    if let Expression::List { items, .. } = expr {
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], Expression::IntegerLit { value: 1, .. }));
    } else {
        panic!("expected list");
    }
}

#[test]
fn list_literal_mixed_expressions() {
    let expr = return_expr("MATCH (n) RETURN [n.name, 1 + 2, 'lit']");
    if let Expression::List { items, .. } = expr {
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], Expression::Property { .. }));
        assert!(matches!(items[1], Expression::Binary { .. }));
        assert!(matches!(items[2], Expression::StringLit { .. }));
    } else {
        panic!("expected list");
    }
}

#[test]
fn index_into_list() {
    let expr = return_expr("MATCH (n) RETURN xs[0]");
    if let Expression::Index { base, index, .. } = expr {
        assert!(matches!(*base, Expression::Variable { .. }));
        assert!(matches!(*index, Expression::IntegerLit { value: 0, .. }));
    } else {
        panic!("expected index");
    }
}

#[test]
fn index_chain_after_property() {
    let expr = return_expr("MATCH (n) RETURN n.tags[2]");
    if let Expression::Index { base, .. } = expr {
        assert!(matches!(*base, Expression::Property { .. }));
    } else {
        panic!("expected index");
    }
}

// -- WITH clause ---------------------------------------------

#[test]
fn with_basic() {
    let p = parse_ok("MATCH (n) WITH n RETURN n");
    let w = with_clause(one_query(&p));
    assert_eq!(w.items.len(), 1);
    assert!(!w.distinct);
    assert!(w.where_clause.is_none());
}

#[test]
fn with_distinct_alias() {
    let p = parse_ok("MATCH (n) WITH DISTINCT n.label AS lbl RETURN lbl");
    let w = with_clause(one_query(&p));
    assert!(w.distinct);
    assert_eq!(w.items[0].alias.as_deref(), Some("lbl"));
}

#[test]
fn with_where_filters_projection() {
    let p = parse_ok("MATCH (n) WITH n.age AS a WHERE a > 18 RETURN a");
    let w = with_clause(one_query(&p));
    assert!(w.where_clause.is_some());
}

#[test]
fn with_order_skip_limit() {
    let p = parse_ok("MATCH (n) WITH n.score AS s ORDER BY s DESC SKIP 5 LIMIT 10 RETURN s");
    let w = with_clause(one_query(&p));
    assert_eq!(w.order_by.len(), 1);
    assert_eq!(w.order_by[0].direction, SortDirection::Desc);
    assert!(w.skip.is_some());
    assert!(w.limit.is_some());
}

#[test]
fn multiple_with_clauses() {
    // Chained WITH - common shape for staged aggregations.
    let p = parse_ok(
        "MATCH (n) WITH n.dept AS d, count(n) AS c \
         WITH d, c WHERE c > 10 \
         RETURN d, c",
    );
    let q = one_query(&p);
    let withs: Vec<_> = q
        .head
        .clauses
        .iter()
        .filter(|c| matches!(c, Clause::With(_)))
        .collect();
    assert_eq!(withs.len(), 2);
}

// -- UNWIND clause -------------------------------------------

#[test]
fn unwind_basic() {
    let p = parse_ok("UNWIND [1, 2, 3] AS x RETURN x");
    let u = unwind_clause(one_query(&p));
    assert_eq!(u.alias, "x");
    assert!(matches!(u.expr, Expression::List { .. }));
}

#[test]
fn unwind_after_match() {
    let p = parse_ok("MATCH (n) UNWIND n.tags AS tag RETURN tag");
    let u = unwind_clause(one_query(&p));
    assert_eq!(u.alias, "tag");
    assert!(matches!(u.expr, Expression::Property { .. }));
}

#[test]
fn unwind_missing_alias_is_an_error() {
    let errs = parse_err("UNWIND [1,2,3] RETURN x");
    assert!(errs.iter().any(|e| e.message.contains("AS")));
}

// -- Spans on new constructs ---------------------------------

#[test]
fn function_call_span_includes_parens() {
    let p = parse_ok("MATCH (n) RETURN count(n)");
    let r = return_clause(one_query(&p));
    let span = r.items[0].expr.span();
    assert_eq!(
        &"MATCH (n) RETURN count(n)"[span.start..span.end],
        "count(n)"
    );
}

#[test]
fn list_literal_span_includes_brackets() {
    let p = parse_ok("MATCH (n) RETURN [1, 2]");
    let r = return_clause(one_query(&p));
    let span = r.items[0].expr.span();
    assert_eq!(&"MATCH (n) RETURN [1, 2]"[span.start..span.end], "[1, 2]");
}

// -- Label expressions --------------------------------------

fn node_label(input: &str) -> LabelExpression {
    let p = parse_ok(input);
    match_clause(one_query(&p)).patterns[0]
        .start
        .label_expr
        .clone()
        .expect("label expression")
}

#[test]
fn label_conjunction() {
    match node_label("MATCH (n:Movie&Award) RETURN n") {
        LabelExpression::Conjunction { operands, .. } => {
            assert_eq!(operands.len(), 2);
            assert!(
                matches!(operands[0], LabelExpression::Label { ref name, .. } if name == "Movie")
            );
            assert!(
                matches!(operands[1], LabelExpression::Label { ref name, .. } if name == "Award")
            );
        }
        other => panic!("expected Conjunction, got {other:?}"),
    }
}

#[test]
fn label_disjunction() {
    match node_label("MATCH (n:Movie|Film) RETURN n") {
        LabelExpression::Disjunction { operands, .. } => {
            assert_eq!(operands.len(), 2);
        }
        other => panic!("expected Disjunction, got {other:?}"),
    }
}

#[test]
fn label_negation() {
    match node_label("MATCH (n:!Hidden) RETURN n") {
        LabelExpression::Negation { operand, .. } => {
            assert!(
                matches!(*operand, LabelExpression::Label { ref name, .. } if name == "Hidden")
            );
        }
        other => panic!("expected Negation, got {other:?}"),
    }
}

#[test]
fn label_wildcard() {
    assert!(matches!(
        node_label("MATCH (n:%) RETURN n"),
        LabelExpression::Wildcard { .. }
    ));
}

#[test]
fn label_and_below_or() {
    // `:A | B & C` = A | (B & C)
    match node_label("MATCH (n:A|B&C) RETURN n") {
        LabelExpression::Disjunction { operands, .. } => {
            assert_eq!(operands.len(), 2);
            assert!(matches!(operands[1], LabelExpression::Conjunction { .. }));
        }
        other => panic!("expected Disjunction at top, got {other:?}"),
    }
}

#[test]
fn label_parens_override_precedence() {
    // `:(A | B) & C` = (A | B) & C - conjunction at top.
    match node_label("MATCH (n:(A|B)&C) RETURN n") {
        LabelExpression::Conjunction { operands, .. } => {
            assert!(matches!(operands[0], LabelExpression::Disjunction { .. }));
            assert!(matches!(operands[1], LabelExpression::Label { ref name, .. } if name == "C"));
        }
        other => panic!("expected Conjunction at top, got {other:?}"),
    }
}

#[test]
fn label_negation_binds_tightest() {
    // `:!A & B` = (!A) & B
    match node_label("MATCH (n:!A&B) RETURN n") {
        LabelExpression::Conjunction { operands, .. } => {
            assert!(matches!(operands[0], LabelExpression::Negation { .. }));
        }
        other => panic!("expected Conjunction at top, got {other:?}"),
    }
}

#[test]
fn edge_label_disjunction() {
    let p = parse_ok("MATCH (a)-[:KNOWS|LIKES]->(b) RETURN a");
    let (edge, _) = step_of(&match_clause(one_query(&p)).patterns[0].chain[0]);
    assert!(matches!(
        edge.label_expr,
        Some(LabelExpression::Disjunction { .. })
    ));
}

// -- Edge quantifiers ---------------------------------------

fn first_edge_quantifier(p: &Program) -> EdgeQuantifier {
    step_of(&match_clause(one_query(p)).patterns[0].chain[0])
        .0
        .quantifier
        .expect("quantifier")
}

#[test]
fn quantifier_open_star() {
    let p = parse_ok("MATCH (a)-[*]->(b) RETURN a");
    let q = first_edge_quantifier(&p);
    assert_eq!(q.min, None);
    assert_eq!(q.max, None);
}

#[test]
fn quantifier_exact() {
    let p = parse_ok("MATCH (a)-[*3]->(b) RETURN a");
    let q = first_edge_quantifier(&p);
    assert_eq!(q.min, Some(3));
    assert_eq!(q.max, Some(3));
}

#[test]
fn quantifier_lower_bound() {
    let p = parse_ok("MATCH (a)-[*2..]->(b) RETURN a");
    let q = first_edge_quantifier(&p);
    assert_eq!(q.min, Some(2));
    assert_eq!(q.max, None);
}

#[test]
fn quantifier_upper_bound() {
    let p = parse_ok("MATCH (a)-[*..5]->(b) RETURN a");
    let q = first_edge_quantifier(&p);
    assert_eq!(q.min, None);
    assert_eq!(q.max, Some(5));
}

#[test]
fn quantifier_range() {
    let p = parse_ok("MATCH (a)-[*1..3]->(b) RETURN a");
    let q = first_edge_quantifier(&p);
    assert_eq!(q.min, Some(1));
    assert_eq!(q.max, Some(3));
}

#[test]
fn quantifier_with_typed_edge() {
    let p = parse_ok("MATCH (a)-[r:KNOWS*1..3]->(b) RETURN r");
    let (edge, _) = step_of(&match_clause(one_query(&p)).patterns[0].chain[0]);
    assert_eq!(edge.variable.as_deref(), Some("r"));
    assert!(matches!(
        edge.label_expr,
        Some(LabelExpression::Label { .. })
    ));
    let q = edge.quantifier.expect("quantifier");
    assert_eq!(q.min, Some(1));
    assert_eq!(q.max, Some(3));
}

// -- UNION --------------------------------------------------

#[test]
fn union_distinct_default() {
    let p = parse_ok("MATCH (a) RETURN a UNION MATCH (b) RETURN b");
    let q = one_query(&p);
    assert_eq!(q.unions.len(), 1);
    assert_eq!(q.unions[0].kind, UnionKind::Distinct);
}

#[test]
fn union_all() {
    let p = parse_ok("MATCH (a) RETURN a UNION ALL MATCH (b) RETURN b");
    let q = one_query(&p);
    assert_eq!(q.unions[0].kind, UnionKind::All);
}

#[test]
fn union_distinct_explicit() {
    let p = parse_ok("MATCH (a) RETURN a UNION DISTINCT MATCH (b) RETURN b");
    let q = one_query(&p);
    assert_eq!(q.unions[0].kind, UnionKind::Distinct);
}

#[test]
fn union_chained() {
    let p = parse_ok("MATCH (a) RETURN a UNION MATCH (b) RETURN b UNION ALL MATCH (c) RETURN c");
    let q = one_query(&p);
    assert_eq!(q.unions.len(), 2);
    assert_eq!(q.unions[0].kind, UnionKind::Distinct);
    assert_eq!(q.unions[1].kind, UnionKind::All);
}

#[test]
fn union_part_body_has_its_own_return() {
    let p = parse_ok("MATCH (a) RETURN a UNION MATCH (b) RETURN b");
    let q = one_query(&p);
    // The head body has its own RETURN; the union part's body
    // also ends in RETURN.
    assert!(matches!(q.head.clauses.last(), Some(Clause::Return(_))));
    assert!(matches!(
        q.unions[0].body.clauses.last(),
        Some(Clause::Return(_))
    ));
}

// -- DML: INSERT / SET / REMOVE / DELETE / FINISH -----------

fn first_clause<'a, T, F: Fn(&'a Clause) -> Option<&'a T>>(q: &'a QueryStatement, f: F) -> &'a T {
    q.head
        .clauses
        .iter()
        .find_map(f)
        .unwrap_or_else(|| panic!("clause not found in: {:?}", q.head.clauses))
}

#[test]
fn insert_single_node() {
    let p = parse_ok("INSERT (n:Person {name: 'Alice'}) RETURN n");
    let q = one_query(&p);
    let ins = first_clause(q, |c| {
        if let Clause::Insert(i) = c {
            Some(i)
        } else {
            None
        }
    });
    assert_eq!(ins.patterns.len(), 1);
    assert!(ins.patterns[0].chain.is_empty());
    assert!(matches!(
        ins.patterns[0].start.label_expr,
        Some(LabelExpression::Label { ref name, .. }) if name == "Person"
    ));
}

#[test]
fn insert_edge() {
    let p = parse_ok("MATCH (a), (b) INSERT (a)-[:KNOWS]->(b) RETURN a");
    let q = one_query(&p);
    let ins = first_clause(q, |c| {
        if let Clause::Insert(i) = c {
            Some(i)
        } else {
            None
        }
    });
    assert_eq!(ins.patterns[0].chain.len(), 1);
    assert_eq!(
        step_of(&ins.patterns[0].chain[0]).0.direction,
        EdgeDirection::Outgoing
    );
}

#[test]
fn create_as_cypher_alias_for_insert() {
    // ISO 39075 uses INSERT; openCypher uses CREATE. The TCK is
    // inherited from openCypher and still uses CREATE in its
    // data-write preambles, so we accept both.
    let p = parse_ok("CREATE (n:Person {name: 'a'}) RETURN n.name");
    let q = one_query(&p);
    let ins = first_clause(q, |c| {
        if let Clause::Insert(i) = c {
            Some(i)
        } else {
            None
        }
    });
    assert_eq!(ins.patterns.len(), 1);
    assert!(matches!(
        ins.patterns[0].start.label_expr,
        Some(LabelExpression::Label { ref name, .. }) if name == "Person"
    ));
}

#[test]
fn create_data_distinguished_from_create_graph() {
    // `CREATE GRAPH foo` is still a catalog statement; the
    // lookahead-disambiguation in parse_statement keeps the two
    // paths separate.
    let p1 = parse_ok("CREATE GRAPH foo");
    assert!(matches!(p1.statements[0], Statement::CreateGraph { .. }));
    let p2 = parse_ok("CREATE (n) RETURN n");
    assert!(matches!(p2.statements[0], Statement::Query(_)));
}

#[test]
fn insert_without_return_is_valid() {
    // INSERT-only queries can terminate without RETURN.
    let p = parse_ok("INSERT (n:Person {name: 'Bob'})");
    let q = one_query(&p);
    assert!(matches!(q.head.clauses.last(), Some(Clause::Insert(_))));
}

#[test]
fn set_property_assign() {
    let p = parse_ok("MATCH (n) SET n.age = 30 RETURN n");
    let q = one_query(&p);
    let s = first_clause(q, |c| {
        if let Clause::Set(s) = c {
            Some(s)
        } else {
            None
        }
    });
    assert_eq!(s.items.len(), 1);
    match &s.items[0] {
        SetItem::Property { target, value, .. } => {
            assert!(matches!(target, Expression::Property { .. }));
            assert!(matches!(value, Expression::IntegerLit { value: 30, .. }));
        }
        other => panic!("expected Property, got {other:?}"),
    }
}

#[test]
fn set_merge_map() {
    let p = parse_ok("MATCH (n) SET n += $patch RETURN n");
    let q = one_query(&p);
    let s = first_clause(q, |c| {
        if let Clause::Set(s) = c {
            Some(s)
        } else {
            None
        }
    });
    assert!(matches!(&s.items[0], SetItem::Merge { .. }));
}

#[test]
fn set_labels() {
    let p = parse_ok("MATCH (n) SET n:Visited:Pending RETURN n");
    let q = one_query(&p);
    let s = first_clause(q, |c| {
        if let Clause::Set(s) = c {
            Some(s)
        } else {
            None
        }
    });
    match &s.items[0] {
        SetItem::Labels { labels, .. } => {
            assert_eq!(labels, &vec!["Visited".to_string(), "Pending".to_string()])
        }
        other => panic!("expected Labels, got {other:?}"),
    }
}

#[test]
fn set_multiple_items() {
    let p = parse_ok("MATCH (n) SET n.age = 30, n.updated = true RETURN n");
    let q = one_query(&p);
    let s = first_clause(q, |c| {
        if let Clause::Set(s) = c {
            Some(s)
        } else {
            None
        }
    });
    assert_eq!(s.items.len(), 2);
}

#[test]
fn remove_property() {
    let p = parse_ok("MATCH (n) REMOVE n.temp RETURN n");
    let q = one_query(&p);
    let r = first_clause(q, |c| {
        if let Clause::Remove(r) = c {
            Some(r)
        } else {
            None
        }
    });
    assert!(matches!(&r.items[0], RemoveItem::Property { .. }));
}

#[test]
fn remove_labels() {
    let p = parse_ok("MATCH (n) REMOVE n:Pending RETURN n");
    let q = one_query(&p);
    let r = first_clause(q, |c| {
        if let Clause::Remove(r) = c {
            Some(r)
        } else {
            None
        }
    });
    match &r.items[0] {
        RemoveItem::Labels { labels, .. } => assert_eq!(labels, &vec!["Pending".to_string()]),
        other => panic!("expected Labels, got {other:?}"),
    }
}

#[test]
fn delete_simple() {
    let p = parse_ok("MATCH (n) DELETE n");
    let q = one_query(&p);
    let d = first_clause(q, |c| {
        if let Clause::Delete(d) = c {
            Some(d)
        } else {
            None
        }
    });
    assert!(!d.detach);
    assert_eq!(d.targets.len(), 1);
}

#[test]
fn delete_detach() {
    let p = parse_ok("MATCH (n) DETACH DELETE n");
    let q = one_query(&p);
    let d = first_clause(q, |c| {
        if let Clause::Delete(d) = c {
            Some(d)
        } else {
            None
        }
    });
    assert!(d.detach);
}

#[test]
fn delete_multiple_targets() {
    let p = parse_ok("MATCH (a)-[r]->(b) DELETE r, a, b");
    let q = one_query(&p);
    let d = first_clause(q, |c| {
        if let Clause::Delete(d) = c {
            Some(d)
        } else {
            None
        }
    });
    assert_eq!(d.targets.len(), 3);
}

#[test]
fn finish_terminates_query() {
    let p = parse_ok("MATCH (n) SET n.touched = true FINISH");
    let q = one_query(&p);
    assert!(matches!(q.head.clauses.last(), Some(Clause::Finish(_))));
}

#[test]
fn read_only_query_without_return_is_an_error() {
    let errs = parse_err("MATCH (n)");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("RETURN") || e.message.contains("FINISH"))
    );
}

#[test]
fn write_then_read_chain() {
    // SET followed by RETURN reads back the mutated row.
    let p = parse_ok("MATCH (n) SET n.touched = true RETURN n");
    let q = one_query(&p);
    let kinds: Vec<_> = q
        .head
        .clauses
        .iter()
        .map(|c| match c {
            Clause::Match(_) => "match",
            Clause::Set(_) => "set",
            Clause::Return(_) => "return",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["match", "set", "return"]);
}

#[test]
fn set_then_delete() {
    let p = parse_ok("MATCH (n) SET n.archived = true DETACH DELETE n");
    let q = one_query(&p);
    let kinds: Vec<_> = q
        .head
        .clauses
        .iter()
        .map(|c| match c {
            Clause::Match(_) => "match",
            Clause::Set(_) => "set",
            Clause::Delete(_) => "delete",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["match", "set", "delete"]);
}

// Span coverage for DML clauses
#[test]
fn insert_clause_span_covers_keyword_through_pattern() {
    let src = "INSERT (n:Foo) RETURN n";
    let p = parse_ok(src);
    let ins = first_clause(one_query(&p), |c| {
        if let Clause::Insert(i) = c {
            Some(i)
        } else {
            None
        }
    });
    assert_eq!(&src[ins.span.start..ins.span.end], "INSERT (n:Foo)");
}

#[test]
fn detach_delete_clause_span_covers_both_keywords() {
    let src = "MATCH (n) DETACH DELETE n";
    let p = parse_ok(src);
    let d = first_clause(one_query(&p), |c| {
        if let Clause::Delete(d) = c {
            Some(d)
        } else {
            None
        }
    });
    assert_eq!(&src[d.span.start..d.span.end], "DETACH DELETE n");
}

// -- Catalog statements (CREATE / DROP / USE) ---------------

#[test]
fn create_graph_basic() {
    let p = parse_ok("CREATE GRAPH social");
    match &p.statements[0] {
        Statement::CreateGraph {
            path,
            if_not_exists,
            body,
            ..
        } => {
            assert_eq!(path.segments, vec!["social"]);
            assert!(!path.absolute);
            assert!(!if_not_exists);
            assert!(body.is_none(), "bare Cypher-style form has no body");
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_if_not_exists() {
    let p = parse_ok("CREATE GRAPH IF NOT EXISTS social");
    match &p.statements[0] {
        Statement::CreateGraph {
            path,
            if_not_exists,
            body,
            ..
        } => {
            assert_eq!(path.segments, vec!["social"]);
            assert!(*if_not_exists);
            assert!(body.is_none());
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn drop_graph_if_exists() {
    let p = parse_ok("DROP GRAPH IF EXISTS social");
    match &p.statements[0] {
        Statement::DropGraph {
            path, if_exists, ..
        } => {
            assert_eq!(path.segments, vec!["social"]);
            assert!(*if_exists);
        }
        other => panic!("expected DropGraph, got {other:?}"),
    }
}

#[test]
fn create_index_named() {
    let p = parse_ok("CREATE INDEX person_name FOR (n:Person) ON (n.name)");
    match &p.statements[0] {
        Statement::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            assert_eq!(name.as_deref(), Some("person_name"));
            assert_eq!(label, "Person");
            assert_eq!(property, "name");
        }
        other => panic!("expected CreateIndex, got {other:?}"),
    }
}

#[test]
fn create_index_anonymous() {
    let p = parse_ok("CREATE INDEX FOR (n:Movie) ON (n.title)");
    match &p.statements[0] {
        Statement::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            assert!(name.is_none());
            assert_eq!(label, "Movie");
            assert_eq!(property, "title");
        }
        other => panic!("expected CreateIndex, got {other:?}"),
    }
}

#[test]
fn drop_index() {
    let p = parse_ok("DROP INDEX person_name");
    match &p.statements[0] {
        Statement::DropIndex { name, .. } => assert_eq!(name, "person_name"),
        other => panic!("expected DropIndex, got {other:?}"),
    }
}

#[test]
fn create_constraint_unique() {
    let p = parse_ok("CREATE CONSTRAINT email_uniq FOR (u:User) REQUIRE u.email IS UNIQUE");
    match &p.statements[0] {
        Statement::CreateConstraint {
            name,
            label,
            property,
            kind,
            ..
        } => {
            assert_eq!(name.as_deref(), Some("email_uniq"));
            assert_eq!(label, "User");
            assert_eq!(property, "email");
            assert_eq!(*kind, ConstraintKind::Unique);
        }
        other => panic!("expected CreateConstraint, got {other:?}"),
    }
}

#[test]
fn create_constraint_not_null() {
    let p = parse_ok("CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL");
    match &p.statements[0] {
        Statement::CreateConstraint { name, kind, .. } => {
            assert!(name.is_none());
            assert_eq!(*kind, ConstraintKind::NotNull);
        }
        other => panic!("expected CreateConstraint, got {other:?}"),
    }
}

#[test]
fn drop_constraint() {
    let p = parse_ok("DROP CONSTRAINT email_uniq");
    match &p.statements[0] {
        Statement::DropConstraint { name, .. } => assert_eq!(name, "email_uniq"),
        other => panic!("expected DropConstraint, got {other:?}"),
    }
}

#[test]
fn use_statement() {
    let p = parse_ok("USE social");
    match &p.statements[0] {
        Statement::Use { graph, .. } => assert_eq!(graph, "social"),
        other => panic!("expected Use, got {other:?}"),
    }
}

#[test]
fn catalog_errors_after_create() {
    // `CREATE TABLE` isn't a catalog statement (TABLE isn't a catalog keyword), so it goes down
    // the query path and fails at `TABLE` where a pattern's `(` is expected
    let errs = parse_err("CREATE TABLE foo");
    assert!(
        errs.iter().any(|e| e.message.contains("`(`")),
        "expected an error about missing `(`, got: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// -- CALL clause / procedures -------------------------------

#[test]
fn call_no_args_no_yield() {
    let p = parse_ok("CALL db.labels()");
    let q = one_query(&p);
    let call = first_clause(q, |c| {
        if let Clause::Call(c) = c {
            Some(c)
        } else {
            None
        }
    });
    assert_eq!(call.name, vec!["db".to_string(), "labels".to_string()]);
    assert!(call.args.is_empty());
    assert!(call.yields.is_empty());
}

#[test]
fn call_with_args() {
    let p = parse_ok("CALL traverse.graph.subgraph('movies', 100)");
    let q = one_query(&p);
    let call = first_clause(q, |c| {
        if let Clause::Call(c) = c {
            Some(c)
        } else {
            None
        }
    });
    assert_eq!(call.name.len(), 3);
    assert_eq!(call.args.len(), 2);
}

#[test]
fn call_with_yield() {
    let p = parse_ok("CALL db.indexes() YIELD name, label");
    let q = one_query(&p);
    let call = first_clause(q, |c| {
        if let Clause::Call(c) = c {
            Some(c)
        } else {
            None
        }
    });
    assert_eq!(call.yields.len(), 2);
    assert_eq!(call.yields[0].field, "name");
    assert_eq!(call.yields[1].field, "label");
}

#[test]
fn call_with_yield_alias() {
    let p = parse_ok("CALL db.indexes() YIELD name AS idxName");
    let q = one_query(&p);
    let call = first_clause(q, |c| {
        if let Clause::Call(c) = c {
            Some(c)
        } else {
            None
        }
    });
    assert_eq!(call.yields[0].alias.as_deref(), Some("idxName"));
}

#[test]
fn call_then_return() {
    let p = parse_ok("CALL db.indexes() YIELD name, label RETURN name, label");
    let q = one_query(&p);
    assert!(q.head.clauses.iter().any(|c| matches!(c, Clause::Call(_))));
    assert!(matches!(q.head.clauses.last(), Some(Clause::Return(_))));
}

#[test]
fn match_then_call_then_return() {
    let p = parse_ok("MATCH (n:Person) CALL stats.compute(n) YIELD score RETURN n, score");
    let q = one_query(&p);
    let kinds: Vec<_> = q
        .head
        .clauses
        .iter()
        .map(|c| match c {
            Clause::Match(_) => "match",
            Clause::Call(_) => "call",
            Clause::Return(_) => "return",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["match", "call", "return"]);
}

// Span coverage for catalog statements
#[test]
fn create_graph_span_covers_keywords_and_name() {
    let src = "CREATE GRAPH IF NOT EXISTS social";
    let p = parse_ok(src);
    let span = match &p.statements[0] {
        Statement::CreateGraph { span, .. } => *span,
        _ => unreachable!(),
    };
    assert_eq!(
        &src[span.start..span.end],
        "CREATE GRAPH IF NOT EXISTS social"
    );
}

#[test]
fn call_span_covers_keyword_through_yield() {
    let src = "CALL db.indexes() YIELD name AS idxName";
    let p = parse_ok(src);
    let q = one_query(&p);
    let call = first_clause(q, |c| {
        if let Clause::Call(c) = c {
            Some(c)
        } else {
            None
        }
    });
    assert_eq!(&src[call.span.start..call.span.end], src);
}

// -- FOR / FILTER (GQL comprehensions) ----------------------

#[test]
fn for_in_list() {
    let p = parse_ok("FOR x IN [1, 2, 3] RETURN x");
    let q = one_query(&p);
    let f = first_clause(q, |c| {
        if let Clause::For(f) = c {
            Some(f)
        } else {
            None
        }
    });
    assert_eq!(f.variable, "x");
    assert!(matches!(f.list, Expression::List { .. }));
}

#[test]
fn for_nested_then_return() {
    let p = parse_ok(
        "FOR a IN [true, false] \
         FOR b IN [true, false] \
         RETURN a, b",
    );
    let q = one_query(&p);
    let fors: Vec<_> = q
        .head
        .clauses
        .iter()
        .filter(|c| matches!(c, Clause::For(_)))
        .collect();
    assert_eq!(fors.len(), 2);
}

#[test]
fn filter_with_where() {
    let p = parse_ok("FOR x IN xs FILTER WHERE x > 0 RETURN x");
    let q = one_query(&p);
    let fil = first_clause(q, |c| {
        if let Clause::Filter(f) = c {
            Some(f)
        } else {
            None
        }
    });
    assert!(matches!(
        fil.predicate,
        Expression::Binary {
            op: BinaryOp::Gt,
            ..
        }
    ));
}

#[test]
fn filter_without_where() {
    // The elided form should produce the same AST.
    let p = parse_ok("FOR x IN xs FILTER x > 0 RETURN x");
    let q = one_query(&p);
    let fil = first_clause(q, |c| {
        if let Clause::Filter(f) = c {
            Some(f)
        } else {
            None
        }
    });
    assert!(matches!(
        fil.predicate,
        Expression::Binary {
            op: BinaryOp::Gt,
            ..
        }
    ));
}

// -- IS NULL / IS NOT NULL ----------------------------------

#[test]
fn is_null_predicate() {
    let p = parse_ok("MATCH (n) WHERE n.name IS NULL RETURN n");
    let pred = match_clause(one_query(&p)).where_clause.clone().unwrap();
    match pred {
        Expression::IsNull {
            operand, negated, ..
        } => {
            assert!(!negated);
            assert!(matches!(*operand, Expression::Property { .. }));
        }
        other => panic!("expected IsNull, got {other:?}"),
    }
}

#[test]
fn is_not_null_predicate() {
    let p = parse_ok("MATCH (n) WHERE n.name IS NOT NULL RETURN n");
    let pred = match_clause(one_query(&p)).where_clause.clone().unwrap();
    match pred {
        Expression::IsNull { negated, .. } => assert!(negated),
        other => panic!("expected IsNull, got {other:?}"),
    }
}

#[test]
fn is_null_combines_with_or() {
    // `a IS NULL OR b IS NULL` - exercises that IS NULL sits at
    // comparison precedence, below the logical operators.
    let p = parse_ok(
        "FOR a IN [true, false, null] \
         FOR b IN [true, false, null] \
         FILTER WHERE a IS NULL OR b IS NULL \
         RETURN a, b",
    );
    let q = one_query(&p);
    let fil = first_clause(q, |c| {
        if let Clause::Filter(f) = c {
            Some(f)
        } else {
            None
        }
    });
    match &fil.predicate {
        Expression::Binary {
            op: BinaryOp::Or,
            lhs,
            rhs,
            ..
        } => {
            assert!(matches!(**lhs, Expression::IsNull { .. }));
            assert!(matches!(**rhs, Expression::IsNull { .. }));
        }
        other => panic!("expected `Or(IsNull, IsNull)`, got {other:?}"),
    }
}

#[test]
fn for_clause_span_covers_keyword_through_list() {
    let src = "FOR x IN [1, 2] RETURN x";
    let p = parse_ok(src);
    let q = one_query(&p);
    let f = first_clause(q, |c| {
        if let Clause::For(f) = c {
            Some(f)
        } else {
            None
        }
    });
    assert_eq!(&src[f.span.start..f.span.end], "FOR x IN [1, 2]");
}

// -- GQL catalog statements ---------------------------------

#[test]
fn create_schema_root() {
    let p = parse_ok("CREATE SCHEMA /myschema");
    match &p.statements[0] {
        Statement::CreateSchema { path, .. } => {
            assert!(path.absolute);
            assert_eq!(path.segments, vec!["myschema"]);
        }
        other => panic!("expected CreateSchema, got {other:?}"),
    }
}

#[test]
fn create_schema_nested_path() {
    let p = parse_ok("CREATE SCHEMA /foo/myschema");
    match &p.statements[0] {
        Statement::CreateSchema { path, .. } => {
            assert!(path.absolute);
            assert_eq!(path.segments, vec!["foo", "myschema"]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn drop_schema_if_exists() {
    let p = parse_ok("DROP SCHEMA IF EXISTS /foo/myschema");
    match &p.statements[0] {
        Statement::DropSchema {
            path, if_exists, ..
        } => {
            assert!(*if_exists);
            assert_eq!(path.segments, vec!["foo", "myschema"]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn create_graph_with_any_body() {
    let p = parse_ok("CREATE GRAPH mygraph ANY");
    match &p.statements[0] {
        Statement::CreateGraph { path, body, .. } => {
            assert_eq!(path.segments, vec!["mygraph"]);
            assert!(matches!(body, Some(GraphTypeBody::Open { .. })));
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_with_typeref_body() {
    let p = parse_ok("CREATE GRAPH mygraph mygraphtype");
    match &p.statements[0] {
        Statement::CreateGraph { path, body, .. } => {
            assert_eq!(path.segments, vec!["mygraph"]);
            match body {
                Some(GraphTypeBody::Reference { name, .. }) => {
                    assert_eq!(name.segments, vec!["mygraphtype"]);
                }
                other => panic!("expected Reference, got {other:?}"),
            }
        }
        _ => unreachable!(),
    }
}

#[test]
fn create_graph_with_inline_body() {
    let p = parse_ok(
        "CREATE GRAPH mygraph {\n\
           (Person :Person {lastname STRING, firstname STRING, joined DATE})\n\
         }",
    );
    match &p.statements[0] {
        Statement::CreateGraph {
            body: Some(GraphTypeBody::Inline { .. }),
            ..
        } => {}
        other => panic!("expected CreateGraph with Inline body, got {other:?}"),
    }
}

#[test]
fn create_graph_like() {
    let p = parse_ok("CREATE GRAPH /mygraph LIKE /mysrcgraph");
    match &p.statements[0] {
        Statement::CreateGraph {
            body: Some(GraphTypeBody::Like { source, .. }),
            ..
        } => {
            assert!(source.absolute);
            assert_eq!(source.segments, vec!["mysrcgraph"]);
        }
        other => panic!("expected Like body, got {other:?}"),
    }
}

#[test]
fn create_graph_as_copy_of() {
    let p = parse_ok("CREATE GRAPH mygraph ANY AS COPY OF mysrcgraph");
    match &p.statements[0] {
        Statement::CreateGraph {
            body: Some(GraphTypeBody::Copy {
                type_part, source, ..
            }),
            ..
        } => {
            assert!(matches!(
                type_part.as_deref(),
                Some(GraphTypeBody::Open { .. })
            ));
            assert_eq!(source.segments, vec!["mysrcgraph"]);
        }
        other => panic!("expected Copy body, got {other:?}"),
    }
}

#[test]
fn create_graph_type_inline_body() {
    let p = parse_ok(
        "CREATE GRAPH TYPE mygraphtype {\n\
           (Person :Person {lastname STRING, firstname STRING, joined DATE})\n\
         }",
    );
    match &p.statements[0] {
        Statement::CreateGraphType { path, body, .. } => {
            assert_eq!(path.segments, vec!["mygraphtype"]);
            assert!(matches!(body, GraphTypeBody::Inline { .. }));
        }
        _ => unreachable!(),
    }
}

#[test]
fn create_graph_type_with_path() {
    let p = parse_ok(
        "CREATE GRAPH TYPE /foo/mygraphtype {\n\
           (Person :Person {lastname STRING})\n\
         }",
    );
    match &p.statements[0] {
        Statement::CreateGraphType { path, .. } => {
            assert!(path.absolute);
            assert_eq!(path.segments, vec!["foo", "mygraphtype"]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn drop_graph_path() {
    let p = parse_ok("DROP GRAPH /foo/mygraph");
    match &p.statements[0] {
        Statement::DropGraph {
            path, if_exists, ..
        } => {
            assert!(path.absolute);
            assert_eq!(path.segments, vec!["foo", "mygraph"]);
            assert!(!if_exists);
        }
        _ => unreachable!(),
    }
}

#[test]
fn drop_graph_type_if_exists() {
    let p = parse_ok("DROP GRAPH TYPE IF EXISTS /foo/mygraphtype");
    match &p.statements[0] {
        Statement::DropGraphType {
            path, if_exists, ..
        } => {
            assert!(*if_exists);
            assert_eq!(path.segments, vec!["foo", "mygraphtype"]);
        }
        _ => unreachable!(),
    }
}

#[test]
fn next_separates_statements() {
    let p = parse_ok("CREATE SCHEMA /foo NEXT CREATE SCHEMA /fee");
    assert_eq!(p.statements.len(), 2);
    assert!(matches!(p.statements[0], Statement::CreateSchema { .. }));
    assert!(matches!(p.statements[1], Statement::CreateSchema { .. }));
}

#[test]
fn next_and_semicolon_are_interchangeable() {
    // Mixed separators in one program - neither has more force
    // than the other.
    let p = parse_ok("CREATE SCHEMA /a; CREATE SCHEMA /b NEXT CREATE SCHEMA /c");
    assert_eq!(p.statements.len(), 3);
}

#[test]
fn create_graph_headless_any_as_copy_of() {
    let p = parse_ok("CREATE GRAPH ANY AS COPY OF mysrcgraph");
    match &p.statements[0] {
        Statement::CreateGraph {
            path,
            body,
            if_not_exists,
            ..
        } => {
            assert!(
                path.segments.is_empty(),
                "headless form has no name segments"
            );
            assert!(!path.absolute);
            assert!(!if_not_exists);
            match body {
                Some(GraphTypeBody::Copy {
                    type_part, source, ..
                }) => {
                    assert!(matches!(
                        type_part.as_deref(),
                        Some(GraphTypeBody::Open { .. })
                    ));
                    assert_eq!(source.segments, vec!["mysrcgraph"]);
                }
                other => panic!("expected Copy body, got {other:?}"),
            }
        }
        other => panic!("expected CreateGraph, got {other:?}"),
    }
}

#[test]
fn create_graph_headless_inline_body() {
    // `{...}` immediately after CREATE GRAPH also triggers the
    // headless form.
    let p = parse_ok("CREATE GRAPH { (a :A) }");
    match &p.statements[0] {
        Statement::CreateGraph {
            path,
            body: Some(GraphTypeBody::Inline { .. }),
            ..
        } => {
            assert!(path.segments.is_empty());
        }
        other => panic!("expected headless inline, got {other:?}"),
    }
}

#[test]
fn inline_body_balances_nested_braces() {
    // The body skipper must count `{` / `}` so nested maps don't
    // close the outer body prematurely.
    let p = parse_ok(
        "CREATE GRAPH TYPE outer {\n\
           (A :A {nested {inside}})\n\
         }",
    );
    match &p.statements[0] {
        Statement::CreateGraphType {
            body: GraphTypeBody::Inline { body_span, .. },
            ..
        } => {
            // The body span should be non-empty.
            assert!(body_span.end > body_span.start);
        }
        _ => unreachable!(),
    }
}
