use super::*;
use crate::ast::*;

#[test]
fn parse_simple_match_return() {
    let stmt = parse("MATCH (n) RETURN n").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            assert!(rq.unions.is_empty());
            let sq = &rq.single;
            assert!(sq.parts.is_empty());
            assert_eq!(sq.final_part.clauses.len(), 1);
            assert!(sq.final_part.return_clause.is_some());
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_match_with_label() {
    let stmt = parse("MATCH (n:Person) RETURN n.name").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[0];
            if let Clause::Match(mc) = clause {
                let node = &mc.pattern.parts[0].element.node;
                assert_eq!(node.variable.as_deref(), Some("n"));
                assert_eq!(node.labels, vec!["Person"]);
            } else {
                panic!("expected Match clause");
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_pattern_with_rel() {
    let stmt = parse("MATCH (a)-[:KNOWS]->(b) RETURN a, b").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[0];
            if let Clause::Match(mc) = clause {
                let elem = &mc.pattern.parts[0].element;
                assert_eq!(elem.chain.len(), 1);
                assert_eq!(elem.chain[0].rel.types, vec!["KNOWS"]);
                assert_eq!(elem.chain[0].rel.direction, RelDirection::Outgoing);
            } else {
                panic!("expected Match clause");
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_create() {
    let stmt = parse("CREATE (n:Person {name: 'Alice'})").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[0];
            assert!(matches!(clause, Clause::Create(_)));
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_where_clause() {
    let stmt = parse("MATCH (n) WHERE n.age > 30 RETURN n").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[0];
            if let Clause::Match(mc) = clause {
                assert!(mc.where_clause.is_some());
            } else {
                panic!("expected Match clause");
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_order_by_limit() {
    let stmt = parse("MATCH (n) RETURN n ORDER BY n.age DESC LIMIT 10").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let ret = rq.single.final_part.return_clause.as_ref().unwrap();
            assert!(ret.order_by.is_some());
            let sort = &ret.order_by.as_ref().unwrap()[0];
            assert!(!sort.ascending);
            assert!(ret.limit.is_some());
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_with_clause() {
    let stmt = parse("MATCH (n) WITH n.type AS t, count(*) AS c RETURN t, c").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            assert_eq!(rq.single.parts.len(), 1);
            assert!(rq.single.final_part.return_clause.is_some());
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_union() {
    let stmt = parse("MATCH (n:A) RETURN n UNION ALL MATCH (m:B) RETURN m").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            assert_eq!(rq.unions.len(), 1);
            assert!(rq.unions[0].all);
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_set_and_delete() {
    let stmt = parse("MATCH (n) SET n.name = 'Bob' DELETE n").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clauses = &rq.single.final_part.clauses;
            assert!(matches!(&clauses[0], Clause::Match(_)));
            assert!(matches!(&clauses[1], Clause::Set(_)));
            assert!(matches!(&clauses[2], Clause::Delete(_)));
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_detach_delete() {
    let stmt = parse("MATCH (n:Temp) DETACH DELETE n").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[1];
            if let Clause::Delete(dc) = clause {
                assert!(dc.detach);
            } else {
                panic!("expected Delete clause");
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_variable_length_path() {
    let stmt = parse("MATCH (a)-[*1..3]->(b) RETURN a, b").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[0];
            if let Clause::Match(mc) = clause {
                let range = mc.pattern.parts[0].element.chain[0].rel.range;
                assert_eq!(range, Some((Some(1), Some(3))));
            } else {
                panic!("expected Match clause");
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_case_expr() {
    let stmt = parse("RETURN CASE WHEN 1 = 1 THEN 'yes' ELSE 'no' END").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let ret = rq.single.final_part.return_clause.as_ref().unwrap();
            if let ProjectionItems::Explicit(items) = &ret.items {
                assert!(matches!(&items[0].expr, Expr::Case { .. }));
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_unwind() {
    let stmt = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            assert!(matches!(
                &rq.single.final_part.clauses[0],
                Clause::Unwind(_)
            ));
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_list_comprehension() {
    let stmt = parse("RETURN [x IN [1, 2, 3] WHERE x > 1 | x * 2]").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let ret = rq.single.final_part.return_clause.as_ref().unwrap();
            if let ProjectionItems::Explicit(items) = &ret.items {
                assert!(matches!(&items[0].expr, Expr::ListComprehension { .. }));
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_merge() {
    let stmt = parse("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true").unwrap();
    match &stmt.query {
        Query::Regular(rq) => {
            let clause = &rq.single.final_part.clauses[0];
            if let Clause::Merge(mc) = clause {
                assert!(mc.on_create.is_some());
                assert!(mc.on_match.is_none());
            } else {
                panic!("expected Merge clause");
            }
        }
        _ => panic!("expected Regular query"),
    }
}

#[test]
fn parse_create_index_memgraph_syntax() {
    let stmt = parse("CREATE INDEX ON :User(id)").unwrap();
    match stmt.query {
        Query::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            assert_eq!(name, "idx_User_id");
            assert_eq!(label, "User");
            assert_eq!(property, "id");
        }
        _ => panic!("expected CreateIndex"),
    }
}

#[test]
fn parse_create_index_neo4j_syntax() {
    let stmt = parse("CREATE INDEX my_idx IF NOT EXISTS FOR (n:Person) ON (n.name)").unwrap();
    match stmt.query {
        Query::CreateIndex {
            name,
            label,
            property,
            ..
        } => {
            assert_eq!(name, "my_idx");
            assert_eq!(label, "Person");
            assert_eq!(property, "name");
        }
        _ => panic!("expected CreateIndex"),
    }
}
