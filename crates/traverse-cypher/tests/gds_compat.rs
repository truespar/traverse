//! GDS compatibility layer: catalog + algorithm mappings.

use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run(g: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    traverse_cypher::execute(g, q)
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(g: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(g, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn one(g: &mut Graph, q: &str) -> CypherValue {
    ok(g, q).remove(0).remove(0)
}

/// Two bridged triangles + an isolated pair: deterministic shapes for wcc/degree/pageRank
/// orderings.
fn fixture(g: &mut Graph) {
    ok(
        g,
        "CREATE (a:P {id: 0}), (b:P {id: 1}), (c:P {id: 2}), \
                (d:P {id: 3}), (e:P {id: 4}), (f:P {id: 5}), \
                (x:Q {id: 6}), (y:Q {id: 7}) \
         CREATE (a)-[:R {w: 1.0}]->(b), (b)-[:R {w: 1.0}]->(c), (c)-[:R {w: 1.0}]->(a), \
                (d)-[:R {w: 1.0}]->(e), (e)-[:R {w: 1.0}]->(f), (f)-[:R {w: 1.0}]->(d), \
                (c)-[:R {w: 5.0}]->(d), \
                (x)-[:S {w: 1.0}]->(y)",
    );
}

#[test]
fn catalog_roundtrip() {
    let mut g = Graph::new();
    fixture(&mut g);
    let rows = ok(
        &mut g,
        "CALL gds.graph.project('gc_test', 'P', 'R') \
         YIELD graphName, nodeCount, relationshipCount \
         RETURN graphName, nodeCount, relationshipCount",
    );
    assert_eq!(rows[0][0], CypherValue::String("gc_test".into()));
    assert_eq!(rows[0][1], CypherValue::Integer(6));
    assert_eq!(rows[0][2], CypherValue::Integer(7));
    let ex = ok(
        &mut g,
        "CALL gds.graph.exists('gc_test') YIELD exists RETURN exists",
    );
    assert_eq!(ex[0][0], CypherValue::Bool(true));
    let listed = ok(
        &mut g,
        "CALL gds.graph.list() YIELD graphName WHERE graphName = 'gc_test' RETURN graphName",
    );
    assert_eq!(listed.len(), 1);
    ok(
        &mut g,
        "CALL gds.graph.drop('gc_test') YIELD graphName RETURN graphName",
    );
    let ex2 = ok(
        &mut g,
        "CALL gds.graph.exists('gc_test') YIELD exists RETURN exists",
    );
    assert_eq!(ex2[0][0], CypherValue::Bool(false));
    // dropping a missing graph errors unless failIfMissing=false
    assert!(run(&mut g, "CALL gds.graph.drop('gc_test')").is_err());
    let quiet = ok(
        &mut g,
        "CALL gds.graph.drop('gc_test', false) YIELD graphName RETURN count(*)",
    );
    assert_eq!(quiet[0][0], CypherValue::Integer(0));
}

#[test]
fn wcc_and_degree_through_projection() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('g1', 'P', 'R')");
    // wcc: the P subgraph is ONE component (triangles are bridged)
    let comp = ok(
        &mut g,
        "CALL gds.wcc.stream('g1') YIELD nodeId, componentId \
         RETURN count(DISTINCT componentId)",
    );
    assert_eq!(comp[0][0], CypherValue::Integer(1));
    // projection filters hold: Q/S excluded, still one component of 6
    let n = ok(
        &mut g,
        "CALL gds.wcc.stream('g1') YIELD nodeId RETURN count(nodeId)",
    );
    assert_eq!(n[0][0], CypherValue::Integer(6));
    // degree with GDS column names; util.asNode joins back to properties
    let deg = ok(
        &mut g,
        "CALL gds.degree.stream('g1') YIELD nodeId, score \
         RETURN gds.util.asNode(nodeId).id AS id, score ORDER BY score DESC, id LIMIT 1",
    );
    assert_eq!(
        deg[0][1],
        CypherValue::Float(2.0),
        "node c: two outgoing R edges"
    );
    ok(&mut g, "CALL gds.graph.drop('g1')");
}

#[test]
fn pagerank_and_write_mode() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('g2', '*', '*')");
    let rows = ok(
        &mut g,
        "CALL gds.pageRank.stream('g2', {maxIterations: 20, dampingFactor: 0.85}) \
         YIELD nodeId, score RETURN count(nodeId), min(score) > 0",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(8));
    assert_eq!(rows[0][1], CypherValue::Bool(true));
    // write mode persists the property
    let w = ok(
        &mut g,
        "CALL gds.pageRank.write('g2', {writeProperty: 'pr'}) \
         YIELD nodePropertiesWritten RETURN nodePropertiesWritten",
    );
    assert_eq!(w[0][0], CypherValue::Integer(8));
    assert_eq!(
        one(&mut g, "MATCH (n) WHERE n.pr IS NOT NULL RETURN count(n)"),
        CypherValue::Integer(8)
    );
    ok(&mut g, "CALL gds.graph.drop('g2')");
}

#[test]
fn dijkstra_similarity_louvain() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(
        &mut g,
        "CALL gds.graph.project('g3', 'P', {R: {type: 'R', properties: 'w'}})",
    );
    // weighted shortest path a(0) -> d(3): a->b->c->d = 1+1+5 = 7
    let sp = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (d:P {id: 3}) \
         CALL gds.shortestPath.dijkstra.stream('g3', \
              {sourceNode: a, targetNode: d, relationshipWeightProperty: 'w'}) \
         YIELD totalCost RETURN totalCost",
    );
    assert_eq!(sp[0][0], CypherValue::Float(7.0));
    // as in GDS, the projected weights count only when the call names them: 3 hops
    let hops = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (d:P {id: 3}) \
         CALL gds.shortestPath.dijkstra.stream('g3', {sourceNode: a, targetNode: d}) \
         YIELD totalCost RETURN totalCost",
    );
    assert_eq!(hops[0][0], CypherValue::Float(3.0));
    // nodeSimilarity with GDS column names (node1/node2/similarity)
    let sim = ok(
        &mut g,
        "CALL gds.nodeSimilarity.stream('g3') YIELD node1, node2, similarity \
         RETURN count(*) > 0, min(similarity) >= 0.0",
    );
    assert_eq!(sim[0][0], CypherValue::Bool(true));
    assert_eq!(sim[0][1], CypherValue::Bool(true));
    // louvain: all 6 projected nodes assigned, a sane community count (exact count on 6 nodes
    // depends on resolution)
    let lv = ok(
        &mut g,
        "CALL gds.louvain.stream('g3') YIELD nodeId, communityId \
         RETURN count(nodeId), count(DISTINCT communityId)",
    );
    assert_eq!(lv[0][0], CypherValue::Integer(6));
    let CypherValue::Integer(comms) = lv[0][1] else {
        panic!()
    };
    assert!((1..=3).contains(&comms), "community count sane: {comms}");
    ok(&mut g, "CALL gds.graph.drop('g3')");
}

#[test]
fn gds_functions_and_errors() {
    let mut g = Graph::new();
    fixture(&mut g);
    // similarity functions
    let r = ok(
        &mut g,
        "RETURN gds.similarity.cosine([1.0, 0.0], [1.0, 0.0]) AS c, \
                gds.similarity.jaccard([1.0, 2.0, 3.0], [2.0, 3.0, 4.0]) AS j, \
                gds.similarity.euclideanDistance([0.0, 3.0], [4.0, 0.0]) AS d, \
                gds.util.isFinite(gds.util.infinity()) AS inf",
    );
    assert_eq!(r[0][0], CypherValue::Float(1.0));
    assert_eq!(r[0][1], CypherValue::Float(0.5));
    assert_eq!(r[0][2], CypherValue::Float(5.0));
    assert_eq!(r[0][3], CypherValue::Bool(false));
    // unknown algorithm: clear error listing the mapped set
    ok(&mut g, "CALL gds.graph.project('g4', '*', '*')");
    let e = run(&mut g, "CALL gds.kmeans.stream('g4')").unwrap_err();
    assert!(e.contains("no Traverse engine mapping"), "got: {e}");
    // missing projection: clear error with the fix
    let e2 = run(&mut g, "CALL gds.pageRank.stream('nope')").unwrap_err();
    assert!(e2.contains("does not exist"), "got: {e2}");
    // mutate stores an overlay on the projection
    let m = ok(
        &mut g,
        "CALL gds.pageRank.mutate('g4', {mutateProperty: 'x'}) \
         YIELD nodePropertiesWritten RETURN nodePropertiesWritten > 0",
    );
    assert_eq!(m[0][0], CypherValue::Bool(true));
    ok(&mut g, "CALL gds.graph.drop('g4')");
}

#[test]
fn batch2_centrality_community() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('b2', 'P', 'R')");
    // scc: two directed triangle cycles = 2 components
    let scc = ok(
        &mut g,
        "CALL gds.scc.stream('b2') YIELD nodeId, componentId RETURN count(DISTINCT componentId)",
    );
    assert_eq!(scc[0][0], CypherValue::Integer(2));
    // triangleCount: every P node is in exactly one triangle
    let tc = ok(
        &mut g,
        "CALL gds.triangleCount.stream('b2') YIELD nodeId, triangleCount \
         RETURN count(nodeId), min(triangleCount), max(triangleCount)",
    );
    assert_eq!(tc[0][0], CypherValue::Integer(6));
    assert_eq!(tc[0][1], CypherValue::Integer(1));
    assert_eq!(tc[0][2], CypherValue::Integer(1));
    // kcore: all nodes of the bridged triangles have core value 2
    let kc = ok(
        &mut g,
        "CALL gds.kcore.stream('b2') YIELD nodeId, coreValue RETURN count(nodeId), max(coreValue)",
    );
    assert_eq!(kc[0][0], CypherValue::Integer(6));
    // hits: value map with auth/hub keys
    let h = ok(
        &mut g,
        "CALL gds.hits.stream('b2') YIELD nodeId, values \
         RETURN count(nodeId), min(values.auth) >= 0.0, min(values.hub) >= 0.0",
    );
    assert_eq!(h[0][0], CypherValue::Integer(6));
    assert_eq!(h[0][1], CypherValue::Bool(true));
    // closeness, harmonic, eigenvector, articleRank each stream 6 rows
    for algo in [
        "closeness",
        "closeness.harmonic",
        "eigenvector",
        "articleRank",
    ] {
        let rows = ok(
            &mut g,
            &format!("CALL gds.{algo}.stream('b2') YIELD nodeId, score RETURN count(nodeId)"),
        );
        assert_eq!(rows[0][0], CypherValue::Integer(6), "{algo}");
    }
    // leiden: assignments for all 6
    let ld = ok(
        &mut g,
        "CALL gds.leiden.stream('b2') YIELD nodeId, communityId RETURN count(nodeId)",
    );
    assert_eq!(ld[0][0], CypherValue::Integer(6));
    ok(&mut g, "CALL gds.graph.drop('b2')");
}

#[test]
fn batch2_paths_and_embeddings() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(
        &mut g,
        "CALL gds.graph.project('b3', 'P', {R: {type: 'R', properties: 'w'}})",
    );
    // aStar falls back to dijkstra without heuristics on non-geo data
    let sp = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (d:P {id: 3}) \
         CALL gds.shortestPath.astar.stream('b3', {sourceNode: a, targetNode: d, relationshipWeightProperty: 'w'}) \
         YIELD totalCost RETURN totalCost",
    );
    assert_eq!(sp[0][0], CypherValue::Float(7.0));
    // bellmanFord: same cost with non-negative weights
    let bf = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (d:P {id: 3}) \
         CALL gds.bellmanFord.stream('b3', {sourceNode: a, targetNode: d, relationshipWeightProperty: 'w'}) \
         YIELD totalCost RETURN totalCost",
    );
    assert_eq!(bf[0][0], CypherValue::Float(7.0));
    // yens top-2: shortest first
    let y = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (d:P {id: 3}) \
         CALL gds.shortestPath.yens.stream('b3', {sourceNode: a, targetNode: d, k: 2, relationshipWeightProperty: 'w'}) \
         YIELD index, totalCost RETURN count(*), min(totalCost)",
    );
    assert_eq!(y[0][1], CypherValue::Float(7.0));
    // bfs from a reaches all 6 P nodes
    let b = ok(
        &mut g,
        "MATCH (a:P {id: 0}) CALL gds.bfs.stream('b3', {sourceNode: a}) \
         YIELD sourceNode, nodeIds RETURN size(nodeIds) >= 1",
    );
    assert_eq!(b[0][0], CypherValue::Bool(true));
    // randomWalk streams walks with nodeIds
    let rw = ok(
        &mut g,
        "CALL gds.randomWalk.stream('b3', {walkLength: 4, walksPerNode: 1}) \
         YIELD nodeIds RETURN count(*) > 0",
    );
    assert_eq!(rw[0][0], CypherValue::Bool(true));
    // node2vec + hashgnn: embeddings for every node
    for algo in ["node2vec", "hashgnn"] {
        let e = ok(
            &mut g,
            &format!(
                "CALL gds.{algo}.stream('b3', {{embeddingDimension: 8}}) \
             YIELD nodeId, embedding RETURN count(nodeId), max(size(embedding))"
            ),
        );
        assert_eq!(e[0][0], CypherValue::Integer(6), "{algo} row count");
        assert_eq!(e[0][1], CypherValue::Integer(8), "{algo} dimension");
    }
    ok(&mut g, "CALL gds.graph.drop('b3')");
}

#[test]
fn batch2_linkprediction_functions() {
    let mut g = Graph::new();
    // the fixture as checked against live GDS
    ok(
        &mut g,
        "CREATE (a:LPX {id: 0, community: 1}), (b:LPX {id: 1, community: 1}), \
                (c:LPX {id: 2, community: 2}), (d:LPX {id: 3, community: 1}) \
         CREATE (a)-[:LR]->(c), (b)-[:LR]->(c), (a)-[:LR]->(d), (b)-[:LR]->(d), (d)-[:LR]->(a)",
    );
    let r = ok(
        &mut g,
        "MATCH (a:LPX {id: 0}), (b:LPX {id: 1}) RETURN \
           gds.linkprediction.commonNeighbors(a, b), \
           gds.linkprediction.totalNeighbors(a, b), \
           round(gds.linkprediction.adamicAdar(a, b) * 10000) / 10000, \
           gds.linkprediction.preferentialAttachment(a, b), \
           round(gds.linkprediction.resourceAllocation(a, b) * 10000) / 10000, \
           gds.linkprediction.sameCommunity(a, b, 'community')",
    );
    // values as live GDS reports them on this fixture
    assert_eq!(r[0][0], CypherValue::Float(2.0));
    assert_eq!(r[0][1], CypherValue::Float(2.0));
    assert_eq!(r[0][2], CypherValue::Float(2.3529));
    assert_eq!(r[0][3], CypherValue::Float(6.0));
    assert_eq!(r[0][4], CypherValue::Float(0.8333));
    assert_eq!(r[0][5], CypherValue::Float(1.0));
}

/// Mixed aggregation keeps the RETURN's column order (Neo4j), not the aggregate's internal
/// keys-then-aggs layout.
#[test]
fn mixed_aggregation_column_order() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:MA {k: 'a', v: 1}), (:MA {k: 'a', v: 2}), (:MA {k: 'b', v: 3})",
    );
    // aggregate first, grouping key second
    let r = traverse_cypher::execute(
        &mut g,
        "MATCH (n:MA) RETURN count(n.v) AS c, n.k AS k ORDER BY k",
    )
    .unwrap();
    assert_eq!(r.columns, vec!["c", "k"], "columns follow RETURN order");
    assert_eq!(r.rows[0][0], CypherValue::Integer(2));
    assert_eq!(r.rows[0][1], CypherValue::String("a".into()));
    // key first works too
    let r2 = traverse_cypher::execute(
        &mut g,
        "MATCH (n:MA) RETURN n.k AS k, count(n.v) AS c ORDER BY k",
    )
    .unwrap();
    assert_eq!(r2.columns, vec!["k", "c"]);
    assert_eq!(r2.rows[0][1], CypherValue::Integer(2));
    // interleaved: agg, key, agg
    let r3 = traverse_cypher::execute(
        &mut g,
        "MATCH (n:MA) RETURN sum(n.v) AS s, n.k AS k, count(*) AS c ORDER BY k",
    )
    .unwrap();
    assert_eq!(r3.columns, vec!["s", "k", "c"]);
    assert_eq!(r3.rows[0][0], CypherValue::Integer(3));
    assert_eq!(r3.rows[0][2], CypherValue::Integer(2));
}

/// Materialized projections are SNAPSHOTS (GDS): writes after project() are invisible to algorithms
/// until re-projection.
#[test]
fn materialized_projection_is_a_snapshot() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('snap', 'P', 'R')");
    let before = ok(
        &mut g,
        "CALL gds.wcc.stream('snap') YIELD nodeId RETURN count(nodeId)",
    );
    assert_eq!(before[0][0], CypherValue::Integer(6));
    // new :P node + edge AFTER projecting
    ok(&mut g, "MATCH (a:P {id: 0}) CREATE (a)-[:R]->(:P {id: 99})");
    let after = ok(
        &mut g,
        "CALL gds.wcc.stream('snap') YIELD nodeId RETURN count(nodeId)",
    );
    assert_eq!(
        after[0][0],
        CypherValue::Integer(6),
        "snapshot: new node invisible"
    );
    // re-projecting picks it up
    ok(&mut g, "CALL gds.graph.drop('snap')");
    ok(&mut g, "CALL gds.graph.project('snap', 'P', 'R')");
    let re = ok(
        &mut g,
        "CALL gds.wcc.stream('snap') YIELD nodeId RETURN count(nodeId)",
    );
    assert_eq!(re[0][0], CypherValue::Integer(7));
    ok(&mut g, "CALL gds.graph.drop('snap')");
}

/// Dense <-> original id translation: projected nodes not starting at id 0 must still report ORIGINAL
/// ids (and configs accept them).
#[test]
fn materialized_id_remapping() {
    let mut g = Graph::new();
    // junk nodes first to offset the id space
    ok(&mut g, "CREATE (:Junk), (:Junk), (:Junk), (:Junk), (:Junk)");
    fixture(&mut g); // :P ids are 5..12 internally now
    ok(
        &mut g,
        "CALL gds.graph.project('remap', 'P', {R: {type: 'R', properties: 'w'}})",
    );
    // results carry original ids: asNode + property access work
    let deg = ok(
        &mut g,
        "CALL gds.degree.stream('remap') YIELD nodeId, score \
         RETURN gds.util.asNode(nodeId).id AS id, score ORDER BY score DESC, id LIMIT 1",
    );
    assert_eq!(
        deg[0][0],
        CypherValue::Integer(2),
        "node c by its property id"
    );
    assert_eq!(deg[0][1], CypherValue::Float(2.0));
    // config takes original node values; totalCost over baked weights
    let sp = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (d:P {id: 3}) \
         CALL gds.shortestPath.dijkstra.stream('remap', \
              {sourceNode: a, targetNode: d, relationshipWeightProperty: 'w'}) \
         YIELD sourceNode, targetNode, totalCost RETURN totalCost, sourceNode = id(a), targetNode = id(d)",
    );
    assert_eq!(sp[0][0], CypherValue::Float(7.0));
    assert_eq!(
        sp[0][1],
        CypherValue::Bool(true),
        "sourceNode remapped to original id"
    );
    assert_eq!(sp[0][2], CypherValue::Bool(true));
    // a node OUTSIDE the projection errors clearly
    let e = run(
        &mut g,
        "MATCH (j:Junk) WITH j LIMIT 1 \
         CALL gds.shortestPath.dijkstra.stream('remap', {sourceNode: j, targetNode: j}) \
         YIELD totalCost RETURN totalCost",
    )
    .unwrap_err();
    assert!(e.contains("not in the projected graph"), "got: {e}");
    ok(&mut g, "CALL gds.graph.drop('remap')");
}

/// mutate mode: results stored as projection overlays, streamable and flushable to the database.
#[test]
fn mutate_mode_overlay_lifecycle() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('mut', 'P', 'R')");
    // pageRank.mutate stores the score overlay
    let m = ok(
        &mut g,
        "CALL gds.pageRank.mutate('mut', {mutateProperty: 'pr'}) \
         YIELD nodePropertiesWritten, nodeCount RETURN nodePropertiesWritten, nodeCount",
    );
    assert_eq!(m[0][0], CypherValue::Integer(6));
    assert_eq!(m[0][1], CypherValue::Integer(6));
    // stream the overlay: original ids + values
    let s = ok(
        &mut g,
        "CALL gds.graph.nodeProperties.stream('mut', ['pr']) \
         YIELD nodeId, nodeProperty, propertyValue \
         RETURN count(nodeId), min(propertyValue) > 0.0",
    );
    assert_eq!(s[0][0], CypherValue::Integer(6));
    assert_eq!(s[0][1], CypherValue::Bool(true));
    // the DATABASE is untouched until write
    assert_eq!(
        one(&mut g, "MATCH (n:P) WHERE n.pr IS NOT NULL RETURN count(n)"),
        CypherValue::Integer(0)
    );
    // wcc.mutate too (community kind)
    ok(
        &mut g,
        "CALL gds.wcc.mutate('mut', {mutateProperty: 'comp'}) YIELD nodePropertiesWritten RETURN 1",
    );
    // flush both to the database
    let w = ok(
        &mut g,
        "CALL gds.graph.nodeProperties.write('mut', ['pr', 'comp']) \
         YIELD propertiesWritten, nodeProperties RETURN propertiesWritten, size(nodeProperties)",
    );
    assert_eq!(w[0][0], CypherValue::Integer(12));
    assert_eq!(w[0][1], CypherValue::Integer(2));
    assert_eq!(
        one(
            &mut g,
            "MATCH (n:P) WHERE n.pr IS NOT NULL AND n.comp IS NOT NULL RETURN count(n)"
        ),
        CypherValue::Integer(6)
    );
    // drop removes the overlay; streaming it afterwards errors
    let d = ok(
        &mut g,
        "CALL gds.graph.nodeProperties.drop('mut', ['pr']) YIELD propertiesRemoved RETURN propertiesRemoved",
    );
    assert_eq!(d[0][0], CypherValue::Integer(6));
    let e = run(
        &mut g,
        "CALL gds.graph.nodeProperties.stream('mut', ['pr']) YIELD nodeId RETURN nodeId",
    )
    .unwrap_err();
    assert!(e.contains("does not exist on graph"), "got: {e}");
    // missing mutateProperty and non-mutatable kinds error clearly
    let e2 = run(&mut g, "CALL gds.pageRank.mutate('mut', {})").unwrap_err();
    assert!(e2.contains("mutateProperty is required"), "got: {e2}");
    ok(&mut g, "CALL gds.graph.drop('mut')");
}

/// fastRP.mutate with offset ids: embeddings stream back with ORIGINAL ids.
#[test]
fn mutate_embeddings_with_offset_ids() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:Junk), (:Junk), (:Junk)");
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('emb', 'P', 'R')");
    ok(
        &mut g,
        "CALL gds.fastRP.mutate('emb', {mutateProperty: 'v', embeddingDimension: 8}) \
         YIELD nodePropertiesWritten RETURN nodePropertiesWritten",
    );
    let s = ok(
        &mut g,
        "CALL gds.graph.nodeProperties.stream('emb', 'v') \
         YIELD nodeId, propertyValue \
         RETURN count(nodeId), min(size(propertyValue)), min(gds.util.asNode(nodeId).id) >= 0",
    );
    assert_eq!(s[0][0], CypherValue::Integer(6));
    assert_eq!(s[0][1], CypherValue::Integer(8));
    assert_eq!(
        s[0][2],
        CypherValue::Bool(true),
        "ids remap to originals (asNode resolves)"
    );
    ok(&mut g, "CALL gds.graph.drop('emb')");
}

/// The mutate chain: fastRP.mutate stores embeddings on the projection, and gds.knn with
/// nodeProperties reads them through the property bridge, no database write in between.
#[test]
fn mutate_chain_fastrp_to_knn() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:Junk), (:Junk)"); // offset ids to exercise remapping
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('chain', 'P', 'R')");
    ok(
        &mut g,
        "CALL gds.fastRP.mutate('chain', {mutateProperty: 'emb', embeddingDimension: 16}) \
         YIELD nodePropertiesWritten RETURN nodePropertiesWritten",
    );
    // database untouched
    assert_eq!(
        one(
            &mut g,
            "MATCH (n:P) WHERE n.emb IS NOT NULL RETURN count(n)"
        ),
        CypherValue::Integer(0)
    );
    // knn over the mutated embeddings (property bridge)
    let rows = ok(
        &mut g,
        "CALL gds.knn.stream('chain', {nodeProperties: ['emb'], topK: 2, similarityCutoff: -1.0}) \
         YIELD node1, node2, similarity \
         RETURN count(*), count(DISTINCT node1), min(gds.util.asNode(node1).id) >= 0",
    );
    assert_eq!(
        rows[0][1],
        CypherValue::Integer(6),
        "every projected node got neighbors"
    );
    assert_eq!(
        rows[0][2],
        CypherValue::Bool(true),
        "ids are original (asNode resolves)"
    );
    let CypherValue::Integer(pairs) = rows[0][0] else {
        panic!()
    };
    assert!(pairs >= 6, "at least one pair per node: {pairs}");
    // second hop: wcc.mutate, then stream both overlays
    ok(
        &mut g,
        "CALL gds.wcc.mutate('chain', {mutateProperty: 'comp'}) YIELD nodePropertiesWritten RETURN 1",
    );
    let both = ok(
        &mut g,
        "CALL gds.graph.nodeProperties.stream('chain', ['emb', 'comp']) \
         YIELD nodeId, nodeProperty RETURN count(*), count(DISTINCT nodeProperty)",
    );
    assert_eq!(both[0][0], CypherValue::Integer(12));
    assert_eq!(both[0][1], CypherValue::Integer(2));
    ok(&mut g, "CALL gds.graph.drop('chain')");
}

/// GDS Cypher projections: gds.graph.project.cypher and the anonymous {nodeQuery,
/// relationshipQuery} config form.
#[test]
fn cypher_projection_named_and_anonymous() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:V {id: 0}), (b:V {id: 1}), (c:V {id: 2}) \
         CREATE (a)-[:R]->(b), (b)-[:R]->(c), (a)-[:R]->(c)",
    );
    // named: aggregated weight from the relationship query
    let r = ok(
        &mut g,
        "CALL gds.graph.project.cypher('cp', \
           'MATCH (v:V) RETURN id(v) AS id', \
           'MATCH (x:V)-[r:R]->(y:V) RETURN id(x) AS source, id(y) AS target, 2.0/count(r) AS weight') \
         YIELD graphName, nodeCount, relationshipCount \
         RETURN graphName, nodeCount, relationshipCount",
    );
    assert_eq!(r[0][1], CypherValue::Integer(3));
    assert_eq!(r[0][2], CypherValue::Integer(3));
    let sp = ok(
        &mut g,
        "MATCH (a:V {id: 0}), (c:V {id: 2}) \
         CALL gds.shortestPath.dijkstra.stream('cp', \
              {sourceNode: a, targetNode: c, relationshipWeightProperty: 'weight'}) \
         YIELD totalCost RETURN totalCost",
    );
    assert_eq!(
        sp[0][0],
        CypherValue::Float(2.0),
        "direct a->c edge, weight 2.0"
    );
    ok(&mut g, "CALL gds.graph.drop('cp')");
    // anonymous: config-only call with inline queries
    let anon = ok(
        &mut g,
        "MATCH (a:V {id: 0}), (c:V {id: 2}) \
         CALL gds.shortestPath.dijkstra.stream({ \
           nodeQuery: 'MATCH (v:V) RETURN id(v) AS id', \
           relationshipQuery: 'MATCH (x:V)-[r:R]->(y:V) RETURN id(x) AS source, id(y) AS target, 1.0 AS weight', \
           sourceNode: a, targetNode: c, relationshipWeightProperty: 'weight'}) \
         YIELD totalCost RETURN totalCost",
    );
    assert_eq!(anon[0][0], CypherValue::Float(1.0));
    // wcc over an anonymous cypher projection (no weights)
    let w = ok(
        &mut g,
        "CALL gds.wcc.stream({ \
           nodeQuery: 'MATCH (v:V) RETURN id(v) AS id', \
           relationshipQuery: 'MATCH (x:V)-[:R]->(y:V) RETURN id(x) AS source, id(y) AS target'}) \
         YIELD nodeId RETURN count(nodeId)",
    );
    assert_eq!(w[0][0], CypherValue::Integer(3));
}

/// GDS dijkstra `path` yield: a real Path of full nodes joined by virtual PATH_0 relationships with
/// per-segment cost deltas.
#[test]
fn dijkstra_path_yield() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:V {id: 0}), (b:V {id: 1}), (c:V {id: 2}) \
         CREATE (a)-[:R {w: 3.0}]->(b), (b)-[:R {w: 4.0}]->(c)",
    );
    ok(
        &mut g,
        "CALL gds.graph.project('pp', 'V', {R: {type: 'R', properties: 'w'}})",
    );
    let rows = ok(
        &mut g,
        "MATCH (a:V {id: 0}), (c:V {id: 2}) \
         CALL gds.shortestPath.dijkstra.stream('pp', \
              {sourceNode: a, targetNode: c, relationshipWeightProperty: 'w'}) \
         YIELD path \
         RETURN size(nodes(path)), size(relationships(path)), \
                [r IN relationships(path) | type(r)], \
                [r IN relationships(path) | r.cost], \
                [n IN nodes(path) | n.id]",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(3));
    assert_eq!(rows[0][1], CypherValue::Integer(2));
    assert_eq!(
        rows[0][2],
        CypherValue::List(vec![
            CypherValue::String("PATH_0".into()),
            CypherValue::String("PATH_0".into())
        ])
    );
    assert_eq!(
        rows[0][3],
        CypherValue::List(vec![CypherValue::Float(3.0), CypherValue::Float(4.0)])
    );
    assert_eq!(
        rows[0][4],
        CypherValue::List(vec![
            CypherValue::Integer(0),
            CypherValue::Integer(1),
            CypherValue::Integer(2)
        ])
    );
    ok(&mut g, "CALL gds.graph.drop('pp')");
}

/// louvain.stats yields modularity + ranLevels (GDS shape); two triangles + bridge = modularity
/// 5/14 ~ 0.3571.
#[test]
fn louvain_stats_modularity() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:V),(b:V),(c:V),(d:V),(e:V),(f:V) \
         CREATE (a)-[:R]->(b),(b)-[:R]->(c),(c)-[:R]->(a), \
                (d)-[:R]->(e),(e)-[:R]->(f),(f)-[:R]->(d),(c)-[:R]->(d)",
    );
    ok(&mut g, "CALL gds.graph.project('lm', 'V', 'R')");
    let r = ok(
        &mut g,
        "CALL gds.louvain.stats('lm') YIELD communityCount, modularity, ranLevels \
         RETURN communityCount, modularity, ranLevels",
    );
    assert_eq!(r[0][0], CypherValue::Integer(2));
    let CypherValue::Float(q) = r[0][1] else {
        panic!("modularity missing")
    };
    assert!((q - 5.0 / 14.0).abs() < 1e-9, "q = {q}");
    assert!(matches!(r[0][2], CypherValue::Integer(l) if l >= 1));
    ok(&mut g, "CALL gds.graph.drop('lm')");
}

/// Algorithm write mode goes through the same path as `SET`: a property index on the written
/// property sees the new values.
#[test]
fn write_mode_maintains_property_indexes() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CREATE INDEX pr_idx FOR (n:P) ON (n.pr)");
    ok(&mut g, "CREATE INDEX comp_idx FOR (n:P) ON (n.comp)");
    ok(
        &mut g,
        "CALL traverse.pageRank.write({writeProperty: 'pr'})",
    );
    ok(&mut g, "CALL traverse.wcc.write({writeProperty: 'comp'})");
    assert_eq!(
        one(&mut g, "MATCH (n:P) WHERE n.pr > 0.0 RETURN count(n)"),
        CypherValue::Integer(6)
    );
    let comp = one(&mut g, "MATCH (n:P {id: 0}) RETURN n.comp");
    assert_eq!(
        one(
            &mut g,
            &format!(
                "MATCH (n:P) WHERE n.comp = {} RETURN count(n)",
                match comp {
                    CypherValue::Integer(i) => i,
                    other => panic!("{other:?}"),
                }
            )
        ),
        CypherValue::Integer(6)
    );
}

/// GDS single-source shortest paths (delta and dijkstra) stream the source itself at cost 0 and a
/// full path to every reachable node, weighted or not; stats counts the same paths.
#[test]
fn single_source_paths_start_at_the_source() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(
        &mut g,
        "CALL gds.graph.project('sssp', 'P', {R: {type: 'R', properties: 'w'}})",
    );
    for algo in ["delta", "dijkstra"] {
        for (weight, far) in [("", 3.0), (", relationshipWeightProperty: 'w'", 7.0)] {
            let rows = ok(
                &mut g,
                &format!(
                    "MATCH (s:P {{id: 0}}) \
                     CALL gds.allShortestPaths.{algo}.stream('sssp', {{sourceNode: s{weight}}}) \
                     YIELD targetNode, totalCost, nodeIds, costs \
                     RETURN gds.util.asNode(targetNode).id AS t, totalCost, \
                            [n IN nodeIds | gds.util.asNode(n).id] AS ids, costs ORDER BY t"
                ),
            );
            let q = format!("{algo}{weight}");
            assert_eq!(rows.len(), 6, "{q}");
            assert_eq!(rows[0][0], CypherValue::Integer(0), "{q}: the source row");
            assert_eq!(rows[0][1], CypherValue::Float(0.0), "{q}");
            assert_eq!(
                rows[0][2],
                CypherValue::List(vec![CypherValue::Integer(0)]),
                "{q}"
            );
            // d(3) is reached through a -> b -> c -> d
            assert_eq!(rows[3][1], CypherValue::Float(far), "{q}");
            assert_eq!(
                rows[3][2],
                CypherValue::List((0..4).map(CypherValue::Integer).collect()),
                "{q}"
            );
            let CypherValue::List(costs) = &rows[3][3] else {
                panic!("{q}: costs {:?}", rows[3][3]);
            };
            assert_eq!(costs.len(), 4, "{q}");
            assert_eq!(costs[3], CypherValue::Float(far), "{q}");
        }
    }
    // a source with nothing reachable still streams itself
    ok(&mut g, "CREATE (:P {id: 9})");
    ok(&mut g, "CALL gds.graph.project('sink', 'P', 'R')");
    let alone = ok(
        &mut g,
        "MATCH (s:P {id: 9}) CALL gds.allShortestPaths.dijkstra.stream('sink', {sourceNode: s}) \
         YIELD totalCost, nodeIds RETURN totalCost, size(nodeIds)",
    );
    assert_eq!(
        alone,
        [[CypherValue::Float(0.0), CypherValue::Integer(1)]],
        "the source alone"
    );
    ok(&mut g, "CALL gds.graph.drop('sink')");
    let count = one(
        &mut g,
        "MATCH (s:P {id: 0}) \
         CALL traverse.deltaStepping.stats({sourceNodeId: id(s)}) YIELD pathCount \
         RETURN pathCount",
    );
    assert_eq!(
        count,
        CypherValue::Integer(5),
        "stats leaves the source out"
    );
    // a weight property the projection doesn't hold is an error, as in GDS
    let err = run(
        &mut g,
        "MATCH (s:P {id: 0}) CALL gds.allShortestPaths.dijkstra.stream('sssp', \
         {sourceNode: s, relationshipWeightProperty: 'nope'}) YIELD totalCost RETURN totalCost",
    )
    .unwrap_err();
    assert!(err.to_string().contains("`nope` not found"), "{err}");
    ok(&mut g, "CALL gds.graph.drop('sssp')");
}

/// GDS `scaler` on the centralities: names case-insensitive, NONE leaves the scores, an unknown
/// name is an error.
#[test]
fn centrality_scalers() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(&mut g, "CALL gds.graph.project('sc', 'P', 'R')");
    let scores = |g: &mut Graph, algo: &str, scaler: &str| -> Vec<f64> {
        ok(
            g,
            &format!(
                "CALL gds.{algo}.stream('sc', {{scaler: '{scaler}'}}) YIELD nodeId, score \
                 RETURN score ORDER BY gds.util.asNode(nodeId).id"
            ),
        )
        .into_iter()
        .map(|r| match r[0] {
            CypherValue::Float(f) => f,
            ref v => panic!("{algo} {scaler}: {v:?}"),
        })
        .collect()
    };
    for algo in ["pageRank", "articleRank"] {
        let raw = scores(&mut g, algo, "NONE");
        let sum: f64 = raw.iter().sum();
        let l1 = scores(&mut g, algo, "l1Norm");
        for (r, s) in raw.iter().zip(&l1) {
            assert!((r / sum - s).abs() < 1e-12, "{algo}: {raw:?} -> {l1:?}");
        }
        let max = raw.iter().cloned().fold(f64::MIN, f64::max);
        let min = raw.iter().cloned().fold(f64::MAX, f64::min);
        let mm = scores(&mut g, algo, "MinMax");
        for (r, s) in raw.iter().zip(&mm) {
            assert!(
                ((r - min) / (max - min) - s).abs() < 1e-12,
                "{algo}: {mm:?}"
            );
        }
    }
    let err = run(
        &mut g,
        "CALL gds.pageRank.stream('sc', {scaler: 'bogus'}) YIELD score RETURN score",
    )
    .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("bogus"), "{err}");
    ok(&mut g, "CALL gds.graph.drop('sc')");
}

/// An UNDIRECTED projection lists every relationship at both ends, as GDS does: the values below
/// are Neo4j GDS 2026.03.0's on the same graph (two triangles joined by c -> d).
#[test]
fn undirected_projection_reads_both_directions() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(
        &mut g,
        "CALL gds.graph.project('u', 'P', {R: {orientation: 'UNDIRECTED'}})",
    );
    let col = |g: &mut Graph, call: &str, yield_: &str| -> Vec<f64> {
        ok(
            g,
            &format!(
                "CALL gds.{call} YIELD nodeId, {yield_} AS v \
                 RETURN round(toFloat(v) * 10000) / 10000 ORDER BY gds.util.asNode(nodeId).id"
            ),
        )
        .into_iter()
        .map(|r| match r[0] {
            CypherValue::Float(f) => f,
            ref v => panic!("{call}: {v:?}"),
        })
        .collect()
    };
    let (rim, hub) = (0.3536, 0.5);
    assert_eq!(
        col(&mut g, "eigenvector.stream('u')", "score"),
        [rim, rim, hub, hub, rim, rim]
    );
    assert_eq!(
        col(&mut g, "degree.stream('u')", "score"),
        [2.0, 2.0, 3.0, 3.0, 2.0, 2.0]
    );
    assert_eq!(
        col(&mut g, "betweenness.stream('u')", "score"),
        [0.0, 0.0, 6.0, 6.0, 0.0, 0.0]
    );
    assert_eq!(
        col(&mut g, "pageRank.stream('u')", "score"),
        [0.8427, 0.8427, 1.1983, 1.1983, 0.8427, 0.8427]
    );
    assert_eq!(
        col(&mut g, "articleRank.stream('u')", "score"),
        [0.2446, 0.2446, 0.2926, 0.2926, 0.2446, 0.2446]
    );
    assert_eq!(
        col(&mut g, "kcore.stream('u')", "coreValue"),
        [2.0, 2.0, 2.0, 2.0, 2.0, 2.0]
    );
    // GDS counts an undirected relationship once per direction
    assert_eq!(
        one(
            &mut g,
            "CALL gds.graph.list('u') YIELD relationshipCount RETURN relationshipCount"
        ),
        CypherValue::Integer(14)
    );
    // reached against the stored direction: f(5) only points at d(3)
    let sp = ok(
        &mut g,
        "MATCH (a:P {id: 0}), (f:P {id: 5}) \
         CALL gds.shortestPath.dijkstra.stream('u', {sourceNode: a, targetNode: f}) \
         YIELD totalCost, nodeIds RETURN totalCost, [n IN nodeIds | gds.util.asNode(n).id]",
    );
    assert_eq!(sp[0][0], CypherValue::Float(3.0));
    // write mode runs on the live graph, undirected there too
    ok(&mut g, "CALL gds.degree.write('u', {writeProperty: 'deg'})");
    assert_eq!(
        one(&mut g, "MATCH (n:P {id: 2}) RETURN n.deg"),
        CypherValue::Float(3.0)
    );
    ok(&mut g, "CALL gds.graph.drop('u')");

    // a self-loop lists twice at its node: GDS's degree counts it 2
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:Z {id: 0}), (b:Z {id: 1}), (c:Z {id: 2}) \
         CREATE (a)-[:L]->(a), (a)-[:L]->(b), (b)-[:L]->(a), (b)-[:L]->(c)",
    );
    ok(
        &mut g,
        "CALL gds.graph.project('z', 'Z', {L: {orientation: 'UNDIRECTED'}})",
    );
    assert_eq!(col(&mut g, "degree.stream('z')", "score"), [4.0, 3.0, 1.0]);
    assert_eq!(
        col(&mut g, "kcore.stream('z')", "coreValue"),
        [2.0, 2.0, 1.0]
    );
}

/// Path traversals stream GDS's shapes: bfs / dfs one row (visit order, a NEXT path), Bellman-Ford
/// the source row, a route and isNegativeCycle false; articleRank and pageRank.mutate GDS's own
/// scale.
#[test]
fn traversal_rows_and_rank_scales_as_in_gds() {
    let mut g = Graph::new();
    fixture(&mut g);
    ok(
        &mut g,
        "CALL gds.graph.project('t', 'P', {R: {type: 'R', properties: 'w'}})",
    );
    let bfs = ok(
        &mut g,
        "MATCH (s:P {id: 3}) CALL gds.bfs.stream('t', {sourceNode: s}) YIELD nodeIds, path \
         RETURN [n IN nodeIds | gds.util.asNode(n).id], [r IN relationships(path) | type(r)]",
    );
    assert_eq!(bfs.len(), 1, "one row per traversal");
    assert_eq!(
        bfs[0][0],
        CypherValue::List((3..6).map(CypherValue::Integer).collect())
    );
    assert_eq!(
        bfs[0][1],
        CypherValue::List(vec![CypherValue::String("NEXT".into()); 2])
    );
    let dfs = ok(
        &mut g,
        "MATCH (s:P {id: 0}) CALL gds.dfs.stream('t', {sourceNode: s}) YIELD nodeIds \
         RETURN [n IN nodeIds | gds.util.asNode(n).id]",
    );
    assert_eq!(
        dfs,
        [[CypherValue::List(
            (0..6).map(CypherValue::Integer).collect()
        )]]
    );
    let bf = ok(
        &mut g,
        "MATCH (s:P {id: 0}) CALL gds.bellmanFord.stream('t', \
              {sourceNode: s, relationshipWeightProperty: 'w'}) \
         YIELD targetNode, totalCost, route, isNegativeCycle \
         RETURN gds.util.asNode(targetNode).id AS t, totalCost, length(route), isNegativeCycle \
         ORDER BY t",
    );
    assert_eq!(bf.len(), 6);
    assert_eq!(
        bf[0],
        [
            CypherValue::Integer(0),
            CypherValue::Float(0.0),
            CypherValue::Integer(0),
            CypherValue::Bool(false)
        ]
    );
    assert_eq!(bf[5][1], CypherValue::Float(9.0));
    let ar = one(
        &mut g,
        "MATCH (n:P {id: 0}) CALL gds.articleRank.stream('t') YIELD nodeId, score \
         WHERE nodeId = id(n) RETURN round(score * 10000) / 10000",
    );
    assert_eq!(ar, CypherValue::Float(0.2149));
    ok(
        &mut g,
        "CALL gds.pageRank.mutate('t', {mutateProperty: 'pr'}) YIELD nodePropertiesWritten",
    );
    let pr = one(
        &mut g,
        "MATCH (n:P {id: 3}) CALL gds.graph.nodeProperty.stream('t', 'pr') \
         YIELD nodeId, propertyValue WHERE nodeId = id(n) RETURN round(propertyValue * 1000) / 1000",
    );
    assert_eq!(pr, CypherValue::Float(1.536));
}
