use super::*;
use crate::parse;

fn lowered(input: &str) -> cy::Statement {
    let p = parse(input).unwrap_or_else(|e| panic!("parse failed: {e:?}"));
    lower(&p).unwrap_or_else(|e| panic!("lower failed: {e:?}"))
}

fn lower_err(input: &str) -> Vec<ParseError> {
    let p = parse(input).unwrap_or_else(|e| panic!("parse failed: {e:?}"));
    lower(&p).expect_err("expected lowering to fail")
}

fn regular(stmt: &cy::Statement) -> &cy::RegularQuery {
    match &stmt.query {
        cy::Query::Regular(r) => r,
        other => panic!("expected RegularQuery, got {other:?}"),
    }
}

// -- smallest end-to-end cases -------------------------------

#[test]
fn lowers_match_return() {
    let s = lowered("MATCH (n:Person) RETURN n");
    let r = regular(&s);
    assert!(r.unions.is_empty());
    assert_eq!(r.single.parts.len(), 0);
    let final_clauses = &r.single.final_part.clauses;
    assert_eq!(final_clauses.len(), 1);
    match &final_clauses[0] {
        cy::Clause::Match(m) => {
            assert_eq!(m.pattern.parts.len(), 1);
            let node = &m.pattern.parts[0].element.node;
            assert_eq!(node.labels, vec!["Person"]);
            assert_eq!(node.variable.as_deref(), Some("n"));
        }
        other => panic!("expected MATCH, got {other:?}"),
    }
    let ret = r.single.final_part.return_clause.as_ref().expect("RETURN");
    match &ret.items {
        cy::ProjectionItems::Explicit(items) => {
            assert_eq!(items.len(), 1);
            assert!(matches!(items[0].expr, cy::Expr::Variable(ref n) if n == "n"));
        }
        other => panic!("expected Explicit, got {other:?}"),
    }
}

#[test]
fn lowers_match_where_predicate() {
    let s = lowered("MATCH (n) WHERE n.age > 30 RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert!(matches!(m.where_clause, Some(cy::Expr::Comparison(..))));
}

#[test]
fn lowers_optional_match() {
    let s = lowered("OPTIONAL MATCH (n) RETURN n");
    let r = regular(&s);
    assert!(matches!(
        r.single.final_part.clauses[0],
        cy::Clause::OptionalMatch(_)
    ));
}

// -- expressions --------------------------------------------

fn return_expr(input: &str) -> cy::Expr {
    let s = lowered(input);
    let r = regular(&s);
    let ret = r.single.final_part.return_clause.as_ref().unwrap();
    match &ret.items {
        cy::ProjectionItems::Explicit(items) => items[0].expr.clone(),
        _ => unreachable!(),
    }
}

#[test]
fn lowers_integer_literal() {
    assert!(matches!(
        return_expr("MATCH (n) RETURN 42"),
        cy::Expr::Literal(cy::Literal::Integer(42))
    ));
}

#[test]
fn lowers_float_literal() {
    assert!(matches!(
        return_expr("MATCH (n) RETURN 3.14"),
        cy::Expr::Literal(cy::Literal::Float(_))
    ));
}

#[test]
fn lowers_string_literal() {
    assert!(matches!(
        return_expr("MATCH (n) RETURN 'hi'"),
        cy::Expr::Literal(cy::Literal::String(ref s)) if s == "hi"
    ));
}

#[test]
fn lowers_bool_and_null() {
    let s = lowered("MATCH (n) RETURN TRUE, FALSE, NULL");
    let r = regular(&s);
    let items = match &r.single.final_part.return_clause.as_ref().unwrap().items {
        cy::ProjectionItems::Explicit(items) => items,
        _ => unreachable!(),
    };
    assert!(matches!(
        items[0].expr,
        cy::Expr::Literal(cy::Literal::Bool(true))
    ));
    assert!(matches!(
        items[1].expr,
        cy::Expr::Literal(cy::Literal::Bool(false))
    ));
    assert!(matches!(
        items[2].expr,
        cy::Expr::Literal(cy::Literal::Null)
    ));
}

#[test]
fn lowers_variable_and_property() {
    let e = return_expr("MATCH (n) RETURN n.name");
    match e {
        cy::Expr::Property(base, key) => {
            assert_eq!(key, "name");
            assert!(matches!(*base, cy::Expr::Variable(ref s) if s == "n"));
        }
        other => panic!("expected Property, got {other:?}"),
    }
}

#[test]
fn lowers_parameter() {
    let s = lowered("MATCH (n) WHERE n.age > $minAge RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    // a comparison with the parameter on the rhs
    if let Some(cy::Expr::Comparison(_, rest)) = &m.where_clause {
        assert!(matches!(rest[0].1, cy::Expr::Parameter(ref n) if n == "minAge"));
    } else {
        panic!("expected Comparison");
    }
}

#[test]
fn lowers_arithmetic() {
    let e = return_expr("MATCH (n) RETURN 1 + 2");
    assert!(matches!(e, cy::Expr::BinaryOp(_, cy::BinOp::Add, _)));
}

#[test]
fn lowers_comparison_via_comparison_form() {
    let e = return_expr("MATCH (n) RETURN n.x = 5");
    // Comparison form for single and chained alike
    assert!(matches!(e, cy::Expr::Comparison(..)));
}

#[test]
fn lowers_logical_and() {
    let s = lowered("MATCH (n) WHERE n.a > 1 AND n.b < 2 RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert!(matches!(
        m.where_clause,
        Some(cy::Expr::BinaryOp(_, cy::BinOp::And, _))
    ));
}

#[test]
fn lowers_unary_neg_and_not() {
    let e = return_expr("MATCH (n) RETURN - 5");
    assert!(matches!(e, cy::Expr::UnaryOp(cy::UnaryOp::Neg, _)));
    let s = lowered("MATCH (n) WHERE NOT n.flag RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert!(matches!(
        m.where_clause,
        Some(cy::Expr::UnaryOp(cy::UnaryOp::Not, _))
    ));
}

#[test]
fn lowers_function_call_lowercased() {
    let e = return_expr("MATCH (n) RETURN COUNT(n)");
    if let cy::Expr::FunctionCall {
        name,
        distinct,
        args,
    } = e
    {
        assert_eq!(name, "count"); // lowercased
        assert!(!distinct);
        assert_eq!(args.len(), 1);
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn lowers_function_call_distinct() {
    let e = return_expr("MATCH (n) RETURN count(DISTINCT n)");
    if let cy::Expr::FunctionCall { distinct, .. } = e {
        assert!(distinct);
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn lowers_string_concat_to_dedicated_binop() {
    // GQL section 20.23 `||` lowers to cy::BinOp::Concat, distinct from BinOp::Add (Cypher's numeric
    // add and string/list concat); strict typing happens in the executor
    let e = return_expr("MATCH (n) RETURN n.first || ' ' || n.last");
    // left-associative: the outermost op is the second `||`
    if let cy::Expr::BinaryOp(_, op, _) = e {
        assert_eq!(op, cy::BinOp::Concat);
    } else {
        panic!("expected BinaryOp at top level");
    }
}

#[test]
fn lowers_trim_to_gql_strict_variant() {
    // GQL section 20.24 TRIM removes only ASCII space, Cypher's trim() any whitespace, so `trim`
    // routes to `gql_trim`
    let e = return_expr("MATCH (n) RETURN trim(n.name)");
    if let cy::Expr::FunctionCall { name, .. } = e {
        assert_eq!(name, "gql_trim");
    } else {
        panic!("expected FunctionCall");
    }
}

#[test]
fn lowers_non_divergent_function_names_verbatim() {
    // aliases with Cypher-identical semantics (upper / lower / ceiling / ln / local_time /
    // zoned_datetime / ...) pass through; the function table takes both spellings
    for spelling in [
        "upper",
        "lower",
        "ceiling",
        "ln",
        "char_length",
        "character_length",
        "local_time",
        "zoned_datetime",
    ] {
        let e = return_expr(&format!("MATCH (n) RETURN {spelling}(n.name)"));
        if let cy::Expr::FunctionCall { name, .. } = e {
            assert_eq!(name, spelling, "{spelling} should lower verbatim");
        } else {
            panic!("expected FunctionCall for {spelling}");
        }
    }
}

#[test]
fn lowers_list_literal_and_index() {
    let e1 = return_expr("MATCH (n) RETURN [1, 2, 3]");
    assert!(matches!(e1, cy::Expr::ListLiteral(_)));
    let e2 = return_expr("MATCH (n) RETURN xs[0]");
    assert!(matches!(e2, cy::Expr::ListIndex(..)));
}

#[test]
fn lowers_paren_flatten() {
    // Cypher drops parens: `(1+2)*3` becomes `BinaryOp(BinaryOp(...), Mul, 3)`
    let e = return_expr("MATCH (n) RETURN (1 + 2) * 3");
    if let cy::Expr::BinaryOp(lhs, cy::BinOp::Mul, _) = e {
        assert!(matches!(*lhs, cy::Expr::BinaryOp(_, cy::BinOp::Add, _)));
    } else {
        panic!("expected Mul at top");
    }
}

// -- patterns -----------------------------------------------

#[test]
fn lowers_outgoing_edge() {
    let s = lowered("MATCH (a)-[r:KNOWS]->(b) RETURN r");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    let chain = &m.pattern.parts[0].element.chain;
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].rel.types, vec!["KNOWS"]);
    assert_eq!(chain[0].rel.direction, cy::RelDirection::Outgoing);
}

#[test]
fn lowers_undirected_edge() {
    let s = lowered("MATCH (a)-[]-(b) RETURN a");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert_eq!(
        m.pattern.parts[0].element.chain[0].rel.direction,
        cy::RelDirection::Both
    );
}

#[test]
fn lowers_edge_type_disjunction() {
    let s = lowered("MATCH (a)-[:KNOWS|LIKES]->(b) RETURN a");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert_eq!(
        m.pattern.parts[0].element.chain[0].rel.types,
        vec!["KNOWS", "LIKES"]
    );
}

#[test]
fn lowers_edge_quantifier() {
    let s = lowered("MATCH (a)-[*1..3]->(b) RETURN a");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    let rel = &m.pattern.parts[0].element.chain[0].rel;
    assert_eq!(rel.range, Some((Some(1), Some(3))));
}

#[test]
fn lowers_label_conjunction() {
    let s = lowered("MATCH (n:Movie&Award) RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert_eq!(
        m.pattern.parts[0].element.node.labels,
        vec!["Movie", "Award"]
    );
}

#[test]
fn label_disjunction_on_node_is_not_implemented() {
    let errs = lower_err("MATCH (n:A|B) RETURN n");
    assert!(errs[0].kind == ParseErrorKind::NotImplemented);
    assert!(errs[0].message.contains("disjunction"));
}

#[test]
fn label_wildcard_is_not_implemented() {
    let errs = lower_err("MATCH (n:%) RETURN n");
    assert!(errs[0].message.contains("wildcard"));
}

#[test]
fn lowers_property_map_to_map_literal() {
    let s = lowered("MATCH (n {name: 'Alice'}) RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    let props = m.pattern.parts[0].element.node.properties.clone().unwrap();
    if let cy::Expr::MapLiteral(entries) = props {
        assert_eq!(entries[0].0, "name");
    } else {
        panic!("expected MapLiteral");
    }
}

// -- DML clauses --------------------------------------------

#[test]
fn lowers_insert_to_create() {
    let s = lowered("INSERT (n:Person {name: 'Bob'})");
    let r = regular(&s);
    assert!(matches!(
        r.single.final_part.clauses[0],
        cy::Clause::Create(_)
    ));
}

#[test]
fn lowers_set_property() {
    let s = lowered("MATCH (n) SET n.age = 30 RETURN n");
    let r = regular(&s);
    let set = match &r.single.final_part.clauses[1] {
        cy::Clause::Set(s) => s,
        _ => unreachable!(),
    };
    assert!(matches!(set.items[0], cy::SetItem::Property(_, _)));
}

#[test]
fn lowers_set_labels() {
    let s = lowered("MATCH (n) SET n:Visited RETURN n");
    let r = regular(&s);
    let set = match &r.single.final_part.clauses[1] {
        cy::Clause::Set(s) => s,
        _ => unreachable!(),
    };
    assert!(matches!(set.items[0], cy::SetItem::Label(_, _, _)));
}

#[test]
fn lowers_remove_property() {
    let s = lowered("MATCH (n) REMOVE n.temp RETURN n");
    let r = regular(&s);
    let rm = match &r.single.final_part.clauses[1] {
        cy::Clause::Remove(r) => r,
        _ => unreachable!(),
    };
    assert!(matches!(rm.items[0], cy::RemoveItem::Property(_)));
}

#[test]
fn lowers_detach_delete() {
    let s = lowered("MATCH (n) DETACH DELETE n");
    let r = regular(&s);
    let del = match &r.single.final_part.clauses[1] {
        cy::Clause::Delete(d) => d,
        _ => unreachable!(),
    };
    assert!(del.detach);
    assert_eq!(del.exprs.len(), 1);
}

#[test]
fn lowers_unwind() {
    let s = lowered("UNWIND [1,2,3] AS x RETURN x");
    let r = regular(&s);
    let u = match &r.single.final_part.clauses[0] {
        cy::Clause::Unwind(u) => u,
        _ => unreachable!(),
    };
    assert_eq!(u.alias, "x");
}

// -- WITH splits the query ---------------------------------

#[test]
fn with_splits_into_query_part() {
    let s = lowered("MATCH (n) WITH n.age AS a WHERE a > 18 RETURN a");
    let r = regular(&s);
    assert_eq!(r.single.parts.len(), 1);
    assert!(matches!(r.single.parts[0].clauses[0], cy::Clause::Match(_)));
    assert!(r.single.parts[0].with.where_clause.is_some());
    assert!(r.single.final_part.return_clause.is_some());
}

#[test]
fn chained_with() {
    let s = lowered("MATCH (n) WITH n.dept AS d, n WITH d, count(n) AS c WHERE c > 0 RETURN d, c");
    let r = regular(&s);
    assert_eq!(r.single.parts.len(), 2);
}

#[test]
fn finish_terminates_with_no_return() {
    let s = lowered("MATCH (n) SET n.touched = true FINISH");
    let r = regular(&s);
    assert!(r.single.final_part.return_clause.is_none());
}

// -- RETURN modifiers --------------------------------------

#[test]
fn return_distinct_and_alias() {
    let s = lowered("MATCH (n) RETURN DISTINCT n.label AS lbl");
    let r = regular(&s);
    let ret = r.single.final_part.return_clause.as_ref().unwrap();
    assert!(ret.distinct);
    if let cy::ProjectionItems::Explicit(items) = &ret.items {
        assert_eq!(items[0].alias.as_deref(), Some("lbl"));
    }
}

#[test]
fn return_order_skip_limit() {
    let s = lowered("MATCH (n) RETURN n ORDER BY n.name DESC SKIP 5 LIMIT 10");
    let r = regular(&s);
    let ret = r.single.final_part.return_clause.as_ref().unwrap();
    let order = ret.order_by.as_ref().unwrap();
    assert!(!order[0].ascending);
    assert!(matches!(
        ret.skip,
        Some(cy::Expr::Literal(cy::Literal::Integer(5)))
    ));
    assert!(matches!(
        ret.limit,
        Some(cy::Expr::Literal(cy::Literal::Integer(10)))
    ));
}

// -- catalog -----------------------------------------------

#[test]
fn lowers_create_index() {
    let s = lowered("CREATE INDEX person_name FOR (n:Person) ON (n.name)");
    match s.query {
        cy::Query::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            assert_eq!(name, "person_name");
            assert_eq!(label, "Person");
            assert_eq!(property, "name");
        }
        _ => panic!("expected CreateIndex"),
    }
}

#[test]
fn lowers_create_index_anon_synthesises_name() {
    let s = lowered("CREATE INDEX FOR (n:Movie) ON (n.title)");
    match s.query {
        cy::Query::CreateIndex { name, .. } => {
            assert_eq!(name, "idx_Movie_title");
        }
        _ => panic!("expected CreateIndex"),
    }
}

#[test]
fn lowers_drop_index() {
    let s = lowered("DROP INDEX foo");
    match s.query {
        cy::Query::DropIndex { name } => assert_eq!(name, "foo"),
        _ => panic!("expected DropIndex"),
    }
}

#[test]
fn lowers_create_constraint_unique() {
    let s = lowered("CREATE CONSTRAINT email_uniq FOR (u:User) REQUIRE u.email IS UNIQUE");
    match s.query {
        cy::Query::CreateConstraint {
            name,
            label,
            property,
            constraint_type,
            ..
        } => {
            assert_eq!(name, "email_uniq");
            assert_eq!(label, "User");
            assert_eq!(property, "email");
            assert_eq!(constraint_type, "UNIQUE");
        }
        _ => panic!("expected CreateConstraint"),
    }
}

#[test]
fn lowers_create_constraint_not_null() {
    let s = lowered("CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL");
    match s.query {
        cy::Query::CreateConstraint {
            constraint_type, ..
        } => {
            assert_eq!(constraint_type, "NOT NULL");
        }
        _ => panic!("expected CreateConstraint"),
    }
}

#[test]
fn create_graph_is_not_implemented() {
    let errs = lower_err("CREATE GRAPH social");
    assert_eq!(errs[0].kind, ParseErrorKind::NotImplemented);
}

#[test]
fn use_is_not_implemented() {
    let errs = lower_err("USE social");
    assert_eq!(errs[0].kind, ParseErrorKind::NotImplemented);
}

// -- UNION lowering -----------------------------------------

#[test]
fn lowers_union_distinct() {
    let s = lowered("MATCH (a) RETURN a UNION MATCH (b) RETURN b");
    let r = regular(&s);
    assert_eq!(r.unions.len(), 1);
    // bare UNION is DISTINCT: `all` false
    assert!(!r.unions[0].all);
    // both sides have their own RETURN
    assert!(r.single.final_part.return_clause.is_some());
    assert!(r.unions[0].query.final_part.return_clause.is_some());
}

#[test]
fn lowers_union_all() {
    let s = lowered("MATCH (a) RETURN a UNION ALL MATCH (b) RETURN b");
    let r = regular(&s);
    assert!(r.unions[0].all);
}

#[test]
fn lowers_chained_unions() {
    let s = lowered(
        "MATCH (a) RETURN a \
         UNION MATCH (b) RETURN b \
         UNION ALL MATCH (c) RETURN c",
    );
    let r = regular(&s);
    assert_eq!(r.unions.len(), 2);
    assert!(!r.unions[0].all); // DISTINCT
    assert!(r.unions[1].all); // ALL
}

// -- CALL lowering ------------------------------------------

fn first_call(s: &cy::Statement) -> &cy::CallClause {
    let r = regular(s);
    for c in &r.single.final_part.clauses {
        if let cy::Clause::Call(c) = c {
            return c;
        }
    }
    panic!("no CALL clause in {:?}", r.single.final_part.clauses);
}

#[test]
fn lowers_call_no_args_no_yield() {
    let s = lowered("CALL db.labels()");
    let c = first_call(&s);
    assert_eq!(c.procedure_name, "db.labels");
    // empty parens -> Some(vec![]), not None (None = implicit args from query parameters, never
    // produced by GQL)
    assert_eq!(c.args.as_ref().map(Vec::len), Some(0));
    assert!(c.yield_items.is_none());
}

#[test]
fn lowers_call_namespaced_name() {
    let s = lowered("CALL traverse.graph.labels()");
    let c = first_call(&s);
    assert_eq!(c.procedure_name, "traverse.graph.labels");
}

#[test]
fn lowers_call_args() {
    let s = lowered("CALL traverse.graph.subgraph('movies', 100)");
    let c = first_call(&s);
    let args = c.args.as_ref().unwrap();
    assert_eq!(args.len(), 2);
    assert!(matches!(args[0], cy::Expr::Literal(cy::Literal::String(_))));
    assert!(matches!(
        args[1],
        cy::Expr::Literal(cy::Literal::Integer(100))
    ));
}

#[test]
fn lowers_call_yield_explicit() {
    let s = lowered("CALL db.indexes() YIELD name, label");
    let c = first_call(&s);
    match c.yield_items.as_ref().unwrap() {
        cy::YieldItems::Explicit(items) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].name, "name");
            assert!(items[0].alias.is_none());
            assert_eq!(items[1].name, "label");
        }
        other => panic!("expected Explicit, got {other:?}"),
    }
}

#[test]
fn lowers_call_yield_with_alias() {
    let s = lowered("CALL db.indexes() YIELD name AS idxName");
    let c = first_call(&s);
    match c.yield_items.as_ref().unwrap() {
        cy::YieldItems::Explicit(items) => {
            assert_eq!(items[0].alias.as_deref(), Some("idxName"));
        }
        _ => unreachable!(),
    }
}

#[test]
fn lowers_match_call_return_pipeline() {
    let s = lowered("MATCH (n:Person) CALL stats.compute(n) YIELD score RETURN n, score");
    let r = regular(&s);
    let clauses = &r.single.final_part.clauses;
    assert!(matches!(clauses[0], cy::Clause::Match(_)));
    assert!(matches!(clauses[1], cy::Clause::Call(_)));
    assert!(r.single.final_part.return_clause.is_some());
}

// -- FOR / FILTER / IS NULL lowering ------------------------

#[test]
fn for_lowers_to_unwind() {
    let s = lowered("FOR x IN [1, 2, 3] RETURN x");
    let r = regular(&s);
    match &r.single.final_part.clauses[0] {
        cy::Clause::Unwind(u) => assert_eq!(u.alias, "x"),
        other => panic!("expected Unwind, got {other:?}"),
    }
}

#[test]
fn nested_for_lowers_to_two_unwinds() {
    let s = lowered("FOR a IN [1, 2] FOR b IN [3, 4] RETURN a, b");
    let r = regular(&s);
    let unwinds: Vec<_> = r
        .single
        .final_part
        .clauses
        .iter()
        .filter(|c| matches!(c, cy::Clause::Unwind(_)))
        .collect();
    assert_eq!(unwinds.len(), 2);
}

#[test]
fn filter_lowers_to_with_star_where() {
    let s = lowered("FOR x IN xs FILTER WHERE x > 0 RETURN x");
    let r = regular(&s);
    // FILTER becomes a part split whose `with` is `WITH * WHERE x > 0`
    assert_eq!(r.single.parts.len(), 1);
    let part = &r.single.parts[0];
    assert!(matches!(part.with.items, cy::ProjectionItems::All));
    assert!(part.with.where_clause.is_some());
    assert!(!part.with.distinct);
}

#[test]
fn filter_elided_where_lowers_the_same() {
    let s1 = lowered("FOR x IN xs FILTER WHERE x > 0 RETURN x");
    let s2 = lowered("FOR x IN xs FILTER x > 0 RETURN x");
    // both give one QueryPart with a WHERE-bearing WITH: the sugar collapses
    let p1 = regular(&s1);
    let p2 = regular(&s2);
    assert_eq!(p1.single.parts.len(), p2.single.parts.len());
    assert!(p1.single.parts[0].with.where_clause.is_some());
    assert!(p2.single.parts[0].with.where_clause.is_some());
}

#[test]
fn is_null_lowers_to_cypher_is_null() {
    let s = lowered("MATCH (n) WHERE n.name IS NULL RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert!(matches!(m.where_clause, Some(cy::Expr::IsNull(_))));
}

#[test]
fn is_not_null_lowers_to_cypher_is_not_null() {
    let s = lowered("MATCH (n) WHERE n.name IS NOT NULL RETURN n");
    let r = regular(&s);
    let m = match &r.single.final_part.clauses[0] {
        cy::Clause::Match(m) => m,
        _ => unreachable!(),
    };
    assert!(matches!(m.where_clause, Some(cy::Expr::IsNotNull(_))));
}
