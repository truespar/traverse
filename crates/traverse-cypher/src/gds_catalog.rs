//! `gds.*` compatibility: the graph catalog and GDS algorithm names mapped onto
//! the native engines.
//!
//! GDS is two-phase: `gds.graph.project('g', nodes, rels)` creates a NAMED
//! PROJECTION, then algorithms run on it by name (`gds.pageRank.stream('g',
//! config)`). A projection here holds its label/type filters, merged into the
//! algorithm config at call time, plus a materialized snapshot for stream/stats
//! calls. The registry lives as long as the process (like `apoc.static.*` /
//! triggers).

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use crate::error::CypherError;
use crate::types::CypherValue;

/// One named projection: label/type filters plus counts at project time (for `gds.graph.list`).
#[derive(Debug, Clone)]
pub struct GdsProjection {
    /// Name (catalog key).
    pub name: String,
    /// Node labels; empty = all (`'*'`).
    pub node_labels: Vec<String>,
    /// Relationship types; empty = all (`'*'`).
    pub rel_types: Vec<String>,
    /// Relationship weight property, if declared.
    pub rel_weight_property: Option<String>,
    /// Node count at project time.
    pub node_count: i64,
    /// Relationship count at project time.
    pub rel_count: i64,
    /// Original projection arguments, echoed by list/project.
    pub node_projection: CypherValue,
    /// See [`Self::node_projection`].
    pub rel_projection: CypherValue,
    /// Dense-id CSR snapshot built at project time. Stream/stats calls run on it (O(projection)
    /// allocation, GDS snapshot semantics); write mode and property-reading algorithms use the live
    /// graph.
    pub materialized: Option<std::sync::Arc<crate::algorithms::materialized::MaterializedGraph>>,
}

fn registry() -> &'static RwLock<Vec<GdsProjection>> {
    static REG: OnceLock<RwLock<Vec<GdsProjection>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(Vec::new()))
}

/// Inserts or replaces a projection.
pub fn put(p: GdsProjection) {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.retain(|x| x.name != p.name);
    reg.push(p);
}

/// Projection by name.
pub fn get(name: &str) -> Option<GdsProjection> {
    registry()
        .read()
        .ok()?
        .iter()
        .find(|p| p.name == name)
        .cloned()
}

/// Removes and returns a projection.
pub fn drop_projection(name: &str) -> Option<GdsProjection> {
    let mut reg = registry().write().ok()?;
    let i = reg.iter().position(|p| p.name == name)?;
    Some(reg.remove(i))
}

/// All projections, sorted by name.
pub fn list() -> Vec<GdsProjection> {
    let mut v = registry().read().map(|r| r.clone()).unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// GDS node projection (`'*'`, `'Label'`, `['A', 'B']`, `{Label: {...}, ...}`) -> label list (empty =
/// all).
pub fn parse_node_projection(v: &CypherValue) -> Result<Vec<String>, CypherError> {
    match v {
        CypherValue::String(s) if s == "*" => Ok(Vec::new()),
        CypherValue::String(s) => Ok(vec![s.clone()]),
        CypherValue::List(items) => items
            .iter()
            .map(|i| match i {
                CypherValue::String(s) => Ok(s.clone()),
                other => Err(CypherError::type_error(format!(
                    "gds.graph.project: node projection list expects strings, got {}",
                    other.type_name()
                ))),
            })
            .collect(),
        CypherValue::Map(m) => Ok(m.keys().cloned().collect()),
        other => Err(CypherError::type_error(format!(
            "gds.graph.project: unsupported node projection ({})",
            other.type_name()
        ))),
    }
}

/// GDS relationship projection (`'*'`, `'TYPE'`, `['A', 'B']`, `{TYPE: {type, orientation,
/// properties}, ...}`) -> (types, weight_property).
pub fn parse_rel_projection(
    v: &CypherValue,
) -> Result<(Vec<String>, Option<String>, bool), CypherError> {
    let mut undirected = false;
    match v {
        CypherValue::String(s) if s == "*" => Ok((Vec::new(), None, undirected)),
        CypherValue::String(s) => Ok((vec![s.clone()], None, undirected)),
        CypherValue::List(items) => {
            let types = items
                .iter()
                .map(|i| match i {
                    CypherValue::String(s) => Ok(s.clone()),
                    other => Err(CypherError::type_error(format!(
                        "gds.graph.project: relationship projection list expects strings, got {}",
                        other.type_name()
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((types, None, undirected))
        }
        CypherValue::Map(m) => {
            let mut types = Vec::new();
            let mut weight = None;
            for (key, spec) in m {
                match spec {
                    CypherValue::Map(sm) => {
                        // {FRIEND: {type: 'FRIEND', orientation, properties}}
                        let t = match sm.get("type") {
                            Some(CypherValue::String(t)) => t.clone(),
                            _ => key.clone(),
                        };
                        types.push(t);
                        if matches!(sm.get("orientation"),
                            Some(CypherValue::String(o)) if o.eq_ignore_ascii_case("UNDIRECTED"))
                        {
                            undirected = true;
                        }
                        match sm.get("properties") {
                            Some(CypherValue::String(p)) => weight = Some(p.clone()),
                            Some(CypherValue::Map(pm)) => {
                                if let Some(first) = pm.keys().next() {
                                    weight = Some(first.clone());
                                }
                            }
                            Some(CypherValue::List(pl)) => {
                                if let Some(CypherValue::String(p)) = pl.first() {
                                    weight = Some(p.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => types.push(key.clone()),
                }
            }
            Ok((types, weight, undirected))
        }
        other => Err(CypherError::type_error(format!(
            "gds.graph.project: unsupported relationship projection ({})",
            other.type_name()
        ))),
    }
}

/// GDS algorithm name -> (engine name, column renames traverse->gds, extra null columns GDS declares
/// that we don't produce). Unmapped names get a clear error.
pub fn algo_mapping(
    gds_algo: &str,
) -> Option<(
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
)> {
    Some(match gds_algo {
        "pageRank" => ("pageRank", &[], &[]),
        "wcc" => ("wcc", &[("communityId", "componentId")], &[]),
        "louvain" => ("louvain", &[], &["intermediateCommunityIds"]),
        "labelPropagation" => ("labelPropagation", &[], &[]),
        "betweenness" => ("betweenness", &[], &[]),
        "degree" => ("degree", &[], &[]),
        "nodeSimilarity" => (
            "nodeSimilarity",
            &[
                ("sourceNodeId", "node1"),
                ("targetNodeId", "node2"),
                ("score", "similarity"),
            ],
            &[],
        ),
        "knn" => (
            "knn",
            &[
                ("sourceNodeId", "node1"),
                ("targetNodeId", "node2"),
                ("score", "similarity"),
            ],
            &[],
        ),
        "fastRP" => ("fastRP", &[], &[]),
        "shortestPath.dijkstra" => (
            "dijkstra",
            &[
                ("sourceNodeId", "sourceNode"),
                ("targetNodeId", "targetNode"),
            ],
            &["index", "path"],
        ),
        // -- further engines ------
        "articleRank" => ("articleRank", &[], &[]),
        "eigenvector" => ("eigenvector", &[], &[]),
        "closeness" => ("closeness", &[], &[]),
        "closeness.harmonic" => ("harmonicCloseness", &[], &[]),
        "hits" => ("hits", &[], &["values"]),
        "triangleCount" => ("triangleCount", &[], &[]),
        "scc" => ("scc", &[("communityId", "componentId")], &[]),
        "kcore" => ("kCore", &[("communityId", "coreValue")], &[]),
        "leiden" => ("leiden", &[], &["intermediateCommunityIds"]),
        "bfs" => ("bfs", &[("sourceNodeId", "sourceNode")], &["path"]),
        "dfs" => ("dfs", &[("sourceNodeId", "sourceNode")], &["path"]),
        "shortestPath.astar" => (
            "aStar",
            &[
                ("sourceNodeId", "sourceNode"),
                ("targetNodeId", "targetNode"),
            ],
            &["index", "path"],
        ),
        "shortestPath.yens" => (
            "yens",
            &[
                ("sourceNodeId", "sourceNode"),
                ("targetNodeId", "targetNode"),
            ],
            &["index", "path"],
        ),
        "bellmanFord" => (
            "bellmanFord",
            &[
                ("sourceNodeId", "sourceNode"),
                ("targetNodeId", "targetNode"),
            ],
            &["index", "route", "isNegativeCycle"],
        ),
        "allShortestPaths" => ("allShortestPaths", &[("totalCost", "distance")], &[]),
        "allShortestPaths.delta" => (
            "deltaStepping",
            &[
                ("sourceNodeId", "sourceNode"),
                ("targetNodeId", "targetNode"),
            ],
            &["index", "path"],
        ),
        "randomWalk" => ("randomWalk", &[], &["path"]),
        "node2vec" => ("node2vec", &[], &[]),
        "hashgnn" => ("hashGnn", &[], &[]),
        "dag.topologicalSort" => (
            "topologicalSort",
            &[("score", "maxDistanceFromSource")],
            &[],
        ),
        "dag.longestPath" => (
            "longestPath",
            &[("nodeId", "targetNode"), ("score", "totalCost")],
            &["index", "sourceNode", "nodeIds", "costs", "path"],
        ),
        "influenceMaximization.celf" => ("celf", &[("score", "spread")], &[]),
        _ => return None,
    })
}

/// Merges a projection's filters into an algorithm config. Explicit config keys win (GDS:
/// algorithm-level nodeLabels / relationshipTypes further filter the projection; override is the
/// closest equivalent with filter-based projections).
pub fn merge_projection_into_config(
    proj: &GdsProjection,
    config: &mut BTreeMap<String, CypherValue>,
) {
    if !proj.node_labels.is_empty() && !config.contains_key("nodeLabels") {
        config.insert(
            "nodeLabels".to_string(),
            CypherValue::List(
                proj.node_labels
                    .iter()
                    .map(|l| CypherValue::String(l.clone()))
                    .collect(),
            ),
        );
    }
    if !proj.rel_types.is_empty() && !config.contains_key("relationshipTypes") {
        config.insert(
            "relationshipTypes".to_string(),
            CypherValue::List(
                proj.rel_types
                    .iter()
                    .map(|t| CypherValue::String(t.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(w) = &proj.rel_weight_property {
        config
            .entry("relationshipWeightProperty".to_string())
            .or_insert_with(|| CypherValue::String(w.clone()));
    }
}

/// `gds.util.*` / `gds.similarity.*` FUNCTIONS (names arrive lowercased); None outside the subset.
pub(crate) fn eval_fn(
    name: &str,
    args: &[CypherValue],
    graph: &dyn traverse_core::tx::view::GraphView,
) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    fn nums(v: &CypherValue) -> Option<Vec<f64>> {
        match v {
            V::List(l) => l
                .iter()
                .map(|x| match x {
                    V::Integer(i) => Some(*i as f64),
                    V::Float(f) => Some(*f),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }
    let node_of = |v: &CypherValue| -> CypherValue {
        let id = match v {
            V::Integer(i) if *i >= 0 => traverse_core::graph::types::NodeId(*i as u64),
            V::NodeRef(id) => *id,
            _ => return V::Null,
        };
        if graph.get_node(id).is_ok() {
            V::NodeRef(id)
        } else {
            V::Null
        }
    };
    // link-prediction scores (GDS ships them as FUNCTIONS), matching live GDS: undirected;
    // commonNeighbors / totalNeighbors count DISTINCT neighbors; adamicAdar / resourceAllocation /
    // preferentialAttachment weight by RAW degree (in + out, parallel edges included)
    if let Some(rest) = name.strip_prefix("gds.linkprediction.") {
        use traverse_core::graph::types::{Direction, NodeId};
        let id_of = |v: &CypherValue| -> Option<NodeId> {
            match v {
                V::NodeRef(id) => Some(*id),
                V::Node { id, .. } => Some(*id),
                V::Integer(i) if *i >= 0 => Some(NodeId(*i as u64)),
                _ => None,
            }
        };
        let (Some(a), Some(b)) = (args.first().and_then(id_of), args.get(1).and_then(id_of)) else {
            return Some(Ok(V::Null));
        };
        let nbrs = |n: NodeId| -> Vec<NodeId> {
            let mut v = graph
                .neighbors(n, Direction::Outgoing, None)
                .unwrap_or_default();
            v.extend(
                graph
                    .neighbors(n, Direction::Incoming, None)
                    .unwrap_or_default(),
            );
            v.retain(|x| *x != n);
            v.sort_unstable_by_key(|x| x.0);
            v.dedup();
            v
        };
        let degree = |n: NodeId| -> f64 {
            (graph
                .edges_of(n, Direction::Outgoing, None)
                .unwrap_or_default()
                .len()
                + graph
                    .edges_of(n, Direction::Incoming, None)
                    .unwrap_or_default()
                    .len()) as f64
        };
        let na = nbrs(a);
        let nb = nbrs(b);
        let common: Vec<NodeId> = na.iter().filter(|x| nb.contains(x)).copied().collect();
        let score = match rest {
            "commonneighbors" => common.len() as f64,
            "totalneighbors" => {
                let mut u = na.clone();
                u.extend(nb.iter().copied());
                u.sort_unstable_by_key(|x| x.0);
                u.dedup();
                u.len() as f64
            }
            "preferentialattachment" => degree(a) * degree(b),
            "adamicadar" => common
                .iter()
                .map(|z| {
                    let d = degree(*z);
                    if d > 1.0 { 1.0 / d.ln() } else { 0.0 }
                })
                .sum(),
            "resourceallocation" => common
                .iter()
                .map(|z| {
                    let d = degree(*z);
                    if d > 0.0 { 1.0 / d } else { 0.0 }
                })
                .sum(),
            "samecommunity" => {
                let prop = match args.get(2) {
                    Some(V::String(s)) => s.clone(),
                    _ => "community".to_string(),
                };
                let (pa, pb) = (
                    graph.get_node_property(a, &prop).ok().flatten(),
                    graph.get_node_property(b, &prop).ok().flatten(),
                );
                match (pa, pb) {
                    (Some(x), Some(y)) if x == y => 1.0,
                    _ => 0.0,
                }
            }
            _ => {
                return Some(Err(CypherError::semantic(format!(
                    "Unknown function: {name} (not in Traverse's GDS compatibility subset)"
                ))
                .with_status(crate::status::GqlStatus::UnknownFunction)));
            }
        };
        return Some(Ok(V::Float(score)));
    }
    Some(match name {
        "gds.util.asnode" => Ok(args.first().map(node_of).unwrap_or(V::Null)),
        "gds.util.asnodes" => Ok(V::List(match args.first() {
            Some(V::List(l)) => l.iter().map(node_of).collect(),
            _ => Vec::new(),
        })),
        "gds.util.nan" => Ok(V::Float(f64::NAN)),
        "gds.util.infinity" => Ok(V::Float(f64::INFINITY)),
        "gds.util.isfinite" => Ok(V::Bool(
            matches!(
                args.first(),
                Some(V::Float(f)) if f.is_finite()
            ) || matches!(args.first(), Some(V::Integer(_))),
        )),
        "gds.util.isinfinite" => Ok(V::Bool(
            matches!(args.first(), Some(V::Float(f)) if f.is_infinite()),
        )),
        "gds.similarity.cosine"
        | "gds.similarity.jaccard"
        | "gds.similarity.overlap"
        | "gds.similarity.pearson"
        | "gds.similarity.euclidean"
        | "gds.similarity.euclideandistance" => {
            let (Some(a), Some(b)) = (args.first().and_then(nums), args.get(1).and_then(nums))
            else {
                return Some(Ok(V::Null));
            };
            let v = match name {
                "gds.similarity.cosine" => {
                    let dot: f64 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
                    let (na, nb) = (
                        a.iter().map(|x| x * x).sum::<f64>().sqrt(),
                        b.iter().map(|x| x * x).sum::<f64>().sqrt(),
                    );
                    if na == 0.0 || nb == 0.0 {
                        0.0
                    } else {
                        dot / (na * nb)
                    }
                }
                "gds.similarity.jaccard" => {
                    // set semantics over the values (GDS jaccard on id lists)
                    use std::collections::BTreeSet;
                    let sa: BTreeSet<u64> = a.iter().map(|x| x.to_bits()).collect();
                    let sb: BTreeSet<u64> = b.iter().map(|x| x.to_bits()).collect();
                    let inter = sa.intersection(&sb).count() as f64;
                    let union = sa.union(&sb).count() as f64;
                    if union == 0.0 { 0.0 } else { inter / union }
                }
                "gds.similarity.overlap" => {
                    use std::collections::BTreeSet;
                    let sa: BTreeSet<u64> = a.iter().map(|x| x.to_bits()).collect();
                    let sb: BTreeSet<u64> = b.iter().map(|x| x.to_bits()).collect();
                    let inter = sa.intersection(&sb).count() as f64;
                    let min = sa.len().min(sb.len()) as f64;
                    if min == 0.0 { 0.0 } else { inter / min }
                }
                "gds.similarity.pearson" => {
                    let n = a.len().min(b.len()) as f64;
                    if n == 0.0 {
                        0.0
                    } else {
                        let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
                        let cov: f64 = a.iter().zip(&b).map(|(x, y)| (x - ma) * (y - mb)).sum();
                        let (da, db) = (
                            a.iter().map(|x| (x - ma).powi(2)).sum::<f64>().sqrt(),
                            b.iter().map(|y| (y - mb).powi(2)).sum::<f64>().sqrt(),
                        );
                        if da == 0.0 || db == 0.0 {
                            0.0
                        } else {
                            cov / (da * db)
                        }
                    }
                }
                "gds.similarity.euclideandistance" => a
                    .iter()
                    .zip(&b)
                    .map(|(x, y)| (x - y).powi(2))
                    .sum::<f64>()
                    .sqrt(),
                // euclidean similarity = 1 / (1 + distance)
                _ => {
                    let d = a
                        .iter()
                        .zip(&b)
                        .map(|(x, y)| (x - y).powi(2))
                        .sum::<f64>()
                        .sqrt();
                    1.0 / (1.0 + d)
                }
            };
            Ok(V::Float(v))
        }
        _ => return None,
    })
}
