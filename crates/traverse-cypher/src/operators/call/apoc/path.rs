//! `apoc.path.*` and `apoc.algo.*`: configurable expansion and path algorithms.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::operators::call::ProcCall;
use crate::types::CypherValue;

use super::{apoc_expand, apoc_step, node_ref_of, parse_rel_specs};

/// `apoc.path.*` and `apoc.algo.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.path.expand(start, relFilter, labelFilter, minLevel, maxLevel) :: (path)
        // apoc.path.subgraphNodes(start, config) :: (node)
        // apoc.path.spanningTree(start, config) :: (path)
        "apoc.path.expand" | "apoc.path.subgraphnodes" | "apoc.path.spanningtree" => {
            let mode = name_lower
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_string();
            let mut out = Vec::new();
            for frame in frames {
                let start_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let Some(start) = node_ref_of(&start_v) else {
                    continue; // null start: no rows (APOC)
                };
                // positional (expand) vs config map (subgraphNodes / spanningTree) arguments
                let (rel_filter, label_filter, min_level, max_level) = if mode == "expand" {
                    (
                        match args.and_then(|a| a.get(1)) {
                            Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                                CypherValue::String(s) => s,
                                _ => String::new(),
                            },
                            None => String::new(),
                        },
                        match args.and_then(|a| a.get(2)) {
                            Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                                CypherValue::String(s) => s,
                                _ => String::new(),
                            },
                            None => String::new(),
                        },
                        args.and_then(|a| a.get(3))
                            .map(|e| eval_expr(e, frame, &*graph, symtab))
                            .transpose()?
                            .and_then(|v| match v {
                                CypherValue::Integer(n) => Some(n),
                                _ => None,
                            })
                            .unwrap_or(1),
                        args.and_then(|a| a.get(4))
                            .map(|e| eval_expr(e, frame, &*graph, symtab))
                            .transpose()?
                            .and_then(|v| match v {
                                CypherValue::Integer(n) => Some(n),
                                _ => None,
                            })
                            .unwrap_or(-1),
                    )
                } else {
                    let config = match args.and_then(|a| a.get(1)) {
                        Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                            CypherValue::Map(m) => m,
                            _ => Default::default(),
                        },
                        None => Default::default(),
                    };
                    let gets = |k: &str| match config.get(k) {
                        Some(CypherValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let geti = |k: &str, d: i64| match config.get(k) {
                        Some(CypherValue::Integer(n)) => *n,
                        _ => d,
                    };
                    (
                        gets("relationshipFilter"),
                        gets("labelFilter"),
                        geti("minLevel", 0),
                        geti("maxLevel", -1),
                    )
                };
                let paths = apoc_expand(
                    graph,
                    start,
                    &rel_filter,
                    &label_filter,
                    min_level,
                    max_level,
                    &mode,
                )?;
                for (nodes, edges) in paths {
                    let mut nf = frame.clone();
                    if mode == "subgraphnodes" {
                        if let Some(sl) = cx.slot_for("node") {
                            nf[sl] =
                                CypherValue::NodeRef(*node_ids_of_path(&nodes).last().unwrap())
                                    .into();
                        }
                    } else if let Some(sl) = cx.slot_for("path") {
                        nf[sl] = CypherValue::Path { nodes, edges }.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.path.expandconfig" | "apoc.path.subgraphall" => {
            let all_mode = name_lower.ends_with("subgraphall");
            let mut out = Vec::new();
            for frame in frames {
                let start_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let Some(start) = node_ref_of(&start_v) else {
                    continue;
                };
                let config = match cx.opt_arg(1, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let gets = |k: &str| match config.get(k) {
                    Some(CypherValue::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let geti = |k: &str, d: i64| match config.get(k) {
                    Some(CypherValue::Integer(n)) => *n,
                    _ => d,
                };
                if all_mode {
                    let paths = apoc_expand(
                        &*graph,
                        start,
                        &gets("relationshipFilter"),
                        &gets("labelFilter"),
                        0,
                        geti("maxLevel", -1),
                        "subgraphnodes",
                    )?;
                    let nodes: Vec<CypherValue> = paths
                        .iter()
                        .filter_map(|(n, _)| n.last().cloned())
                        .collect();
                    let node_set: std::collections::HashSet<u64> =
                        nodes.iter().filter_map(node_ref_of).map(|n| n.0).collect();
                    // All rels among the subgraph's nodes.
                    let mut rels = Vec::new();
                    for v in &nodes {
                        let Some(nid) = node_ref_of(v) else { continue };
                        use traverse_core::graph::types::Direction;
                        for eid in graph
                            .edges_of(nid, Direction::Outgoing, None)
                            .unwrap_or_default()
                        {
                            if let Ok(e) = graph.get_edge(eid)
                                && node_set.contains(&e.target.0)
                            {
                                rels.push(CypherValue::EdgeRef {
                                    id: eid,
                                    edge_type_id: e.edge_type,
                                    source: e.source,
                                    target: e.target,
                                });
                            }
                        }
                    }
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("nodes") {
                        nf[sl] = CypherValue::List(nodes).into();
                    }
                    if let Some(sl) = cx.slot_for("relationships") {
                        nf[sl] = CypherValue::List(rels).into();
                    }
                    out.push(nf);
                } else {
                    let paths = apoc_expand(
                        &*graph,
                        start,
                        &gets("relationshipFilter"),
                        &gets("labelFilter"),
                        geti("minLevel", 1).max(0),
                        geti("maxLevel", -1),
                        "expand",
                    )?;
                    for (nodes, edges) in paths {
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("path") {
                            nf[sl] = CypherValue::Path { nodes, edges }.into();
                        }
                        out.push(nf);
                    }
                }
            }
            Ok(out)
        }
        "apoc.algo.cover" => {
            let mut out = Vec::new();
            for frame in frames {
                let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let ids: Vec<_> = match v {
                    CypherValue::List(l) => l.iter().filter_map(node_ref_of).collect(),
                    other => node_ref_of(&other).into_iter().collect(),
                };
                let idset: std::collections::HashSet<u64> = ids.iter().map(|n| n.0).collect();
                use traverse_core::graph::types::Direction;
                for nid in &ids {
                    for eid in graph
                        .edges_of(*nid, Direction::Outgoing, None)
                        .unwrap_or_default()
                    {
                        let Ok(e) = graph.get_edge(eid) else { continue };
                        if idset.contains(&e.target.0) {
                            let mut nf = frame.clone();
                            if let Some(sl) = cx.slot_for("rel") {
                                nf[sl] = CypherValue::EdgeRef {
                                    id: eid,
                                    edge_type_id: e.edge_type,
                                    source: e.source,
                                    target: e.target,
                                }
                                .into();
                            }
                            out.push(nf);
                        }
                    }
                }
            }
            Ok(out)
        }
        "apoc.algo.dijkstra" | "apoc.algo.astar" | "apoc.algo.astarconfig" => {
            let mut out = Vec::new();
            for frame in frames {
                let (Some(start), Some(end)) = (
                    eval_expr(cx.arg(0)?, frame, &*graph, symtab)
                        .ok()
                        .as_ref()
                        .and_then(node_ref_of),
                    eval_expr(cx.arg(1)?, frame, &*graph, symtab)
                        .ok()
                        .as_ref()
                        .and_then(node_ref_of),
                ) else {
                    continue;
                };
                let spec = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => String::new(),
                };
                let weight_prop = match name_lower.as_str() {
                    "apoc.algo.astarconfig" => match cx.opt_arg(3, frame, &*graph) {
                        Some(CypherValue::Map(m)) => match m.get("weight") {
                            Some(CypherValue::String(s)) => s.clone(),
                            _ => "weight".to_string(),
                        },
                        _ => "weight".to_string(),
                    },
                    _ => match cx.opt_arg(3, frame, &*graph) {
                        Some(CypherValue::String(s)) => s,
                        _ => "weight".to_string(),
                    },
                };
                if let Some((nodes, edges, weight)) =
                    dijkstra_path(&*graph, start, end, &spec, &weight_prop)
                {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("path") {
                        nf[sl] = CypherValue::Path { nodes, edges }.into();
                    }
                    if let Some(sl) = cx.slot_for("weight") {
                        nf[sl] = CypherValue::Float(weight).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.algo.allsimplepaths" => {
            let mut out = Vec::new();
            for frame in frames {
                let (Some(start), Some(end)) = (
                    eval_expr(cx.arg(0)?, frame, &*graph, symtab)
                        .ok()
                        .as_ref()
                        .and_then(node_ref_of),
                    eval_expr(cx.arg(1)?, frame, &*graph, symtab)
                        .ok()
                        .as_ref()
                        .and_then(node_ref_of),
                ) else {
                    continue;
                };
                let spec = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => String::new(),
                };
                let max_nodes = match cx.opt_arg(3, frame, &*graph) {
                    Some(CypherValue::Integer(n)) => n.max(1),
                    _ => 6,
                };
                let paths = apoc_expand(&*graph, start, &spec, "", 1, max_nodes - 1, "expand")?;
                for (nodes, edges) in paths {
                    // Simple = node-unique; expand is edge-unique, so
                    // filter to node-unique paths ending at `end`.
                    let ids: Vec<u64> = nodes.iter().filter_map(node_ref_of).map(|n| n.0).collect();
                    let unique = {
                        let mut s: std::collections::HashSet<u64> = Default::default();
                        ids.iter().all(|i| s.insert(*i))
                    };
                    if unique && nodes.last().and_then(node_ref_of) == Some(end) {
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("path") {
                            nf[sl] = CypherValue::Path { nodes, edges }.into();
                        }
                        out.push(nf);
                    }
                }
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}

/// Node ids along a path's node list.
fn node_ids_of_path(nodes: &[CypherValue]) -> Vec<traverse_core::graph::types::NodeId> {
    nodes
        .iter()
        .filter_map(|v| match v {
            CypherValue::NodeRef(id) => Some(*id),
            CypherValue::Node { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

/// Dijkstra over weighted edges (apoc.algo.dijkstra / aStar): (nodes, edges, total weight) of the
/// cheapest path. Missing weights count as 1.0.
fn dijkstra_path(
    graph: &dyn GraphView,
    start: traverse_core::graph::types::NodeId,
    end: traverse_core::graph::types::NodeId,
    rel_spec: &str,
    weight_prop: &str,
) -> Option<(Vec<CypherValue>, Vec<CypherValue>, f64)> {
    use std::cmp::Reverse;
    use std::collections::{BinaryHeap, HashMap};
    let specs = parse_rel_specs(rel_spec);
    #[derive(PartialEq)]
    struct Entry(f64, u64);
    impl Eq for Entry {}
    impl PartialOrd for Entry {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Entry {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.0
                .partial_cmp(&other.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(self.1.cmp(&other.1))
        }
    }
    let mut dist: HashMap<u64, f64> = HashMap::new();
    let mut prev: HashMap<
        u64,
        (
            traverse_core::graph::types::NodeId,
            traverse_core::graph::types::EdgeId,
        ),
    > = HashMap::new();
    let mut heap = BinaryHeap::new();
    dist.insert(start.0, 0.0);
    heap.push(Reverse(Entry(0.0, start.0)));
    while let Some(Reverse(Entry(d, u))) = heap.pop() {
        if u == end.0 {
            break;
        }
        if d > *dist.get(&u).unwrap_or(&f64::MAX) {
            continue;
        }
        let unode = traverse_core::graph::types::NodeId(u);
        for (eid, other) in apoc_step(graph, unode, &specs) {
            let w = graph
                .get_edge_property(eid, weight_prop)
                .ok()
                .flatten()
                .and_then(|pv| match pv {
                    traverse_core::graph::property::PropertyValue::Int(i) => Some(i as f64),
                    traverse_core::graph::property::PropertyValue::Float(f) => Some(f),
                    _ => None,
                })
                .unwrap_or(1.0);
            let nd = d + w;
            if nd < *dist.get(&other.0).unwrap_or(&f64::MAX) {
                dist.insert(other.0, nd);
                prev.insert(other.0, (unode, eid));
                heap.push(Reverse(Entry(nd, other.0)));
            }
        }
    }
    let total = *dist.get(&end.0)?;
    // reconstruct
    let mut nodes_rev = vec![CypherValue::NodeRef(end)];
    let mut edges_rev = Vec::new();
    let mut cur = end.0;
    while cur != start.0 {
        let (p, eid) = prev.get(&cur)?;
        let e = graph.get_edge(*eid).ok()?;
        edges_rev.push(CypherValue::EdgeRef {
            id: *eid,
            edge_type_id: e.edge_type,
            source: e.source,
            target: e.target,
        });
        nodes_rev.push(CypherValue::NodeRef(*p));
        cur = p.0;
    }
    nodes_rev.reverse();
    edges_rev.reverse();
    Some((nodes_rev, edges_rev, total))
}
