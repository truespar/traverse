use std::sync::atomic::{AtomicI64, Ordering};

/// Parameter kind for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Vertex,
    VertexPair,
    Create,
    None,
}

/// A benchmark query definition.
#[derive(Debug, Clone)]
pub struct QueryDef {
    pub name: &'static str,
    pub cypher: &'static str,
    pub params: ParamKind,
    pub group: &'static str,
}

/// All 26 Pokec benchmark queries.
pub static QUERIES: &[QueryDef] = &[
    // -- Read --
    QueryDef {
        name: "single_vertex_read",
        cypher: "MATCH (n:User {id: $id}) RETURN n",
        params: ParamKind::Vertex,
        group: "read",
    },
    QueryDef {
        name: "single_vertex_property",
        cypher: "MATCH (n:User {id: $id}) RETURN n.age, n.name",
        params: ParamKind::Vertex,
        group: "read",
    },
    QueryDef {
        name: "pattern_short",
        cypher: "MATCH (n:User {id: $id})-[e]->(m) RETURN m LIMIT 1",
        params: ParamKind::Vertex,
        group: "read",
    },
    QueryDef {
        name: "pattern_cycle",
        cypher: "MATCH (n:User {id: $id})-[e1]->(m)-[e2]->(n) RETURN e1, m, e2",
        params: ParamKind::Vertex,
        group: "read",
    },
    QueryDef {
        name: "pattern_long",
        cypher: "MATCH (n1:User {id: $id})-[e1]->(n2)-[e2]->(n3)-[e3]->(n4)<-[e4]-(n5) RETURN n5 LIMIT 1",
        params: ParamKind::Vertex,
        group: "read",
    },
    // -- Expansion --
    QueryDef {
        name: "expansion_1",
        cypher: "MATCH (s:User {id: $id})-->(n:User) RETURN n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_1_with_filter",
        cypher: "MATCH (s:User {id: $id})-->(n:User) WHERE n.age >= 18 RETURN n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_2",
        cypher: "MATCH (s:User {id: $id})-->()-->(n:User) RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_2_with_filter",
        cypher: "MATCH (s:User {id: $id})-->()-->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_3",
        cypher: "MATCH (s:User {id: $id})-->()-->()-->(n:User) RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_3_with_filter",
        cypher: "MATCH (s:User {id: $id})-->()-->()-->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_4",
        cypher: "MATCH (s:User {id: $id})-->()-->()-->()-->(n:User) RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "expansion_4_with_filter",
        cypher: "MATCH (s:User {id: $id})-->()-->()-->()-->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "neighbours_2",
        cypher: "MATCH (s:User {id: $id})-[*1..2]->(n:User) RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "neighbours_2_with_filter",
        cypher: "MATCH (s:User {id: $id})-[*1..2]->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "neighbours_2_with_data",
        cypher: "MATCH (s:User {id: $id})-[*1..2]->(n:User) RETURN DISTINCT n.id, n.age, n.name",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    QueryDef {
        name: "neighbours_2_with_data_and_filter",
        cypher: "MATCH (s:User {id: $id})-[*1..2]->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id, n",
        params: ParamKind::Vertex,
        group: "expansion",
    },
    // -- Scan (full-scan queries that exercise morsel parallelism) --
    QueryDef {
        name: "scan_expand",
        cypher: "MATCH (n:User)-[:Friend]->(m:User) RETURN n.id, m.id",
        params: ParamKind::None,
        group: "scan",
    },
    QueryDef {
        name: "scan_expand_filter",
        cypher: "MATCH (n:User)-[:Friend]->(m:User) WHERE m.age > 50 RETURN n.id, m.id",
        params: ParamKind::None,
        group: "scan",
    },
    QueryDef {
        name: "scan_expand_2hop",
        cypher: "MATCH (n:User)-[:Friend]->()-[:Friend]->(m:User) RETURN DISTINCT m.id",
        params: ParamKind::None,
        group: "scan",
    },
    QueryDef {
        name: "scan_expand_2hop_filter",
        cypher: "MATCH (n:User)-[:Friend]->()-[:Friend]->(m:User) WHERE m.age > 50 RETURN DISTINCT m.id",
        params: ParamKind::None,
        group: "scan",
    },
    QueryDef {
        name: "scan_project",
        cypher: "MATCH (n:User) RETURN n.age, n.name",
        params: ParamKind::None,
        group: "scan",
    },
    QueryDef {
        name: "scan_agg_global",
        cypher: "MATCH (n:User)-[:Friend]->()-[:Friend]->(m:User) RETURN count(m)",
        params: ParamKind::None,
        group: "scan",
    },
    QueryDef {
        name: "scan_agg_grouped",
        cypher: "MATCH (n:User)-[:Friend]->()-[:Friend]->(m:User) RETURN m.age, count(*)",
        params: ParamKind::None,
        group: "scan",
    },
    // -- Shortest path --
    QueryDef {
        name: "shortest_path",
        cypher: "MATCH (n:User {id: $from}), (m:User {id: $to}) WITH n, m MATCH p = shortestPath((n)-[*..15]->(m)) RETURN [node IN nodes(p) | node.id] AS path",
        params: ParamKind::VertexPair,
        group: "shortest_path",
    },
    QueryDef {
        name: "all_shortest_paths",
        cypher: "MATCH (n:User {id: $from}), (m:User {id: $to}) WITH n, m MATCH p = allShortestPaths((n)-[*..2]->(m)) RETURN [node IN nodes(p) | node.id] AS path",
        params: ParamKind::VertexPair,
        group: "shortest_path",
    },
    // -- Write --
    QueryDef {
        name: "vertex_create",
        cypher: "CREATE (n:UserTemp {id: $id}) RETURN n",
        params: ParamKind::Create,
        group: "write",
    },
    QueryDef {
        name: "edge_create",
        cypher: "MATCH (a:User {id: $from}), (b:User {id: $to}) CREATE (a)-[:TempEdge]->(b)",
        params: ParamKind::VertexPair,
        group: "write",
    },
    // -- Update --
    QueryDef {
        name: "vertex_update",
        cypher: "MATCH (n:User {id: $id}) SET n.property = -1",
        params: ParamKind::Vertex,
        group: "update",
    },
    // -- Aggregate --
    QueryDef {
        name: "count",
        cypher: "MATCH (n) RETURN count(n), count(n.age)",
        params: ParamKind::None,
        group: "aggregate",
    },
    QueryDef {
        name: "min_max_avg",
        cypher: "MATCH (n) RETURN min(n.age), max(n.age), avg(n.age)",
        params: ParamKind::None,
        group: "aggregate",
    },
    QueryDef {
        name: "group_by_age",
        cypher: "MATCH (n:User) RETURN n.age, count(*)",
        params: ParamKind::None,
        group: "aggregate",
    },
    QueryDef {
        name: "aggregate_with_filter",
        cypher: "MATCH (n:User) WHERE n.age >= 18 RETURN n.age, count(*)",
        params: ParamKind::None,
        group: "aggregate",
    },
    // -- GDS --
    QueryDef {
        name: "gds_pagerank",
        cypher: "CALL traverse.pageRank.stats({maxIterations: 20, tolerance: 1e-7}) \
                 YIELD nodeCount, mean, computeMillis \
                 RETURN nodeCount, mean, computeMillis",
        params: ParamKind::None,
        group: "gds",
    },
    QueryDef {
        name: "gds_wcc",
        cypher: "CALL traverse.wcc.stats({}) YIELD nodeCount, communityCount, largestCommunitySize \
                 RETURN nodeCount, communityCount, largestCommunitySize",
        params: ParamKind::None,
        group: "gds",
    },
    QueryDef {
        name: "gds_triangle_count",
        cypher: "CALL traverse.triangleCount.stats({}) YIELD nodeCount, fieldCount, computeMillis \
                 RETURN nodeCount, fieldCount, computeMillis",
        params: ParamKind::None,
        group: "gds",
    },
    QueryDef {
        name: "gds_louvain",
        cypher: "CALL traverse.louvain.stats({maxLevels: 5, maxIterations: 10}) \
                 YIELD nodeCount, communityCount \
                 RETURN nodeCount, communityCount",
        params: ParamKind::None,
        group: "gds",
    },
];

/// Query group ordering (matches Python default order).
pub static GROUP_ORDER: &[&str] = &[
    "read",
    "expansion",
    "scan",
    "aggregate",
    "shortest_path",
    "update",
    "write",
    "gds",
];

/// Resolve a comma-separated groups string to a list of query definitions.
/// If `groups` is None or "all", returns all queries in default group order.
pub fn resolve_queries(groups: Option<&str>) -> Vec<&'static QueryDef> {
    match groups {
        None | Some("all") | Some("") => {
            let mut result = Vec::new();
            for &group in GROUP_ORDER {
                for q in QUERIES {
                    if q.group == group {
                        result.push(q);
                    }
                }
            }
            result
        }
        Some(s) => {
            let mut result = Vec::new();
            for token in s.split(',') {
                let token = token.trim();
                // Check if it's a group name
                let is_group = GROUP_ORDER.contains(&token);
                if is_group {
                    for q in QUERIES {
                        if q.group == token {
                            result.push(q);
                        }
                    }
                } else {
                    // Try as individual query name
                    if let Some(q) = QUERIES.iter().find(|q| q.name == token) {
                        result.push(q);
                    } else {
                        eprintln!("Warning: unknown group/query '{token}', skipping");
                    }
                }
            }
            result
        }
    }
}

/// Shared atomic counter for ParamKind::Create.
static CREATE_COUNTER: AtomicI64 = AtomicI64::new(1_000_000);

/// Generate query parameters based on the param kind.
pub fn generate_params(
    kind: ParamKind,
    vertex_count: i64,
    rng: &mut impl rand::RngExt,
) -> Vec<(String, i64)> {
    match kind {
        ParamKind::Vertex => {
            vec![("id".into(), rng.random_range(1..=vertex_count))]
        }
        ParamKind::VertexPair => {
            let a = rng.random_range(1..=vertex_count);
            let mut b = rng.random_range(1..=vertex_count);
            while b == a {
                b = rng.random_range(1..=vertex_count);
            }
            vec![("from".into(), a), ("to".into(), b)]
        }
        ParamKind::Create => {
            let id = CREATE_COUNTER.fetch_add(1, Ordering::Relaxed);
            vec![("id".into(), id)]
        }
        ParamKind::None => vec![],
    }
}

/// Vertex counts per dataset variant. Returns `None` for unknown variants.
pub fn vertex_count(variant: &str) -> Option<i64> {
    match variant {
        "small" => Some(10_000),
        "medium" => Some(100_000),
        "large" => Some(1_632_803),
        _ => None,
    }
}
