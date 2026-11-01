//! APOC compatibility layer: curated apoc.* functions and procedures; these tests pin the embedded
//! behavior, which matches APOC core 2026.05.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn one(graph: &mut Graph, q: &str) -> CypherValue {
    ok(graph, q)[0][0].clone()
}

/// Filesystem path as a Cypher string literal payload: forward slashes, so a Windows temp path
/// (`C:\OSTemp\...`) isn't read as escapes inside `'...'`; Rust opens `C:/...` fine.
fn cy(p: &std::path::Path) -> String {
    p.display().to_string().replace('\\', "/")
}

#[test]
fn coll_functions() {
    let mut g = Graph::new();
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.sum([1, 2, 3])"),
        CypherValue::Float(6.0)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.avg([2, 4])"),
        CypherValue::Float(3.0)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.min([3, 1, 2])"),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.max([3, 1, 2])"),
        CypherValue::Integer(3)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.sort([3, 1, 2])"),
        CypherValue::List(
            vec![1, 2, 3]
                .into_iter()
                .map(CypherValue::Integer)
                .collect()
        )
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.contains([1, 2], 2)"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.toSet([1, 2, 1, 3, 2])"),
        CypherValue::List(
            vec![1, 2, 3]
                .into_iter()
                .map(CypherValue::Integer)
                .collect()
        )
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.flatten([[1, 2], [3]])"),
        CypherValue::List(
            vec![1, 2, 3]
                .into_iter()
                .map(CypherValue::Integer)
                .collect()
        )
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.pairsMin([1, 2, 3])"),
        CypherValue::List(vec![
            CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(2)]),
            CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(3)]),
        ])
    );
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.coll.intersection([1, 2, 3], [2, 3, 4])"
        ),
        CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(3)])
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.subtract([1, 2, 3], [2])"),
        CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(3)])
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.indexOf([5, 7, 9], 7)"),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.partition([1, 2, 3, 4, 5], 2)"),
        CypherValue::List(vec![
            CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(2)]),
            CypherValue::List(vec![CypherValue::Integer(3), CypherValue::Integer(4)]),
            CypherValue::List(vec![CypherValue::Integer(5)]),
        ])
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.duplicates([1, 2, 1, 3, 3])"),
        CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(3)])
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.coll.occurrences([1, 2, 1], 1)"),
        CypherValue::Integer(2)
    );
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.coll.dropDuplicateNeighbors([1, 1, 2, 2, 1])"
        ),
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Integer(2),
            CypherValue::Integer(1)
        ])
    );
    assert_eq!(one(&mut g, "RETURN apoc.coll.sum(null)"), CypherValue::Null);
}

#[test]
fn text_functions() {
    let mut g = Graph::new();
    assert_eq!(
        one(&mut g, "RETURN apoc.text.join(['a', 'b', 'c'], '-')"),
        CypherValue::String("a-b-c".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.clean('Hello, World!')"),
        CypherValue::String("helloworld".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.distance('kitten', 'sitting')"),
        CypherValue::Integer(3)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.hammingDistance('abc', 'axc')"),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.fuzzyMatch('hello', 'helo')"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.capitalize('hello')"),
        CypherValue::String("Hello".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.camelCase('FOO BAR')"),
        CypherValue::String("fooBar".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.upperCamelCase('foo bar')"),
        CypherValue::String("FooBar".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.snakeCase('fooBar baz')"),
        CypherValue::String("foo-bar-baz".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.lpad('7', 3, '0')"),
        CypherValue::String("007".into())
    );
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.text.regexGroups('abc123def456', '([a-z]+)(\\\\d+)')"
        ),
        CypherValue::List(vec![
            CypherValue::List(vec![
                CypherValue::String("abc123".into()),
                CypherValue::String("abc".into()),
                CypherValue::String("123".into()),
            ]),
            CypherValue::List(vec![
                CypherValue::String("def456".into()),
                CypherValue::String("def".into()),
                CypherValue::String("456".into()),
            ]),
        ])
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.replace('hello world', 'o', '0')"),
        CypherValue::String("hell0 w0rld".into())
    );
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.text.format('%s has %d items', ['cart', 3])"
        ),
        CypherValue::String("cart has 3 items".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.base64Encode('ab')"),
        CypherValue::String("YWI=".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.base64Decode('YWI=')"),
        CypherValue::String("ab".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.urlencode('a b&c')"),
        CypherValue::String("a+b%26c".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.text.slug('Hello, World!')"),
        CypherValue::String("Hello-World-".into())
    );
}

#[test]
fn map_functions() {
    let mut g = Graph::new();
    let m = one(&mut g, "RETURN apoc.map.fromPairs([['a', 1], ['b', 2]])");
    let CypherValue::Map(m) = m else { panic!() };
    assert_eq!(m.get("a"), Some(&CypherValue::Integer(1)));
    let merged = one(&mut g, "RETURN apoc.map.merge({a: 1}, {b: 2, a: 9})");
    let CypherValue::Map(mm) = merged else {
        panic!()
    };
    assert_eq!(mm.get("a"), Some(&CypherValue::Integer(9)));
    let cleaned = one(
        &mut g,
        "RETURN apoc.map.clean({a: 1, b: null, c: 2}, ['c'], [])",
    );
    let CypherValue::Map(cm) = cleaned else {
        panic!()
    };
    assert_eq!(cm.len(), 1);
    let flat = one(&mut g, "RETURN apoc.map.flatten({a: {b: 1}}, '.')");
    let CypherValue::Map(fm) = flat else { panic!() };
    assert_eq!(fm.get("a.b"), Some(&CypherValue::Integer(1)));
    let sub = one(&mut g, "RETURN apoc.map.submap({a: 1, b: 2}, ['a'])");
    let CypherValue::Map(sm) = sub else { panic!() };
    assert_eq!(sm.len(), 1);
}

#[test]
fn convert_date_number_util() {
    let mut g = Graph::new();
    assert_eq!(
        one(&mut g, "RETURN apoc.convert.toJson({a: [1, 'x']})"),
        CypherValue::String("{\"a\":[1,\"x\"]}".into())
    );
    let back = one(&mut g, "RETURN apoc.convert.fromJsonMap('{\"k\": 7}')");
    let CypherValue::Map(bm) = back else { panic!() };
    assert_eq!(bm.get("k"), Some(&CypherValue::Integer(7)));
    assert_eq!(
        one(&mut g, "RETURN apoc.date.format(86400, 's', 'yyyy-MM-dd')"),
        CypherValue::String("1970-01-02".into())
    );
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.date.parse('1970-01-02', 'd', 'yyyy-MM-dd')"
        ),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.date.toISO8601(0, 'ms')"),
        CypherValue::String("1970-01-01T00:00:00.000Z".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.number.parseInt('42')"),
        CypherValue::Integer(42)
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.number.format(1234567.891, '#,##0.00')"),
        CypherValue::String("1,234,567.89".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.util.md5(['abc'])"),
        CypherValue::String("900150983cd24fb0d6963f7d28e17f72".into())
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.util.sha256(['abc'])"),
        CypherValue::String(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()
        )
    );
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.util.validatePredicate(false, 'nope', [])"
        ),
        CypherValue::Bool(true)
    );
    let e = run(
        &mut g,
        "RETURN apoc.util.validatePredicate(true, 'boom', [])",
    )
    .unwrap_err();
    assert!(e.contains("boom"));
}

#[test]
fn graph_aware_functions() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:P {name: 'a'})-[:KNOWS]->(:P {name: 'b'}) CREATE (a)-[:LIKES]->(:P {name: 'c'})",
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (n:P {name: 'a'}) RETURN apoc.label.exists(n, 'P')"
        ),
        CypherValue::Bool(true)
    );
    assert_eq!(
        one(&mut g, "MATCH (n:P {name: 'a'}) RETURN apoc.node.degree(n)"),
        CypherValue::Integer(2)
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (n:P {name: 'a'}) RETURN apoc.node.degree(n, 'KNOWS>')"
        ),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (n:P {name: 'b'}) RETURN apoc.node.relationship.exists(n, '<KNOWS')"
        ),
        CypherValue::Bool(true)
    );
}

#[test]
fn create_and_merge_procedures() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "CALL apoc.create.node(['Person', 'Actor'], {name: 'Keanu'}) YIELD node RETURN node.name",
    );
    assert_eq!(rows[0][0], CypherValue::String("Keanu".into()));
    let check = ok(&mut g, "MATCH (n:Person:Actor) RETURN count(n)");
    assert_eq!(check[0][0], CypherValue::Integer(1));

    ok(
        &mut g,
        "CALL apoc.create.nodes(['T'], [{v: 1}, {v: 2}]) YIELD node RETURN node",
    );
    assert_eq!(
        one(&mut g, "MATCH (t:T) RETURN count(t)"),
        CypherValue::Integer(2)
    );

    ok(
        &mut g,
        "MATCH (a:Person), (t:T {v: 1}) \
         CALL apoc.create.relationship(a, 'DID', {y: 1999}, t) YIELD rel RETURN rel",
    );
    assert_eq!(
        one(&mut g, "MATCH (:Person)-[r:DID]->(:T) RETURN r.y"),
        CypherValue::Integer(1999)
    );

    // merge.node: first call creates (with onCreate props), second matches
    ok(
        &mut g,
        "CALL apoc.merge.node(['City'], {name: 'Malmo'}, {founded: 1275}, {}) YIELD node RETURN node",
    );
    ok(
        &mut g,
        "CALL apoc.merge.node(['City'], {name: 'Malmo'}, {founded: 9999}, {visited: true}) YIELD node RETURN node",
    );
    assert_eq!(
        one(&mut g, "MATCH (c:City) RETURN count(c)"),
        CypherValue::Integer(1),
        "merge matched, not duplicated"
    );
    assert_eq!(
        one(&mut g, "MATCH (c:City) RETURN c.founded"),
        CypherValue::Integer(1275)
    );
    assert_eq!(
        one(&mut g, "MATCH (c:City) RETURN c.visited"),
        CypherValue::Bool(true)
    );

    // merge.relationship is idempotent
    ok(
        &mut g,
        "MATCH (a:Person), (c:City) CALL apoc.merge.relationship(a, 'VISITED', {year: 2020}, {}, c) YIELD rel RETURN rel",
    );
    ok(
        &mut g,
        "MATCH (a:Person), (c:City) CALL apoc.merge.relationship(a, 'VISITED', {year: 2020}, {}, c) YIELD rel RETURN rel",
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (:Person)-[r:VISITED]->(:City) RETURN count(r)"
        ),
        CypherValue::Integer(1)
    );
}

#[test]
fn uuids_and_validate() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "CALL apoc.create.uuids(3) YIELD row, uuid RETURN row, uuid",
    );
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[2][1], CypherValue::String(s) if s.len() == 36));
    let e = run(
        &mut g,
        "CALL apoc.util.validate(true, 'custom failure', [])",
    )
    .unwrap_err();
    assert!(e.contains("custom failure"));
    ok(&mut g, "CALL apoc.util.validate(false, 'fine', [])");
}

#[test]
fn cypher_run_dynamic() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:D {v: 1}), (:D {v: 2})");
    let rows = ok(
        &mut g,
        "CALL apoc.cypher.run('MATCH (d:D) WHERE d.v > $min RETURN d.v AS v', {min: 1}) \
         YIELD value RETURN value.v",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

#[test]
fn periodic_iterate() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND range(1, 10) AS i CREATE (:Src {v: i})");
    let rows = ok(
        &mut g,
        "CALL apoc.periodic.iterate( \
           'MATCH (s:Src) RETURN s.v AS v', \
           'CREATE (:Dst {v: $v * 10})', \
           {batchSize: 3}) YIELD batches, total RETURN batches, total",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(4), "ceil(10/3)");
    assert_eq!(rows[0][1], CypherValue::Integer(10));
    assert_eq!(
        one(&mut g, "MATCH (d:Dst) RETURN count(d)"),
        CypherValue::Integer(10)
    );
    assert_eq!(
        one(&mut g, "MATCH (d:Dst {v: 100}) RETURN count(d)"),
        CypherValue::Integer(1)
    );
}

#[test]
fn unknown_apoc_names_clear_errors() {
    let mut g = Graph::new();
    let e = run(&mut g, "RETURN apoc.zz.nope(1)").unwrap_err();
    assert!(e.contains("compatibility subset"), "got: {e}");
    let e2 = run(&mut g, "CALL apoc.zz.nope()").unwrap_err();
    assert!(!e2.is_empty());
}

// -- aggregates, when, path expansion, meta, load.json --------------

#[test]
fn agg_functions() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND [3, 1, null, 2] AS x \
         RETURN apoc.agg.first(x) AS f, apoc.agg.last(x) AS l, \
                apoc.agg.median(x) AS med, apoc.agg.product(x) AS prod",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(3));
    assert_eq!(rows[0][1], CypherValue::Integer(2));
    assert_eq!(rows[0][2], CypherValue::Float(2.0));
    assert_eq!(rows[0][3], CypherValue::Integer(6));
    let st = one(
        &mut g,
        "UNWIND [1, 2, 3, 4] AS x RETURN apoc.agg.statistics(x)",
    );
    let CypherValue::Map(m) = st else { panic!() };
    assert_eq!(m.get("total"), Some(&CypherValue::Integer(4)));
    assert_eq!(m.get("mean"), Some(&CypherValue::Float(2.5)));
    assert_eq!(m.get("0.5"), Some(&CypherValue::Integer(2)));
    let p = one(
        &mut g,
        "UNWIND range(1, 10) AS x RETURN apoc.agg.percentiles(x, [0.5, 0.9])",
    );
    assert_eq!(
        p,
        CypherValue::List(vec![CypherValue::Integer(5), CypherValue::Integer(9)])
    );
    assert_eq!(
        one(&mut g, "UNWIND [] AS x RETURN apoc.agg.product(x)"),
        CypherValue::Float(0.0)
    );
}

#[test]
fn when_procedures() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "CALL apoc.when(true, 'RETURN 1 AS v', 'RETURN 2 AS v', {}) YIELD value RETURN value.v",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(1));
    let rows2 = ok(
        &mut g,
        "CALL apoc.when(false, 'RETURN 1 AS v', 'RETURN 2 AS v', {}) YIELD value RETURN value.v",
    );
    assert_eq!(rows2[0][0], CypherValue::Integer(2));
    // params flow into the branch query; do.when may write
    ok(
        &mut g,
        "CALL apoc.do.when(true, 'CREATE (n:W {v: $x}) RETURN n', '', {x: 7}) YIELD value RETURN value",
    );
    assert_eq!(
        one(&mut g, "MATCH (w:W) RETURN w.v"),
        CypherValue::Integer(7)
    );
}

#[test]
fn path_expand_family() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:PX {name: 'a'})-[:R1]->(b:PX {name: 'b'})-[:R2]->(c:PX {name: 'c'}), \
                (b)-[:R1]->(d:PY {name: 'd'})",
    );
    let rows = ok(
        &mut g,
        "MATCH (a:PX {name: 'a'}) \
         CALL apoc.path.expand(a, 'R1>|R2>', '+PX', 1, 2) YIELD path \
         RETURN [n IN nodes(path) | n.name] AS names ORDER BY names",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][0],
        CypherValue::List(vec![
            CypherValue::String("a".into()),
            CypherValue::String("b".into())
        ])
    );
    assert_eq!(
        rows[1][0],
        CypherValue::List(vec![
            CypherValue::String("a".into()),
            CypherValue::String("b".into()),
            CypherValue::String("c".into())
        ])
    );
    let sub = ok(
        &mut g,
        "MATCH (a:PX {name: 'a'}) \
         CALL apoc.path.subgraphNodes(a, {maxLevel: 2}) YIELD node \
         RETURN node.name AS n ORDER BY n",
    );
    let names: Vec<String> = sub
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            _ => panic!(),
        })
        .collect();
    assert_eq!(names, vec!["a", "b", "c", "d"]);
    let tree = ok(
        &mut g,
        "MATCH (a:PX {name: 'a'}) \
         CALL apoc.path.spanningTree(a, {maxLevel: 2}) YIELD path \
         RETURN length(path) AS l ORDER BY l",
    );
    assert_eq!(
        tree.len(),
        4,
        "one first-visit path per reachable node incl. start"
    );
}

#[test]
fn meta_stats() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:A {x: 1})-[:T]->(:B)");
    let rows = ok(
        &mut g,
        "CALL apoc.meta.stats() YIELD nodeCount, relCount, labelCount, labels \
         RETURN nodeCount, relCount, labelCount, labels",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(2));
    assert_eq!(rows[0][1], CypherValue::Integer(1));
    assert_eq!(rows[0][2], CypherValue::Integer(2));
    let CypherValue::Map(lm) = &rows[0][3] else {
        panic!()
    };
    assert_eq!(lm.get("A"), Some(&CypherValue::Integer(1)));
}

#[test]
fn load_json_local_files() {
    let dir = std::env::temp_dir();
    let obj = dir.join("apoc_lj_obj.json");
    let arr = dir.join("apoc_lj_arr.json");
    std::fs::write(&obj, r#"{"name": "solo", "n": 1}"#).unwrap();
    std::fs::write(&arr, r#"[{"v": 1}, {"v": 2}]"#).unwrap();
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.load.json('{}') YIELD value RETURN value.name",
            cy(&obj)
        ),
    );
    assert_eq!(rows[0][0], CypherValue::String("solo".into()));
    let rows2 = ok(
        &mut g,
        &format!(
            "CALL apoc.load.json('{}') YIELD value RETURN value.v ORDER BY value.v",
            cy(&arr)
        ),
    );
    assert_eq!(rows2.len(), 2);
    let e = run(&mut g, "CALL apoc.load.json('https://example.com/x.json')").unwrap_err();
    assert!(e.contains("local files only"), "got: {e}");
    std::fs::remove_file(obj).ok();
    std::fs::remove_file(arr).ok();
}

// -- refactor, schema.assert, periodic.commit, label helpers --------

#[test]
fn refactor_merge_nodes() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:MN {name: 'a', x: 1})-[:R {w: 1}]->(:MN {name: 'c'}), \
                (b:MN {name: 'b', x: 2, y: 9})-[:R {w: 2}]->(:MN {name: 'c2'}), \
                (:MN {name: 'z'})-[:S]->(b)",
    );
    ok(
        &mut g,
        "MATCH (a:MN {name: 'a'}), (b:MN {name: 'b'}) \
         CALL apoc.refactor.mergeNodes([a, b], {properties: 'combine'}) YIELD node RETURN node",
    );
    // 'combine': colliding scalars become lists; unique props kept
    let rows = ok(
        &mut g,
        "MATCH (n:MN) WHERE n.y = 9 RETURN n.x, n.y, size(n.name)",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(2)])
    );
    assert_eq!(rows[0][1], CypherValue::Integer(9));
    // relationships moved: merged node has R->c, R->c2, S<-z
    let deg = ok(
        &mut g,
        "MATCH (n:MN) WHERE n.y = 9 RETURN apoc.node.degree(n)",
    );
    assert_eq!(deg[0][0], CypherValue::Integer(3));
    assert_eq!(
        one(&mut g, "MATCH (n:MN) RETURN count(n)"),
        CypherValue::Integer(4),
        "loser deleted"
    );
}

#[test]
fn refactor_clone_and_edges() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:CN {name: 'a', k: 1})-[:R {w: 5}]->(:CN {name: 'b'})",
    );
    ok(
        &mut g,
        "MATCH (a:CN {name: 'a'}) CALL apoc.refactor.cloneNodes([a], true) YIELD output RETURN output",
    );
    assert_eq!(
        one(&mut g, "MATCH (n:CN {name: 'a'}) RETURN count(n)"),
        CypherValue::Integer(2)
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (:CN {name: 'a'})-[r:R]->(:CN {name: 'b'}) RETURN count(r)"
        ),
        CypherValue::Integer(2),
        "clone carries relationships"
    );
    // setType / invert / to
    let t = ok(
        &mut g,
        "MATCH (:CN {name: 'b'})<-[r:R]-() WITH r LIMIT 1 \
         CALL apoc.refactor.setType(r, 'NEW') YIELD output RETURN type(output)",
    );
    assert_eq!(t[0][0], CypherValue::String("NEW".into()));
    ok(
        &mut g,
        "CREATE (x:CN {name: 'x'})-[:INV]->(y:CN {name: 'y'})",
    );
    ok(
        &mut g,
        "MATCH (:CN {name: 'x'})-[r:INV]->() CALL apoc.refactor.invert(r) YIELD output RETURN output",
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (:CN {name: 'y'})-[r:INV]->(:CN {name: 'x'}) RETURN count(r)"
        ),
        CypherValue::Integer(1)
    );
}

#[test]
fn refactor_rename() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:RL1 {v: 1}), (:RL1 {v: 2}), (:Other)");
    let r = ok(
        &mut g,
        "CALL apoc.refactor.rename.label('RL1', 'RL2') YIELD total, committedOperations \
         RETURN total, committedOperations",
    );
    assert_eq!(r[0][0], CypherValue::Integer(2));
    assert_eq!(
        one(&mut g, "MATCH (n:RL2) RETURN count(n)"),
        CypherValue::Integer(2)
    );
    assert_eq!(
        one(&mut g, "MATCH (n:RL1) RETURN count(n)"),
        CypherValue::Integer(0)
    );
    ok(&mut g, "CREATE (:RT)-[:OLDT {w: 1}]->(:RT)");
    ok(
        &mut g,
        "CALL apoc.refactor.rename.type('OLDT', 'NEWT') YIELD total RETURN total",
    );
    assert_eq!(
        one(&mut g, "MATCH ()-[r:NEWT]->() RETURN r.w"),
        CypherValue::Integer(1),
        "properties preserved through type rename"
    );
}

#[test]
fn schema_assert() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE INDEX old_idx FOR (n:Old) ON (n.p)");
    let rows = ok(
        &mut g,
        "CALL apoc.schema.assert({SA: ['p1'], SC: [['c1', 'c2']]}, {SB: ['q1']}, true) \
         YIELD label, key, unique AS u, action RETURN label, key, u, action ORDER BY label",
    );
    // old dropped, SA, SB, SC created
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0], CypherValue::String("Old".into()));
    assert_eq!(rows[0][3], CypherValue::String("DROPPED".into()));
    assert_eq!(rows[1][0], CypherValue::String("SA".into()));
    assert_eq!(rows[1][3], CypherValue::String("CREATED".into()));
    assert_eq!(rows[2][0], CypherValue::String("SB".into()));
    assert_eq!(rows[2][2], CypherValue::Bool(true));
    // composite index SC created and visible
    let shown = ok(
        &mut g,
        "SHOW INDEXES YIELD name, properties WHERE name = 'index_SC_c1_c2' RETURN properties",
    );
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0][0], CypherValue::String("c1,c2".into()));
    // re-asserting the same spec -> KEPT
    let again = ok(
        &mut g,
        "CALL apoc.schema.assert({SA: ['p1'], SC: [['c1', 'c2']]}, {SB: ['q1']}, true) \
         YIELD action RETURN DISTINCT action",
    );
    assert_eq!(again.len(), 1);
    assert_eq!(again[0][0], CypherValue::String("KEPT".into()));
}

#[test]
fn periodic_commit_loops_until_zero() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND range(1, 5) AS i CREATE (:PC {v: i})");
    let rows = ok(
        &mut g,
        "CALL apoc.periodic.commit( \
           'MATCH (n:PC) WITH n LIMIT $limit DETACH DELETE n RETURN count(*)', \
           {limit: 2}) YIELD updates, executions RETURN updates, executions",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(5));
    assert_eq!(rows[0][1], CypherValue::Integer(3), "2 + 2 + 1");
    assert_eq!(
        one(&mut g, "MATCH (n:PC) RETURN count(n)"),
        CypherValue::Integer(0)
    );
}

#[test]
fn label_helpers() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:L1 {v: 1})");
    ok(
        &mut g,
        "MATCH (n:L1) CALL apoc.create.addLabels([n], ['L2', 'L3']) YIELD node RETURN node",
    );
    assert_eq!(
        one(&mut g, "MATCH (n:L1:L2:L3) RETURN count(n)"),
        CypherValue::Integer(1)
    );
    ok(
        &mut g,
        "MATCH (n:L1) CALL apoc.create.removeLabels([n], ['L2']) YIELD node RETURN node",
    );
    assert_eq!(
        one(&mut g, "MATCH (n:L2) RETURN count(n)"),
        CypherValue::Integer(0)
    );
    ok(
        &mut g,
        "MATCH (n:L1) CALL apoc.create.setLabels([n], ['Fresh']) YIELD node RETURN node",
    );
    assert_eq!(
        one(&mut g, "MATCH (n:Fresh) RETURN count(n)"),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one(&mut g, "MATCH (n:L1) RETURN count(n)"),
        CypherValue::Integer(0)
    );
}

// -- APOC extended: apoc.generate.* + apoc.static.* ---------------

#[test]
fn generate_graphs() {
    let mut g = Graph::new();
    ok(&mut g, "CALL apoc.generate.complete(5, 'K5', 'E')");
    assert_eq!(
        one(&mut g, "MATCH (n:K5) RETURN count(n)"),
        CypherValue::Integer(5)
    );
    assert_eq!(
        one(&mut g, "MATCH (:K5)-[r:E]->(:K5) RETURN count(r)"),
        CypherValue::Integer(10),
        "K5 has C(5,2) edges"
    );
    ok(&mut g, "CALL apoc.generate.er(10, 15, 'ER', 'R')");
    assert_eq!(
        one(&mut g, "MATCH (n:ER) RETURN count(n)"),
        CypherValue::Integer(10)
    );
    assert_eq!(
        one(&mut g, "MATCH (:ER)-[r:R]->(:ER) RETURN count(r)"),
        CypherValue::Integer(15)
    );
    ok(&mut g, "CALL apoc.generate.ba(20, 2, 'BA', 'L')");
    assert_eq!(
        one(&mut g, "MATCH (n:BA) RETURN count(n)"),
        CypherValue::Integer(20)
    );
    // BA with m=2: first edge + ~2 per node from node 2 on
    let edges = one(&mut g, "MATCH (:BA)-[r:L]->(:BA) RETURN count(r)");
    let CypherValue::Integer(e) = edges else {
        panic!()
    };
    assert!((20..=39).contains(&e), "BA edge count plausible: {e}");
    ok(&mut g, "CALL apoc.generate.ws(12, 4, 0.1, 'WS', 'W')");
    assert_eq!(
        one(&mut g, "MATCH (n:WS) RETURN count(n)"),
        CypherValue::Integer(12)
    );
    assert_eq!(
        one(&mut g, "MATCH (:WS)-[r:W]->(:WS) RETURN count(r)"),
        CypherValue::Integer(24),
        "ring lattice n*k/2 edges"
    );
    ok(&mut g, "CALL apoc.generate.simple([2, 2, 2], 'S', 'T')");
    assert_eq!(
        one(&mut g, "MATCH (:S)-[r:T]-(:S) RETURN count(DISTINCT r)"),
        CypherValue::Integer(3),
        "triangle from degree sequence [2,2,2]"
    );
    // defaults: null label/type -> Person/FRIEND_OF
    ok(&mut g, "CALL apoc.generate.complete(3, null, null)");
    assert_eq!(
        one(
            &mut g,
            "MATCH (:Person)-[r:FRIEND_OF]->(:Person) RETURN count(r)"
        ),
        CypherValue::Integer(3)
    );
    // non-graphical sequences error
    let e2 = run(&mut g, "CALL apoc.generate.simple([3, 1], 'X', 'Y')").unwrap_err();
    assert!(e2.contains("not graphical"), "got: {e2}");
}

#[test]
fn static_store_roundtrip() {
    let mut g = Graph::new();
    // set yields the PREVIOUS value
    let prev = ok(
        &mut g,
        "CALL apoc.static.set('t.k1', 41) YIELD value RETURN value",
    );
    assert_eq!(prev[0][0], CypherValue::Null);
    let prev2 = ok(
        &mut g,
        "CALL apoc.static.set('t.k1', 42) YIELD value RETURN value",
    );
    assert_eq!(prev2[0][0], CypherValue::Integer(41));
    ok(
        &mut g,
        "CALL apoc.static.set('t.k2', 'hello') YIELD value RETURN value",
    );
    // function and procedure forms of get
    assert_eq!(
        one(&mut g, "RETURN apoc.static.get('t.k1')"),
        CypherValue::Integer(42)
    );
    let got = ok(
        &mut g,
        "CALL apoc.static.get('t.k2') YIELD value RETURN value",
    );
    assert_eq!(got[0][0], CypherValue::String("hello".into()));
    // getAll strips the prefix
    let all = one(&mut g, "RETURN apoc.static.getAll('t')");
    let CypherValue::Map(m) = all else { panic!() };
    assert_eq!(m.get("k1"), Some(&CypherValue::Integer(42)));
    assert_eq!(m.get("k2"), Some(&CypherValue::String("hello".into())));
    // list rows.
    let rows = ok(
        &mut g,
        "CALL apoc.static.list('t') YIELD key, value RETURN key, value",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], CypherValue::String("t.k1".into()));
    // unset via null; survives across queries (process-lifetime store)
    ok(
        &mut g,
        "CALL apoc.static.set('t.k1', null) YIELD value RETURN value",
    );
    assert_eq!(
        one(&mut g, "RETURN apoc.static.get('t.k1')"),
        CypherValue::Null
    );
    ok(
        &mut g,
        "CALL apoc.static.set('t.k2', null) YIELD value RETURN value",
    );
}

// -- apoc.trigger.* + the graph event system ----------------------
// The trigger registry is process-global: these tests serialize on a lock and
// clear it on entry.

static TRIGGER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn trigger_test_guard(g: &mut Graph) -> std::sync::MutexGuard<'static, ()> {
    let guard = TRIGGER_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _ = run(g, "CALL apoc.trigger.removeAll() YIELD name RETURN name");
    guard
}

#[test]
fn trigger_lifecycle_and_created_nodes() {
    let mut g = Graph::new();
    let _serial = trigger_test_guard(&mut g);
    // audit trigger: stamp every created node
    ok(
        &mut g,
        "CALL apoc.trigger.add('t_stamp', \
           'UNWIND $createdNodes AS n SET n.audited = true', {phase: 'after'}, {}) \
         YIELD name RETURN name",
    );
    ok(&mut g, "CREATE (:TA {v: 1}), (:TA {v: 2})");
    assert_eq!(
        one(&mut g, "MATCH (n:TA {audited: true}) RETURN count(n)"),
        CypherValue::Integer(2),
        "trigger stamped both created nodes"
    );
    // list shape.
    let rows = ok(
        &mut g,
        "CALL apoc.trigger.list() YIELD name, query, installed, paused \
         RETURN name, installed, paused",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], CypherValue::Bool(true));
    assert_eq!(rows[0][2], CypherValue::Bool(false));
    // pause stops firing, resume restarts
    ok(
        &mut g,
        "CALL apoc.trigger.pause('t_stamp') YIELD name RETURN name",
    );
    ok(&mut g, "CREATE (:TA {v: 3})");
    assert_eq!(
        one(&mut g, "MATCH (n:TA {audited: true}) RETURN count(n)"),
        CypherValue::Integer(2),
        "paused trigger must not fire"
    );
    ok(
        &mut g,
        "CALL apoc.trigger.resume('t_stamp') YIELD name RETURN name",
    );
    ok(&mut g, "CREATE (:TA {v: 4})");
    assert_eq!(
        one(&mut g, "MATCH (n:TA {audited: true}) RETURN count(n)"),
        CypherValue::Integer(3)
    );
    ok(
        &mut g,
        "CALL apoc.trigger.remove('t_stamp') YIELD name RETURN name",
    );
    let empty = ok(&mut g, "CALL apoc.trigger.list() YIELD name RETURN name");
    assert!(empty.is_empty());
}

#[test]
fn trigger_deleted_nodes_are_virtual_snapshots() {
    let mut g = Graph::new();
    let _serial = trigger_test_guard(&mut g);
    ok(&mut g, "CREATE (:Tomb {name: 'x'}), (:Keep {name: 'y'})");
    ok(
        &mut g,
        "CALL apoc.trigger.add('t_grave', \
           'UNWIND $deletedNodes AS n CREATE (:Grave {was: n.name, labels: labels(n)})', \
           {phase: 'after'}, {}) YIELD name RETURN name",
    );
    ok(&mut g, "MATCH (t:Tomb) DELETE t");
    let rows = ok(&mut g, "MATCH (gr:Grave) RETURN gr.was, gr.labels");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("x".into()));
    assert_eq!(
        rows[0][1],
        CypherValue::List(vec![CypherValue::String("Tomb".into())]),
        "deleted-node snapshot keeps labels + properties"
    );
    ok(
        &mut g,
        "CALL apoc.trigger.removeAll() YIELD name RETURN name",
    );
}

#[test]
fn trigger_property_and_label_events() {
    let mut g = Graph::new();
    let _serial = trigger_test_guard(&mut g);
    ok(&mut g, "CREATE (:PP {name: 'a', v: 1})");
    ok(
        &mut g,
        "CALL apoc.trigger.add('t_props', \
           'UNWIND coalesce($assignedNodeProperties[\\'v\\'], []) AS c \
            CREATE (:Audit {key: c.key, old: c.old, new: c.new})', \
           {phase: 'after'}, {}) YIELD name RETURN name",
    );
    ok(&mut g, "MATCH (n:PP) SET n.v = 2");
    let rows = ok(&mut g, "MATCH (a:Audit) RETURN a.key, a.old, a.new");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("v".into()));
    assert_eq!(
        rows[0][1],
        CypherValue::Integer(1),
        "old value captured via unindex snapshot"
    );
    assert_eq!(rows[0][2], CypherValue::Integer(2));
    ok(
        &mut g,
        "CALL apoc.trigger.removeAll() YIELD name RETURN name",
    );

    // Label events.
    ok(
        &mut g,
        "CALL apoc.trigger.add('t_lbl', \
           'UNWIND coalesce($assignedLabels[\\'Hot\\'], []) AS n SET n.flagged = true', \
           {phase: 'after'}, {}) YIELD name RETURN name",
    );
    ok(&mut g, "MATCH (n:PP) SET n:Hot");
    assert_eq!(
        one(&mut g, "MATCH (n:PP {flagged: true}) RETURN count(n)"),
        CypherValue::Integer(1)
    );
    ok(
        &mut g,
        "CALL apoc.trigger.removeAll() YIELD name RETURN name",
    );
}

#[test]
fn trigger_no_cascade_and_failure_reporting() {
    let mut g = Graph::new();
    let _serial = trigger_test_guard(&mut g);
    // a trigger creating nodes must not re-trigger itself
    ok(
        &mut g,
        "CALL apoc.trigger.add('t_echo', \
           'UNWIND $createdNodes AS n CREATE (:Echo)', {phase: 'after'}, {}) \
         YIELD name RETURN name",
    );
    ok(&mut g, "CREATE (:Src1)");
    assert_eq!(
        one(&mut g, "MATCH (e:Echo) RETURN count(e)"),
        CypherValue::Integer(1),
        "exactly one echo - triggers do not cascade"
    );
    ok(
        &mut g,
        "CALL apoc.trigger.removeAll() YIELD name RETURN name",
    );
    // failing trigger statements name the trigger
    ok(
        &mut g,
        "CALL apoc.trigger.add('t_bad', 'UNWIND $createdNodes AS n RETURN 1/0', \
           {phase: 'after'}, {}) YIELD name RETURN name",
    );
    let e = run(&mut g, "CREATE (:Boom)").unwrap_err();
    assert!(e.contains("t_bad"), "got: {e}");
    ok(
        &mut g,
        "CALL apoc.trigger.removeAll() YIELD name RETURN name",
    );
    // bad statements are rejected at registration
    let e2 = run(
        &mut g,
        "CALL apoc.trigger.add('t_syntax', 'THIS IS NOT CYPHER', {}, {}) YIELD name RETURN name",
    )
    .unwrap_err();
    assert!(!e2.is_empty());
}

// -- apoc.export.* / apoc.import.* round-trips --------------------

fn fixture_for_export(g: &mut Graph) {
    ok(
        g,
        "CREATE (a:XP {name: 'ann', age: 30})-[:KNOWS {since: 1999}]->(b:XP:VIP {name: 'bob'}) \
         CREATE (a)-[:LIKES]->(:XQ {tag: 'x'})",
    );
}

fn export_counts(g: &mut Graph) -> (CypherValue, CypherValue) {
    let n = one(g, "MATCH (n) RETURN count(n)");
    let r = one(g, "MATCH ()-[r]->() RETURN count(r)");
    (n, r)
}

#[test]
fn export_import_json_roundtrip() {
    let dir = std::env::temp_dir().join("apoc_xp_json.jsonl");
    let mut g = Graph::new();
    fixture_for_export(&mut g);
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.export.json.all('{}', {{}}) YIELD nodes, relationships, format \
         RETURN nodes, relationships, format",
            cy(&dir)
        ),
    );
    assert_eq!(rows[0][0], CypherValue::Integer(3));
    assert_eq!(rows[0][1], CypherValue::Integer(2));
    // fresh graph -> import -> identical shape
    let mut g2 = Graph::new();
    let imp = ok(
        &mut g2,
        &format!(
            "CALL apoc.import.json('{}', {{}}) YIELD nodes, relationships RETURN nodes, relationships",
            cy(&dir)
        ),
    );
    assert_eq!(imp[0][0], CypherValue::Integer(3));
    assert_eq!(imp[0][1], CypherValue::Integer(2));
    assert_eq!(
        export_counts(&mut g2),
        (CypherValue::Integer(3), CypherValue::Integer(2))
    );
    assert_eq!(
        one(
            &mut g2,
            "MATCH (a:XP {name: 'ann'})-[k:KNOWS]->(b:VIP) RETURN k.since"
        ),
        CypherValue::Integer(1999),
        "properties, labels and topology survive the round-trip"
    );
    std::fs::remove_file(dir).ok();
}

#[test]
fn export_import_graphml_roundtrip() {
    let dir = std::env::temp_dir().join("apoc_xp.graphml");
    let mut g = Graph::new();
    fixture_for_export(&mut g);
    ok(
        &mut g,
        &format!(
            "CALL apoc.export.graphml.all('{}', {{}}) YIELD nodes RETURN nodes",
            cy(&dir)
        ),
    );
    let mut g2 = Graph::new();
    let imp = ok(
        &mut g2,
        &format!(
            "CALL apoc.import.graphml('{}', {{}}) YIELD nodes, relationships RETURN nodes, relationships",
            cy(&dir)
        ),
    );
    assert_eq!(imp[0][0], CypherValue::Integer(3));
    assert_eq!(imp[0][1], CypherValue::Integer(2));
    // GraphML properties import as strings (documented)
    assert_eq!(
        one(
            &mut g2,
            "MATCH (a:XP {name: 'ann'})-[:KNOWS]->(b:VIP {name: 'bob'}) RETURN count(*)"
        ),
        CypherValue::Integer(1)
    );
    std::fs::remove_file(dir).ok();
}

#[test]
fn export_cypher_script_replays() {
    let mut g = Graph::new();
    fixture_for_export(&mut g);
    // stream mode: the script comes back in the data column
    let rows = ok(
        &mut g,
        "CALL apoc.export.cypher.all('', {stream: true}) YIELD data RETURN data",
    );
    let CypherValue::String(script) = &rows[0][0] else {
        panic!()
    };
    assert!(
        script.contains("_UNIQUE_IMPORT_LABEL"),
        "APOC import-id convention"
    );
    // replay the script into a fresh graph
    let mut g2 = Graph::new();
    for stmt in script.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            ok(&mut g2, stmt);
        }
    }
    assert_eq!(
        export_counts(&mut g2),
        (CypherValue::Integer(3), CypherValue::Integer(2))
    );
    assert_eq!(
        one(
            &mut g2,
            "MATCH (:XP {name: 'ann'})-[k:KNOWS]->() RETURN k.since"
        ),
        CypherValue::Integer(1999)
    );
    // the cleanup statement removed the import ids
    assert_eq!(
        one(
            &mut g2,
            "MATCH (n) WHERE n.`_UNIQUE_IMPORT_ID` IS NOT NULL RETURN count(n)"
        ),
        CypherValue::Integer(0)
    );
}

#[test]
fn export_csv_and_query_forms() {
    let mut g = Graph::new();
    fixture_for_export(&mut g);
    let rows = ok(
        &mut g,
        "CALL apoc.export.csv.all('', {stream: true}) YIELD data, nodes RETURN data, nodes",
    );
    let CypherValue::String(csv) = &rows[0][0] else {
        panic!()
    };
    assert!(
        csv.starts_with("_id,_labels"),
        "APOC csv.all header shape: {csv}"
    );
    assert!(csv.contains(":XP"), "labels rendered with colons");
    // query export: plain result CSV
    let q = ok(
        &mut g,
        "CALL apoc.export.csv.query('MATCH (n:XP) RETURN n.name AS name ORDER BY name', '', {stream: true}) \
         YIELD data RETURN data",
    );
    let CypherValue::String(qcsv) = &q[0][0] else {
        panic!()
    };
    assert_eq!(qcsv.trim(), "name\nann\nbob");
    // Schema export.
    ok(&mut g, "CREATE INDEX xp_name FOR (n:XP) ON (n.name)");
    let s = ok(
        &mut g,
        "CALL apoc.export.cypher.schema('', {stream: true}) YIELD data RETURN data",
    );
    let CypherValue::String(schema) = &s[0][0] else {
        panic!()
    };
    assert!(schema.contains("CREATE INDEX `xp_name`"), "got: {schema}");
}

#[test]
fn import_csv_admin_convention() {
    let dir = std::env::temp_dir();
    let nfile = dir.join("apoc_ic_people.csv");
    let rfile = dir.join("apoc_ic_knows.csv");
    std::fs::write(
        &nfile,
        "id:ID,name,age:int,:LABEL\n1,ann,30,Person\n2,bob,25,Person;VIP\n",
    )
    .unwrap();
    std::fs::write(
        &rfile,
        ":START_ID,:END_ID,:TYPE,since:int\n1,2,KNOWS,1999\n",
    )
    .unwrap();
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.import.csv([{{fileName: '{}', labels: []}}], [{{fileName: '{}', type: 'KNOWS'}}], {{}}) \
         YIELD nodes, relationships RETURN nodes, relationships",
            cy(&nfile),
            cy(&rfile)
        ),
    );
    assert_eq!(rows[0][0], CypherValue::Integer(2));
    assert_eq!(rows[0][1], CypherValue::Integer(1));
    assert_eq!(
        one(
            &mut g,
            "MATCH (a:Person {name: 'ann', age: 30})-[k:KNOWS {since: 1999}]->(b:VIP) RETURN count(*)"
        ),
        CypherValue::Integer(1),
        "typed columns, multi-labels via ';', :TYPE column"
    );
    std::fs::remove_file(nfile).ok();
    std::fs::remove_file(rfile).ok();
}

#[test]
fn export_refuses_remote_urls() {
    let mut g = Graph::new();
    let e = run(
        &mut g,
        "CALL apoc.export.json.all('https://x.com/out.json', {})",
    )
    .unwrap_err();
    assert!(e.contains("local files only"), "got: {e}");
    let e2 = run(&mut g, "CALL apoc.import.json('https://x.com/in.json', {})").unwrap_err();
    assert!(e2.contains("local files only"), "got: {e2}");
}

// -- BYTEARRAY + arrow + compress ---------------------------------

#[test]
fn compress_roundtrip_and_bytes_type() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WITH apoc.util.compress('hello compression world', {compression: 'GZIP'}) AS b \
         RETURN valueType(b) AS t, apoc.util.decompress(b, {compression: 'GZIP'}) AS back, size(apoc.text.bytes('x')) AS sanity",
    );
    assert_eq!(rows[0][0], CypherValue::String("BYTEARRAY NOT NULL".into()));
    assert_eq!(
        rows[0][1],
        CypherValue::String("hello compression world".into())
    );
    // DEFLATE + NONE
    let r2 = ok(
        &mut g,
        "WITH apoc.util.compress('abc', {compression: 'DEFLATE'}) AS b \
         RETURN apoc.util.decompress(b, {compression: 'DEFLATE'})",
    );
    assert_eq!(r2[0][0], CypherValue::String("abc".into()));
    let e = run(
        &mut g,
        "RETURN apoc.util.compress('x', {compression: 'BZIP2'})",
    )
    .unwrap_err();
    assert!(e.contains("unsupported compression"), "got: {e}");
    // bytes properties round-trip through storage
    ok(
        &mut g,
        "CREATE (n:BinHolder {payload: apoc.util.compress('stored', {})})",
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (n:BinHolder) RETURN apoc.util.decompress(n.payload, {})"
        ),
        CypherValue::String("stored".into())
    );
}

#[test]
fn arrow_export_roundtrip() {
    let dir = std::env::temp_dir().join("apoc_xp.arrow");
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:AR {name: 'ann', age: 30, score: 1.5})-[:KNOWS {since: 1999}]->(b:AR {name: 'bob', age: 25})",
    );
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.export.arrow.all('{}', {{}}) YIELD nodes, relationships, format \
         RETURN nodes, relationships, format",
            cy(&dir)
        ),
    );
    assert_eq!(rows[0][0], CypherValue::Integer(2));
    assert_eq!(rows[0][1], CypherValue::Integer(1));
    assert_eq!(rows[0][2], CypherValue::String("arrow".into()));
    // read back via apoc.load.arrow (typed columns kept)
    let back = ok(
        &mut g,
        &format!(
            "CALL apoc.load.arrow('{}') YIELD value \
         WHERE value.name IS NOT NULL \
         RETURN value.name AS n, value.age AS a, value.labels AS l ORDER BY n",
            cy(&dir)
        ),
    );
    assert_eq!(back.len(), 2);
    assert_eq!(back[0][0], CypherValue::String("ann".into()));
    assert_eq!(
        back[0][1],
        CypherValue::Integer(30),
        "Int64 column preserved"
    );
    assert_eq!(back[0][2], CypherValue::String(":AR".into()));
    // relationship row carries type + endpoint ids
    let rel = ok(
        &mut g,
        &format!(
            "CALL apoc.load.arrow('{}') YIELD value WHERE value.`<type>` IS NOT NULL \
         RETURN value.`<type>`, value.since",
            cy(&dir)
        ),
    );
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0][0], CypherValue::String("KNOWS".into()));
    assert_eq!(rel[0][1], CypherValue::Integer(1999));
    std::fs::remove_file(dir).ok();
}

#[test]
fn arrow_stream_and_query_forms() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:AQ {v: 1}), (:AQ {v: 2})");
    // stream: BYTEARRAY payload in the value column
    let rows = ok(
        &mut g,
        "CALL apoc.export.arrow.stream.all({}) YIELD value RETURN valueType(value) AS t, size(value) > 100 AS nonempty",
    );
    assert_eq!(rows[0][0], CypherValue::String("BYTEARRAY NOT NULL".into()));
    assert_eq!(rows[0][1], CypherValue::Bool(true));
    // query export -> arrow file -> typed load
    let dir = std::env::temp_dir().join("apoc_q.arrow");
    ok(
        &mut g,
        &format!(
            "CALL apoc.export.arrow.query('{}', 'MATCH (n:AQ) RETURN n.v AS v ORDER BY v', {{}}) YIELD rows RETURN rows",
            cy(&dir)
        ),
    );
    let back = ok(
        &mut g,
        &format!(
            "CALL apoc.load.arrow('{}') YIELD value RETURN value.v ORDER BY value.v",
            cy(&dir)
        ),
    );
    assert_eq!(back.len(), 2);
    assert_eq!(back[0][0], CypherValue::Integer(1));
    assert_eq!(back[1][0], CypherValue::Integer(2));
    std::fs::remove_file(dir).ok();
}

// -- runFirstColumn* (COLLECT{} desugar) + xml.parse/load.xml -----

#[test]
fn run_first_column_desugar() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:RFC {name: 'a', v: 1}), (:RFC {name: 'b', v: 2}), (:RFC {name: 'c', v: 3})",
    );
    // Many: whole first column
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.cypher.runFirstColumnMany('MATCH (m:RFC) RETURN m.name ORDER BY m.name', {})"
        ),
        CypherValue::List(vec![
            CypherValue::String("a".into()),
            CypherValue::String("b".into()),
            CypherValue::String("c".into()),
        ])
    );
    // Single: head of the column
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.cypher.runFirstColumnSingle('MATCH (m:RFC) RETURN sum(m.v)', {})"
        ),
        CypherValue::Integer(6)
    );
    // correlated params: the outer variable flows through the params map
    let rows = ok(
        &mut g,
        "MATCH (n:RFC) WHERE n.name = 'a' \
         RETURN apoc.cypher.runFirstColumnMany(\
           'MATCH (m:RFC) WHERE m.v > minv RETURN m.name ORDER BY m.name', {minv: n.v}) AS bigger",
    );
    assert_eq!(
        rows[0][0],
        CypherValue::List(vec![
            CypherValue::String("b".into()),
            CypherValue::String("c".into()),
        ]),
        "params bind as identifiers, correlated with the outer row"
    );
    // legacy 2/3-arg spelling
    assert_eq!(
        one(
            &mut g,
            "RETURN apoc.cypher.runFirstColumn('RETURN 41 + 1', {})"
        ),
        CypherValue::List(vec![CypherValue::Integer(42)])
    );
    // dynamic (non-literal) statement -> clear error pointing at the CALL form
    let e = run(
        &mut g,
        "WITH 'RETURN 1' AS q RETURN apoc.cypher.runFirstColumnMany(q, {})",
    )
    .unwrap_err();
    assert!(e.contains("string-literal statement"), "got: {e}");
}

#[test]
fn xml_parse_and_load() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WITH apoc.xml.parse('<catalog owner=\"me\"><book id=\"1\"><title>Dune</title></book><book id=\"2\"/></catalog>') AS x \
         RETURN x._type, x.owner, size(x._children), x._children[0]._type, x._children[0]._children[0]._text, x._children[1].id",
    );
    assert_eq!(rows[0][0], CypherValue::String("catalog".into()));
    assert_eq!(rows[0][1], CypherValue::String("me".into()));
    assert_eq!(rows[0][2], CypherValue::Integer(2));
    assert_eq!(rows[0][3], CypherValue::String("book".into()));
    assert_eq!(
        rows[0][4],
        CypherValue::String("Dune".into()),
        "text-only child gets _text"
    );
    assert_eq!(
        rows[0][5],
        CypherValue::String("2".into()),
        "self-closing element with attribute"
    );
    // simple mode: children under _<tag> keys
    let s = ok(
        &mut g,
        "WITH apoc.xml.parse('<r><a>1</a><a>2</a><b>3</b></r>', '/', {}, true) AS x \
         RETURN size(x._a), x._a[1]._text, x._b[0]._text",
    );
    assert_eq!(s[0][0], CypherValue::Integer(2));
    assert_eq!(s[0][1], CypherValue::String("2".into()));
    assert_eq!(s[0][2], CypherValue::String("3".into()));
    // load.xml: same shape from a local file
    let f = std::env::temp_dir().join("apoc_lx.xml");
    std::fs::write(&f, "<root><item k=\"v\">hello</item></root>").unwrap();
    let l = ok(
        &mut g,
        &format!(
            "CALL apoc.load.xml('{}') YIELD value \
         RETURN value._type, value._children[0].k, value._children[0]._text",
            cy(&f)
        ),
    );
    assert_eq!(l[0][0], CypherValue::String("root".into()));
    assert_eq!(l[0][1], CypherValue::String("v".into()));
    assert_eq!(l[0][2], CypherValue::String("hello".into()));
    std::fs::remove_file(f).ok();
    // unsupported xpath -> clear error
    let e = run(&mut g, "RETURN apoc.xml.parse('<a/>', '/a/b')").unwrap_err();
    assert!(e.contains("root path"), "got: {e}");
}

// -- apoc.nodes.group + apoc.import.xml ---------------------------

#[test]
fn nodes_group_virtual_aggregation() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:NG {g: 'a', v: 1}), (:NG {g: 'a', v: 2}), (:NG {g: 'b', v: 10})",
    );
    ok(
        &mut g,
        "MATCH (x:NG {g: 'a', v: 1}), (y:NG {g: 'b'}) CREATE (x)-[:LINKS {w: 5}]->(y)",
    );
    ok(
        &mut g,
        "MATCH (x:NG {g: 'a', v: 2}), (y:NG {g: 'b'}) CREATE (x)-[:LINKS {w: 7}]->(y)",
    );
    // as live APOC: labels = selected label, props = groupBy + count_* (+ sum_v), one row per
    // (node, outgoing rel), orphan groups get relationship = null
    let rows = ok(
        &mut g,
        "CALL apoc.nodes.group(['NG'], ['g'], [{`*`: 'count', v: 'sum'}, {`*`: 'count', w: 'sum'}]) \
         YIELD node, relationship \
         RETURN labels(node), node.g, node.`count_*`, node.sum_v, \
                type(relationship), relationship.`count_*`, relationship.sum_w \
         ORDER BY node.g",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][0],
        CypherValue::List(vec![CypherValue::String("NG".into())])
    );
    assert_eq!(rows[0][1], CypherValue::String("a".into()));
    assert_eq!(rows[0][2], CypherValue::Integer(2));
    assert_eq!(rows[0][3], CypherValue::Integer(3));
    assert_eq!(rows[0][4], CypherValue::String("LINKS".into()));
    assert_eq!(
        rows[0][5],
        CypherValue::Integer(2),
        "2 edges aggregated into one virtual rel"
    );
    assert_eq!(rows[0][6], CypherValue::Integer(12));
    // group b is an orphan (no outgoing rels) -> null relationship
    assert_eq!(rows[1][1], CypherValue::String("b".into()));
    assert_eq!(rows[1][4], CypherValue::Null);
    // orphans: false drops the b row
    let no_orphans = ok(
        &mut g,
        "CALL apoc.nodes.group(['NG'], ['g'], null, {orphans: false}) YIELD node RETURN node.g",
    );
    assert_eq!(no_orphans.len(), 1);
    assert_eq!(no_orphans[0][0], CypherValue::String("a".into()));
}

#[test]
fn import_xml_graph_model() {
    let f = std::env::temp_dir().join("apoc_ix.xml");
    std::fs::write(
        &f,
        "<book title=\"Dune\"><chapter>great desert</chapter><chapter>worms</chapter></book>",
    )
    .unwrap();
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.import.xml('{}') YIELD node RETURN labels(node)",
            cy(&f)
        ),
    );
    assert_eq!(
        rows[0][0],
        CypherValue::List(vec![CypherValue::String("XmlDocument".into())])
    );
    // structure: root tag under document, chapters under book, words under chapters
    assert_eq!(
        one(&mut g, "MATCH (t:XmlTag {_name: 'book'}) RETURN t.title"),
        CypherValue::String("Dune".into()),
        "attributes become tag properties"
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (c:XmlTag {_name: 'chapter'})-[:IS_CHILD_OF]->(:XmlTag {_name: 'book'}) RETURN count(c)"
        ),
        CypherValue::Integer(2)
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (w:XmlCharacters)-[:IS_CHILD_OF]->(:XmlTag {_name: 'chapter'}) RETURN count(w)"
        ),
        CypherValue::Integer(4),
        "APOC tokenization: great, ' ', desert, worms (whitespace runs included)"
    );
    // cumulative character offsets across the document
    let idx = ok(
        &mut g,
        "MATCH (w:XmlCharacters) RETURN w.text, w.startIndex, w.endIndex ORDER BY w.startIndex",
    );
    assert_eq!(idx[0][0], CypherValue::String("great".into()));
    assert_eq!(idx[0][1], CypherValue::Integer(0));
    assert_eq!(idx[0][2], CypherValue::Integer(4));
    assert_eq!(idx[3][0], CypherValue::String("worms".into()));
    assert_eq!(
        idx[3][1],
        CypherValue::Integer(12),
        "offsets continue across elements"
    );
    // LAST_CHILD_OF to tag parents
    assert_eq!(
        one(
            &mut g,
            "MATCH (w:XmlCharacters {text: 'desert'})-[:LAST_CHILD_OF]->(c) RETURN c._name"
        ),
        CypherValue::String("chapter".into())
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (:XmlTag {_name: 'chapter'})-[:NEXT_SIBLING]->(c2:XmlTag) RETURN count(c2)"
        ),
        CypherValue::Integer(1)
    );
    // NEXT chain spans the document in order
    assert_eq!(
        one(
            &mut g,
            "MATCH p = (:XmlDocument)-[:NEXT*]->(w:XmlCharacters {text: 'worms'}) RETURN length(p)"
        ),
        CypherValue::Integer(7),
        "doc -> book -> chapter -> great -> ' ' -> desert -> chapter -> worms"
    );
    // NEXT_WORD only with the config flag
    assert_eq!(
        one(&mut g, "MATCH ()-[r:NEXT_WORD]->() RETURN count(r)"),
        CypherValue::Integer(0)
    );
    ok(&mut g, "MATCH (n) DETACH DELETE n");
    let rows2 = ok(
        &mut g,
        &format!(
            "CALL apoc.import.xml('{}', {{createNextWordRelationships: true}}) YIELD node RETURN node",
            cy(&f)
        ),
    );
    assert_eq!(rows2.len(), 1);
    assert_eq!(
        one(
            &mut g,
            "MATCH (:XmlCharacters {text: 'desert'})-[:NEXT_WORD]->(w) RETURN w.text"
        ),
        CypherValue::String("worms".into()),
        "NEXT_WORD skips whitespace tokens"
    );
    std::fs::remove_file(f).ok();
}

// -- schedulers, geocode, jsonParams, movies, jdbc ------------------

/// One-shot canned HTTP server on a random port: (port, handle). Records the request head so tests
/// can check method/headers/body.
fn canned_http(response_json: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<String>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            // read until headers AND any Content-Length body are in (a POST body may arrive in a
            // second TCP segment)
            s.set_read_timeout(Some(std::time::Duration::from_millis(500)))
                .ok();
            let mut req = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
                let text = String::from_utf8_lossy(&req);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let clen = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if req.len() >= head_end + 4 + clen {
                        break;
                    }
                }
            }
            *seen2.lock().unwrap() = String::from_utf8_lossy(&req).to_string();
            let body = response_json;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (port, seen)
}

#[test]
fn periodic_scheduler_embedded_inline() {
    let mut g = Graph::new();
    // no background executor in embedded tests: submit runs inline once
    let rows = ok(
        &mut g,
        "CALL apoc.periodic.submit('job1', 'CREATE (:Tick)') YIELD name, done, cancelled \
         RETURN name, done, cancelled",
    );
    assert_eq!(rows[0][0], CypherValue::String("job1".into()));
    assert_eq!(
        rows[0][1],
        CypherValue::Bool(true),
        "inline fallback completes immediately"
    );
    assert_eq!(
        one(&mut g, "MATCH (t:Tick) RETURN count(t)"),
        CypherValue::Integer(1),
        "statement executed exactly once"
    );
    // list shows the job; cancel removes it
    let listed = ok(
        &mut g,
        "CALL apoc.periodic.list() YIELD name, done RETURN name, done",
    );
    assert!(
        listed
            .iter()
            .any(|r| r[0] == CypherValue::String("job1".into()))
    );
    ok(
        &mut g,
        "CALL apoc.periodic.cancel('job1') YIELD cancelled RETURN cancelled",
    );
    let after = ok(&mut g, "CALL apoc.periodic.list() YIELD name RETURN name");
    assert!(
        !after
            .iter()
            .any(|r| r[0] == CypherValue::String("job1".into()))
    );
    // bad statements rejected at submit
    let e = run(&mut g, "CALL apoc.periodic.submit('bad', 'NOT CYPHER')").unwrap_err();
    assert!(!e.is_empty());
}

#[test]
fn geocode_with_provider_override() {
    let (port, _) =
        canned_http(r#"[{"lat": "59.33", "lon": "18.06", "display_name": "Stockholm, Sweden"}]"#);
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.spatial.geocode('Stockholm', 1, false, {{url: 'http://127.0.0.1:{port}/search?q=PLACE'}}) \
         YIELD latitude, longitude, description RETURN latitude, longitude, description"
        ),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Float(59.33));
    assert_eq!(rows[0][1], CypherValue::Float(18.06));
    assert_eq!(rows[0][2], CypherValue::String("Stockholm, Sweden".into()));
    // reverseGeocode with LAT/LNG substitution
    let (port2, seen2) =
        canned_http(r#"{"lat": "59.33", "lon": "18.06", "display_name": "Somewhere"}"#);
    let r2 = ok(
        &mut g,
        &format!(
            "CALL apoc.spatial.reverseGeocode(59.33, 18.06, false, {{reverseUrl: 'http://127.0.0.1:{port2}/rev?lat=LAT&lon=LNG'}}) \
         YIELD description RETURN description"
        ),
    );
    assert_eq!(r2[0][0], CypherValue::String("Somewhere".into()));
    assert!(
        seen2.lock().unwrap().contains("lat=59.33&lon=18.06"),
        "LAT/LNG substituted"
    );
}

#[test]
fn load_json_params_headers_and_payload() {
    let (port, seen) = canned_http(r#"{"ok": true, "n": 7}"#);
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.load.jsonParams('http://127.0.0.1:{port}/api', \
           {{Authorization: 'Bearer tok123'}}, '{{\"q\": 1}}') \
         YIELD value RETURN value.ok, value.n"
        ),
    );
    assert_eq!(rows[0][0], CypherValue::Bool(true));
    assert_eq!(rows[0][1], CypherValue::Integer(7));
    let req = seen.lock().unwrap().clone();
    assert!(req.starts_with("POST "), "payload forces POST: {req}");
    // header names are case-insensitive on the wire (ureq 3 lowercases them, as the `http` crate
    // canonicalises)
    assert!(
        req.to_ascii_lowercase()
            .contains("authorization: bearer tok123"),
        "header forwarded: {req}"
    );
    assert!(req.contains("{\"q\": 1}"), "payload sent");
}

#[test]
fn example_movies_dataset() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "CALL apoc.example.movies() YIELD nodes, relationships RETURN nodes, relationships",
    );
    assert_eq!(
        rows[0][0],
        CypherValue::Integer(169),
        "38 movies + 131 people (matches live APOC report)"
    );
    // actual graph truth (live APOC agrees):
    assert_eq!(
        one(&mut g, "MATCH ()-[r]->() RETURN count(r)"),
        CypherValue::Integer(250)
    );
    assert_eq!(
        rows[0][1],
        CypherValue::Integer(250),
        "matches live APOC report"
    );
    assert_eq!(
        one(&mut g, "MATCH (m:Movie) RETURN count(m)"),
        CypherValue::Integer(38)
    );
    assert_eq!(
        one(
            &mut g,
            "MATCH (p:Person {name: 'Keanu Reeves'})-[:ACTED_IN]->(m) RETURN count(m)"
        ),
        CypherValue::Integer(7)
    );
}

#[test]
fn load_jdbc_sqlite() {
    let db = std::env::temp_dir().join("apoc_jdbc_test.db");
    std::fs::remove_file(&db).ok();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL); \
             INSERT INTO people VALUES (1, 'ann', 9.5), (2, 'bob', NULL);",
        )
        .unwrap();
    }
    let mut g = Graph::new();
    // Full SQL form.
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.load.jdbc('jdbc:sqlite:{}', 'SELECT * FROM people ORDER BY id') \
         YIELD row RETURN row.id, row.name, row.score",
            cy(&db)
        ),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], CypherValue::Integer(1));
    assert_eq!(rows[0][1], CypherValue::String("ann".into()));
    assert_eq!(rows[0][2], CypherValue::Float(9.5));
    assert_eq!(rows[1][2], CypherValue::Null, "SQL NULL maps to null");
    // Bare table-name form.
    let t = ok(
        &mut g,
        &format!(
            "CALL apoc.load.jdbc('jdbc:sqlite:{}', 'people') YIELD row RETURN count(row)",
            cy(&db)
        ),
    );
    assert_eq!(t[0][0], CypherValue::Integer(2));
    // unsupported dialect: clear error
    let e = run(&mut g, "CALL apoc.load.jdbc('jdbc:oracle:thin:@x', 't')").unwrap_err();
    assert!(e.contains("supported dialects"), "got: {e}");
    std::fs::remove_file(db).ok();
}

// -- apoc.log.stream -----------------------------------------------

#[test]
fn log_stream_file_and_ring() {
    let mut g = Graph::new();
    // file variant: stream a real log file
    let f = std::env::temp_dir().join("apoc_ls.log");
    std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.log.stream('{}') YIELD lineNo, line RETURN lineNo, line",
            cy(&f)
        ),
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], CypherValue::Integer(0));
    assert_eq!(rows[0][1], CypherValue::String("alpha".into()));
    // {last: N} tails
    let tail = ok(
        &mut g,
        &format!(
            "CALL apoc.log.stream('{}', {{last: 1}}) YIELD lineNo, line RETURN lineNo, line",
            cy(&f)
        ),
    );
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0][0], CypherValue::Integer(2));
    assert_eq!(tail[0][1], CypherValue::String("gamma".into()));
    std::fs::remove_file(&f).ok();
    // ring-buffer variant: a missing path falls back to the in-process buffer the server's tracing
    // layer feeds
    traverse_cypher::apoc_log::push_line("test ring line 1");
    traverse_cypher::apoc_log::push_line("test ring line 2");
    let ring = ok(
        &mut g,
        "CALL apoc.log.stream('traverse.log') YIELD line \
         WITH line WHERE line STARTS WITH 'test ring' RETURN line ORDER BY line",
    );
    assert!(ring.len() >= 2, "ring lines streamed: {}", ring.len());
    assert_eq!(ring[0][0], CypherValue::String("test ring line 1".into()));
}

// -- label-expression scans use the label index ----------------------

#[test]
fn label_expr_scans_are_index_backed() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:LX1 {name: 'a'}), (:LX1:LX3 {name: 'b'}), (:LX2 {name: 'c'}), (:LX4 {name: 'd'})",
    );
    // WHERE conjunction with negation
    let rows = ok(
        &mut g,
        "MATCH (n) WHERE n:LX1&!LX3 RETURN n.name AS n ORDER BY n",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("a".into()));
    // pattern disjunction + negation + grouping
    let rows2 = ok(
        &mut g,
        "MATCH (n:(LX1|LX2)&!LX3) RETURN n.name AS n ORDER BY n",
    );
    assert_eq!(rows2.len(), 2);
    assert_eq!(rows2[0][0], CypherValue::String("a".into()));
    assert_eq!(rows2[1][0], CypherValue::String("c".into()));
    // dedup: a node with BOTH alternative labels binds once
    ok(&mut g, "CREATE (:LX1:LX2 {name: 'both'})");
    let rows3 = ok(&mut g, "MATCH (n:LX1|LX2) RETURN count(n)");
    assert_eq!(
        rows3[0][0],
        CypherValue::Integer(4),
        "a, b, c, both - each once"
    );
}

/// quick-xml >= 0.38 reports `&amp;`-style references as separate events; consumers must still see
/// one text node per run of character data, as APOC's JVM parser delivers it.
#[test]
fn xml_entity_references_stay_one_text_node() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WITH apoc.xml.parse('<r><a>AT&amp;T &lt;3 &#65;</a><b>x<!--c-->y</b></r>') AS x \
         RETURN x._children[0]._text, x._children[1]._text",
    );
    assert_eq!(rows[0][0], CypherValue::String("AT&T <3 A".into()));
    assert_eq!(
        rows[0][1],
        CypherValue::String("xy".into()),
        "comment does not split the text"
    );

    // apoc.import.xml tokenizes words: `AT&amp;T` is one word, not three
    let f = std::env::temp_dir().join("apoc_ix_ent.xml");
    std::fs::write(&f, "<p>AT&amp;T rocks</p>").unwrap();
    ok(
        &mut g,
        &format!("CALL apoc.import.xml('{}') YIELD node RETURN node", cy(&f)),
    );
    std::fs::remove_file(f).ok();
    let words = ok(
        &mut g,
        "MATCH (w:XmlCharacters)-[:IS_CHILD_OF]->(:XmlTag {_name: 'p'}) \
         RETURN w.text, w.startIndex, w.endIndex ORDER BY w.startIndex",
    );
    assert_eq!(words.len(), 3, "AT&T, ' ', rocks");
    assert_eq!(words[0][0], CypherValue::String("AT&T".into()));
    assert_eq!(words[0][1], CypherValue::Integer(0));
    assert_eq!(words[0][2], CypherValue::Integer(3));
    assert_eq!(words[2][0], CypherValue::String("rocks".into()));
    assert_eq!(words[2][1], CypherValue::Integer(5));

    // GraphML: a value with references round-trips whole, and the importer's trimming keeps the
    // space next to a reference
    let gml = "<graphml><graph><node id=\"n0\"><data key=\"labels\">Co</data>\
               <data key=\"name\">\n  AT&amp;T &quot;Inc&quot;\n</data></node></graph></graphml>";
    let f = std::env::temp_dir().join("apoc_ig_ent.graphml");
    std::fs::write(&f, gml).unwrap();
    let mut g2 = Graph::new();
    ok(
        &mut g2,
        &format!(
            "CALL apoc.import.graphml('{}', {{}}) YIELD nodes RETURN nodes",
            cy(&f)
        ),
    );
    std::fs::remove_file(f).ok();
    assert_eq!(
        one(&mut g2, "MATCH (n:Co) RETURN n.name"),
        CypherValue::String("AT&T \"Inc\"".into())
    );
}

/// rusqlite >= 0.35 rejects SQL with more than one statement instead of silently running the first;
/// a trailing semicolon is still one statement.
#[test]
fn load_jdbc_sqlite_single_statement_only() {
    let db = std::env::temp_dir().join("apoc_jdbc_multi.db");
    std::fs::remove_file(&db).ok();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1), (2);")
            .unwrap();
    }
    let url = format!("jdbc:sqlite:{}", cy(&db));
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.load.jdbc('{url}', 'SELECT id FROM t ORDER BY id;') YIELD row RETURN row.id"
        ),
    );
    assert_eq!(rows.len(), 2, "trailing semicolon is one statement");
    assert_eq!(rows[1][0], CypherValue::Integer(2));

    let err = run(&mut g, &format!(
        "CALL apoc.load.jdbc('{url}', 'SELECT id FROM t; DELETE FROM t') YIELD row RETURN row.id"))
        .unwrap_err();
    assert!(
        err.contains("apoc.load.jdbc"),
        "clear, attributed error: {err}"
    );

    // neither statement ran
    let rows = ok(
        &mut g,
        &format!(
            "CALL apoc.load.jdbc('{url}', 'SELECT count(*) AS n FROM t') YIELD row RETURN row.n"
        ),
    );
    assert_eq!(rows[0][0], CypherValue::Integer(2));
    std::fs::remove_file(db).ok();
}
