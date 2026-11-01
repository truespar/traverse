//! Entity and path functions: `apoc.node.*`, `apoc.nodes.*`, `apoc.rel.*`, `apoc.any.*`,
//! `apoc.label.*`, `apoc.path.*`, `apoc.schema.*` and `apoc.diff.*`.

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{int_arg, key_string, node_id_of, rel_id_of, str_arg};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.path.create" => {
            let Some(start) = args.first().cloned() else {
                return Some(Ok(V::Null));
            };
            let edges = match args.get(1) {
                Some(V::List(l)) => l.clone(),
                _ => Vec::new(),
            };
            // nodes of arbitrary edges need the graph; start-only paths are the common case, with
            // rels their endpoints chain as refs
            let mut nodes = vec![start];
            for e in &edges {
                if let V::EdgeRef { source, target, .. } = e {
                    let last = nodes.last().and_then(|n| match n {
                        V::NodeRef(id) => Some(*id),
                        V::Node { id, .. } => Some(*id),
                        _ => None,
                    });
                    let next = if last == Some(*source) {
                        *target
                    } else {
                        *source
                    };
                    nodes.push(V::NodeRef(next));
                }
            }
            Ok(V::Path { nodes, edges })
        }
        "apoc.path.combine" => {
            let (
                Some(V::Path {
                    nodes: n1,
                    edges: e1,
                }),
                Some(V::Path {
                    nodes: n2,
                    edges: e2,
                }),
            ) = (args.first(), args.get(1))
            else {
                return Some(Ok(V::Null));
            };
            let mut nodes = n1.clone();
            let mut edges = e1.clone();
            // the second path's first node must be the first path's last
            nodes.extend(n2.iter().skip(1).cloned());
            edges.extend(e2.iter().cloned());
            Ok(V::Path { nodes, edges })
        }
        "apoc.path.elements" => match args.first() {
            Some(V::Path { nodes, edges }) => {
                let mut out = Vec::with_capacity(nodes.len() + edges.len());
                for i in 0..nodes.len() {
                    out.push(nodes[i].clone());
                    if i < edges.len() {
                        out.push(edges[i].clone());
                    }
                }
                Ok(V::List(out))
            }
            _ => Ok(V::Null),
        },
        "apoc.path.slice" => {
            let offset = int_arg(args, 1).unwrap_or(0).max(0) as usize;
            let length = int_arg(args, 2).unwrap_or(-1);
            match args.first() {
                Some(V::Path { nodes, edges }) => {
                    let end_edge = if length < 0 {
                        edges.len()
                    } else {
                        (offset + length as usize).min(edges.len())
                    };
                    if offset > edges.len() {
                        return Some(Ok(V::Null));
                    }
                    Ok(V::Path {
                        nodes: nodes[offset..=end_edge.min(nodes.len() - 1)].to_vec(),
                        edges: edges[offset..end_edge].to_vec(),
                    })
                }
                _ => Ok(V::Null),
            }
        }
        "apoc.any.isdeleted" => Ok(V::Bool(false)),
        _ => return None,
    };
    Some(r)
}

/// The functions of this module that read the graph; `None` for other names.
pub(super) fn eval_graph(
    name: &str,
    args: &[CypherValue],
    graph: &dyn GraphView,
) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.label.exists" => {
            let node = args.first()?;
            let label = match args.get(1) {
                Some(V::String(s)) => s.clone(),
                _ => return Some(Ok(V::Null)),
            };
            match node_id_of(node) {
                Some(id) => match graph.get_node(id) {
                    Ok(n) => Ok(V::Bool(
                        n.labels
                            .iter()
                            .any(|l| graph.strings().resolve(l.0) == label),
                    )),
                    Err(_) => Ok(V::Bool(false)),
                },
                None => Ok(V::Null),
            }
        }
        "apoc.node.degree" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            let (etype, dir) = parse_type_spec(args.get(1));
            Ok(V::Integer(node_degree(graph, id, etype.as_deref(), dir)))
        }
        "apoc.node.relationship.exists" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            let (etype, dir) = parse_type_spec(args.get(1));
            Ok(V::Bool(node_degree(graph, id, etype.as_deref(), dir) > 0))
        }
        "apoc.node.id" => Ok(match args.first().and_then(node_id_of) {
            Some(id) => V::Integer(id.0 as i64),
            None => V::Null,
        }),
        "apoc.node.labels" => match args.first().and_then(node_id_of) {
            Some(id) => match graph.get_node(id) {
                Ok(n) => Ok(V::List(
                    n.labels
                        .iter()
                        .map(|l| V::String(graph.strings().resolve(l.0).to_string()))
                        .collect(),
                )),
                Err(_) => Ok(V::Null),
            },
            None => Ok(V::Null),
        },
        "apoc.node.degree.in" | "apoc.node.degree.out" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            let spec = str_arg(args, 1);
            let etype = spec.filter(|s| !s.is_empty());
            let out = name.ends_with("out");
            Ok(V::Integer(node_degree(
                graph,
                id,
                etype.as_deref(),
                Some(out),
            )))
        }
        "apoc.node.relationship.types" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            Ok(V::List(
                node_rel_types(graph, id)
                    .into_iter()
                    .map(V::String)
                    .collect(),
            ))
        }
        "apoc.node.relationships.exist" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            let spec = str_arg(args, 1).unwrap_or_default();
            let mut m = BTreeMap::new();
            for part in spec.split('|').map(str::trim).filter(|s| !s.is_empty()) {
                let (t, d) = parse_type_spec(Some(&V::String(part.to_string())));
                m.insert(
                    part.to_string(),
                    V::Bool(node_degree(graph, id, t.as_deref(), d) > 0),
                );
            }
            Ok(V::Map(m))
        }
        "apoc.nodes.isdense" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            Ok(V::Bool(node_degree(graph, id, None, None) > 50))
        }
        "apoc.nodes.connected" => {
            let (Some(a), Some(b)) = (
                args.first().and_then(node_id_of),
                args.get(1).and_then(node_id_of),
            ) else {
                return Some(Ok(V::Null));
            };
            let spec = str_arg(args, 2).unwrap_or_default();
            // type list only: connectivity checked both ways
            let specs: Vec<Option<String>> = if spec.trim().is_empty() {
                vec![None]
            } else {
                spec.split('|')
                    .map(|s| parse_type_spec(Some(&V::String(s.trim().to_string()))).0)
                    .collect()
            };
            let mut connected = false;
            'outer: for t in &specs {
                for dir in [
                    traverse_core::graph::types::Direction::Outgoing,
                    traverse_core::graph::types::Direction::Incoming,
                ] {
                    if graph
                        .neighbors(a, dir, t.as_deref())
                        .map(|v| v.contains(&b))
                        .unwrap_or(false)
                    {
                        connected = true;
                        break 'outer;
                    }
                }
            }
            Ok(V::Bool(connected))
        }
        "apoc.nodes.relationship.types" => {
            // (nodes, ...) -> list of {node, types} maps, as APOC
            let items = match args.first() {
                Some(V::List(l)) => l.clone(),
                Some(other) => vec![other.clone()],
                None => Vec::new(),
            };
            let out = items
                .iter()
                .filter_map(|item| {
                    let id = node_id_of(item)?;
                    let mut m = BTreeMap::new();
                    m.insert("node".to_string(), V::NodeRef(id));
                    m.insert(
                        "types".to_string(),
                        V::List(
                            node_rel_types(graph, id)
                                .into_iter()
                                .map(V::String)
                                .collect(),
                        ),
                    );
                    Some(V::Map(m))
                })
                .collect();
            Ok(V::List(out))
        }
        "apoc.nodes.relationships.exist" => {
            let items = match args.first() {
                Some(V::List(l)) => l.clone(),
                Some(other) => vec![other.clone()],
                None => Vec::new(),
            };
            let spec = str_arg(args, 1).unwrap_or_default();
            let out = items
                .iter()
                .filter_map(|item| {
                    let id = node_id_of(item)?;
                    let mut per = BTreeMap::new();
                    for part in spec.split('|').map(str::trim).filter(|s| !s.is_empty()) {
                        let (t, d) = parse_type_spec(Some(&V::String(part.to_string())));
                        per.insert(
                            part.to_string(),
                            V::Bool(node_degree(graph, id, t.as_deref(), d) > 0),
                        );
                    }
                    let mut m = BTreeMap::new();
                    m.insert("node".to_string(), V::NodeRef(id));
                    m.insert("exists".to_string(), V::Map(per));
                    Some(V::Map(m))
                })
                .collect();
            Ok(V::List(out))
        }
        "apoc.rel.id" => Ok(match rel_id_of(args.first()) {
            Some(id) => V::Integer(id.0 as i64),
            None => V::Null,
        }),
        "apoc.rel.type" => match rel_id_of(args.first()) {
            Some(id) => match graph.get_edge(id) {
                Ok(e) => Ok(V::String(
                    graph.strings().resolve(e.edge_type.0).to_string(),
                )),
                Err(_) => Ok(V::Null),
            },
            None => Ok(V::Null),
        },
        "apoc.rel.startnode" | "apoc.rel.endnode" => match rel_id_of(args.first()) {
            Some(id) => match graph.get_edge(id) {
                Ok(e) => Ok(V::NodeRef(if name.ends_with("startnode") {
                    e.source
                } else {
                    e.target
                })),
                Err(_) => Ok(V::Null),
            },
            None => Ok(V::Null),
        },
        "apoc.any.property" => {
            let key = str_arg(args, 1);
            let Some(key) = key else {
                return Some(Ok(V::Null));
            };
            Ok(entity_properties(graph, args.first())
                .and_then(|m| m.get(&key).cloned())
                .unwrap_or(V::Null))
        }
        "apoc.any.properties" => {
            let keys: Option<Vec<String>> = match args.get(1) {
                Some(V::List(l)) => Some(l.iter().map(key_string).collect()),
                _ => None,
            };
            match entity_properties(graph, args.first()) {
                Some(m) => Ok(V::Map(match keys {
                    Some(ks) => m.into_iter().filter(|(k, _)| ks.contains(k)).collect(),
                    None => m,
                })),
                None => Ok(V::Null),
            }
        }
        "apoc.diff.nodes" => {
            let (Some(a), Some(b)) = (
                entity_properties(graph, args.first()),
                entity_properties(graph, args.get(1)),
            ) else {
                return Some(Ok(V::Null));
            };
            let mut left_only = BTreeMap::new();
            let mut right_only = BTreeMap::new();
            let mut in_common = BTreeMap::new();
            let mut different = BTreeMap::new();
            for (k, v) in &a {
                match b.get(k) {
                    None => {
                        left_only.insert(k.clone(), v.clone());
                    }
                    Some(w) if w == v => {
                        in_common.insert(k.clone(), v.clone());
                    }
                    Some(w) => {
                        let mut d = BTreeMap::new();
                        d.insert("left".to_string(), v.clone());
                        d.insert("right".to_string(), w.clone());
                        different.insert(k.clone(), V::Map(d));
                    }
                }
            }
            for (k, w) in &b {
                if !a.contains_key(k) {
                    right_only.insert(k.clone(), w.clone());
                }
            }
            let mut m = BTreeMap::new();
            m.insert("leftOnly".to_string(), V::Map(left_only));
            m.insert("rightOnly".to_string(), V::Map(right_only));
            m.insert("inCommon".to_string(), V::Map(in_common));
            m.insert("different".to_string(), V::Map(different));
            Ok(V::Map(m))
        }
        "apoc.schema.node.indexexists"
        | "apoc.schema.node.constraintexists"
        | "apoc.schema.relationship.indexexists"
        | "apoc.schema.relationship.constraintexists" => {
            let (Some(label), Some(V::List(props))) = (str_arg(args, 0), args.get(1)) else {
                return Some(Ok(V::Null));
            };
            let want: Vec<String> = props.iter().map(key_string).collect();
            let strings = |id: u32| graph.strings().resolve(id).to_string();
            let found = if name.contains("indexexists") {
                if name.contains("relationship") {
                    graph.edge_index_catalog().descriptors().iter().any(|d| {
                        strings(d.edge_type_id.0) == label
                            && d.property_key.map(|p| {
                                strings(p.0)
                                    .split('\u{1F}')
                                    .map(str::to_string)
                                    .collect::<Vec<_>>()
                                    == want
                            }) == Some(true)
                    })
                } else {
                    graph.index_catalog().descriptors().iter().any(|d| {
                        strings(d.label_id.0) == label
                            && strings(d.property_key.0)
                                .split('\u{1F}')
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                                == want
                    })
                }
            } else {
                let want_rel = name.contains("relationship");
                graph.constraint_registry().descriptors().iter().any(|d| {
                    let is_rel = matches!(
                        d.entity,
                        traverse_core::graph::constraint::ConstraintEntity::Relationship
                    );
                    if is_rel != want_rel || strings(d.label_id.0) != label {
                        return false;
                    }
                    let mut props = vec![strings(d.property_key.0)];
                    props.extend(d.extra_property_keys.iter().map(|k| strings(k.0)));
                    props == want
                })
            };
            Ok(V::Bool(found))
        }
        _ => return None,
    };
    Some(r)
}

/// Distinct relationship types touching a node.
fn node_rel_types(graph: &dyn GraphView, id: traverse_core::graph::types::NodeId) -> Vec<String> {
    use traverse_core::graph::types::Direction;
    let mut out: Vec<String> = Vec::new();
    for dir in [Direction::Outgoing, Direction::Incoming] {
        for eid in graph.edges_of(id, dir, None).unwrap_or_default() {
            if let Ok(e) = graph.get_edge(eid) {
                let t = graph.strings().resolve(e.edge_type.0).to_string();
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
    }
    out
}

/// String-keyed properties of any entity value (ref or materialized node/rel, or a plain map).
fn entity_properties(
    graph: &dyn GraphView,
    v: Option<&CypherValue>,
) -> Option<BTreeMap<String, CypherValue>> {
    match v {
        Some(CypherValue::Map(m)) => Some(m.clone()),
        Some(CypherValue::Node { properties, .. }) | Some(CypherValue::Edge { properties, .. }) => {
            Some(
                properties
                    .iter()
                    .map(|(k, pv)| (k.clone(), CypherValue::from_property(pv)))
                    .collect(),
            )
        }
        Some(CypherValue::NodeRef(id)) => graph.get_node(*id).ok().map(|n| {
            n.properties
                .iter()
                .map(|(k, pv)| {
                    (
                        graph.strings().resolve(k.0).to_string(),
                        CypherValue::from_property(&pv),
                    )
                })
                .collect()
        }),
        Some(CypherValue::EdgeRef { id, .. }) => graph.get_edge(*id).ok().map(|e| {
            e.properties
                .iter()
                .map(|(k, pv)| {
                    (
                        graph.strings().resolve(k.0).to_string(),
                        CypherValue::from_property(&pv),
                    )
                })
                .collect()
        }),
        _ => None,
    }
}

/// APOC relationship type spec: `"TYPE"`, `"TYPE>"` (outgoing), `"<TYPE"` (incoming), `">"`/`"<"`
/// (any type, directed), or None (all).
fn parse_type_spec(v: Option<&CypherValue>) -> (Option<String>, Option<bool>) {
    let Some(CypherValue::String(s)) = v else {
        return (None, None);
    };
    let s = s.trim();
    if let Some(t) = s.strip_suffix('>') {
        (
            (!t.is_empty()).then(|| t.to_string()),
            Some(true), // outgoing
        )
    } else if let Some(t) = s.strip_prefix('<') {
        ((!t.is_empty()).then(|| t.to_string()), Some(false))
    } else if s.is_empty() {
        (None, None)
    } else {
        (Some(s.to_string()), None)
    }
}

fn node_degree(
    graph: &dyn GraphView,
    id: traverse_core::graph::types::NodeId,
    etype: Option<&str>,
    dir: Option<bool>,
) -> i64 {
    use traverse_core::graph::types::Direction;
    let dirs: &[Direction] = match dir {
        Some(true) => &[Direction::Outgoing],
        Some(false) => &[Direction::Incoming],
        None => &[Direction::Outgoing, Direction::Incoming],
    };
    let mut count = 0i64;
    for d in dirs {
        count += graph.neighbors(id, *d, etype).map(|v| v.len()).unwrap_or(0) as i64;
    }
    count
}
