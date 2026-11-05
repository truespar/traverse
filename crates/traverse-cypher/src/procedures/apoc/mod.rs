//! APOC procedures (`CALL apoc.*`), one module per namespace or family. Names arrive
//! lowercased: CALL matches procedure names case-insensitively.

mod create;
mod cypher;
mod export;
mod graph;
mod load;
mod merge;
mod meta;
mod nodes;
mod path;
mod periodic;
mod refactor;
mod search;
mod trigger;
mod util;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::frame::Frame;
use crate::types::CypherValue;

use super::ProcCall;

/// Runs an `apoc.*` procedure in its namespace's module.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let name = cx.name_lower.as_str();
    // a few procedures live with the namespace they share code with
    let ns = match name {
        "apoc.convert.totree" | "apoc.paths.tojsontree" => "apoc.graph",
        "apoc.load.arrow" => "apoc.export",
        // `apoc.<namespace>`, or the whole name for the top-level ones (`apoc.when`)
        _ => match name.match_indices('.').nth(1) {
            Some((end, _)) => &name[..end],
            None => name,
        },
    };
    match ns {
        "apoc.create" => create::call(cx, graph),
        "apoc.merge" => merge::call(cx, graph),
        "apoc.refactor" => refactor::call(cx, graph),
        "apoc.nodes" | "apoc.neighbors" => nodes::call(cx, graph),
        "apoc.periodic" => periodic::call(cx, graph),
        "apoc.cypher" | "apoc.when" | "apoc.case" | "apoc.do" => cypher::call(cx, graph),
        "apoc.load" => load::call(cx, graph),
        "apoc.export" | "apoc.import" => export::call(cx, graph),
        "apoc.path" | "apoc.algo" => path::call(cx, graph),
        "apoc.meta" | "apoc.schema" | "apoc.stats" => meta::call(cx, graph),
        "apoc.graph" | "apoc.generate" => graph::call(cx, graph),
        "apoc.trigger" => trigger::call(cx, graph),
        "apoc.search" | "apoc.spatial" => search::call(cx, graph),
        "apoc.util" | "apoc.lock" | "apoc.static" | "apoc.log" | "apoc.help" | "apoc.warmup"
        | "apoc.example" | "apoc.coll" | "apoc.text" | "apoc.math" | "apoc.atomic"
        | "apoc.convert" => util::call(cx, graph),
        _ => Err(unknown(name)),
    }
}

/// An `apoc.*` name outside the supported subset.
fn unknown(name: &str) -> CypherError {
    CypherError::semantic(format!(
        "Unknown procedure: {name} (not in Traverse's APOC compatibility subset)"
    ))
    .with_status(crate::status::GqlStatus::UnknownProcedure)
}

/// Label names from a string or a list of strings; anything else gives none.
fn labels_of(v: &CypherValue) -> Vec<String> {
    match v {
        CypherValue::List(l) => l
            .iter()
            .filter_map(|x| match x {
                CypherValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        CypherValue::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// APOC relationship filter: `TYPE>` out, `<TYPE` in, `TYPE` both, `|`-separated; empty = all
/// types, both directions.
fn parse_rel_specs(
    rel_filter: &str,
) -> Vec<(
    Option<String>,
    Option<traverse_core::graph::types::Direction>,
)> {
    use traverse_core::graph::types::Direction;
    rel_filter
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(t) = s.strip_suffix('>') {
                (
                    (!t.is_empty()).then(|| t.to_string()),
                    Some(Direction::Outgoing),
                )
            } else if let Some(t) = s.strip_prefix('<') {
                (
                    (!t.is_empty()).then(|| t.to_string()),
                    Some(Direction::Incoming),
                )
            } else {
                (Some(s.to_string()), None)
            }
        })
        .collect()
}

/// Expansion step of the APOC traversal procedures: (edge, neighbor) pairs from `nid` allowed by
/// the rel specs.
fn apoc_step(
    graph: &dyn GraphView,
    nid: traverse_core::graph::types::NodeId,
    rel_specs: &[(
        Option<String>,
        Option<traverse_core::graph::types::Direction>,
    )],
) -> Vec<(
    traverse_core::graph::types::EdgeId,
    traverse_core::graph::types::NodeId,
)> {
    use traverse_core::graph::types::Direction;
    let mut out = Vec::new();
    let dirsets: Vec<(Option<&str>, Direction)> = if rel_specs.is_empty() {
        vec![(None, Direction::Outgoing), (None, Direction::Incoming)]
    } else {
        rel_specs
            .iter()
            .flat_map(|(t, d)| match d {
                Some(d) => vec![(t.as_deref(), *d)],
                None => vec![
                    (t.as_deref(), Direction::Outgoing),
                    (t.as_deref(), Direction::Incoming),
                ],
            })
            .collect()
    };
    for (t, d) in dirsets {
        for eid in graph.edges_of(nid, d, t).unwrap_or_default() {
            let Ok(e) = graph.get_edge(eid) else { continue };
            let other = if e.source == nid { e.target } else { e.source };
            out.push((eid, other));
        }
    }
    out
}

/// APOC path expansion (`apoc.path.expand` / `subgraphNodes` / `spanningTree`):
/// BFS from `start` with APOC's relationship filter (`TYPE>` out, `<TYPE` in,
/// `TYPE` both, `|`-separated, empty = all) and label filter (`+L` whitelist /
/// `-L` blacklist / bare = whitelist, for nodes after the start).
///
/// * `expand`: every path with depth in [min, max], per-path relationship
///   uniqueness (APOC's RELATIONSHIP_PATH default).
/// * `subgraphnodes` / `spanningtree`: NODE_GLOBAL uniqueness, each node once
///   (spanningTree yields the first-visit path, subgraphNodes the node; depth 0
///   = the start itself).
fn apoc_expand(
    graph: &dyn GraphView,
    start: traverse_core::graph::types::NodeId,
    rel_filter: &str,
    label_filter: &str,
    min_level: i64,
    max_level: i64,
    mode: &str,
) -> Result<Vec<(Vec<CypherValue>, Vec<CypherValue>)>, CypherError> {
    use traverse_core::graph::types::Direction;

    let rel_specs = parse_rel_specs(rel_filter);
    let (mut white, mut black) = (Vec::new(), Vec::new());
    for f in label_filter
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(l) = f.strip_prefix('-') {
            black.push(l.to_string());
        } else {
            white.push(f.trim_start_matches('+').to_string());
        }
    }
    let node_ok = |nid: traverse_core::graph::types::NodeId| -> bool {
        let Ok(node) = graph.get_node(nid) else {
            return false;
        };
        let names: Vec<String> = node
            .labels
            .iter()
            .map(|l| graph.strings().resolve(l.0).to_string())
            .collect();
        if names.iter().any(|n| black.contains(n)) {
            return false;
        }
        white.is_empty() || names.iter().any(|n| white.contains(n))
    };
    let node_val = |nid| CypherValue::NodeRef(nid);
    let edge_val = |eid: traverse_core::graph::types::EdgeId| -> Option<CypherValue> {
        let e = graph.get_edge(eid).ok()?;
        Some(CypherValue::EdgeRef {
            id: eid,
            edge_type_id: e.edge_type,
            source: e.source,
            target: e.target,
        })
    };
    // expansion step: (edge, neighbor) pairs allowed by the rel specs
    let step = |nid: traverse_core::graph::types::NodeId| -> Vec<(
        traverse_core::graph::types::EdgeId,
        traverse_core::graph::types::NodeId,
    )> {
        let mut out = Vec::new();
        let dirsets: Vec<(Option<&str>, Direction)> = if rel_specs.is_empty() {
            vec![(None, Direction::Outgoing), (None, Direction::Incoming)]
        } else {
            rel_specs
                .iter()
                .flat_map(|(t, d)| match d {
                    Some(d) => vec![(t.as_deref(), *d)],
                    None => vec![
                        (t.as_deref(), Direction::Outgoing),
                        (t.as_deref(), Direction::Incoming),
                    ],
                })
                .collect()
        };
        for (t, d) in dirsets {
            for eid in graph.edges_of(nid, d, t).unwrap_or_default() {
                let Ok(e) = graph.get_edge(eid) else { continue };
                let other = if e.source == nid { e.target } else { e.source };
                out.push((eid, other));
            }
        }
        out
    };

    let cap = if max_level < 0 { i64::MAX } else { max_level };
    let mut results = Vec::new();
    if mode == "expand" {
        // DFS over paths, per-path edge uniqueness
        let mut stack: Vec<(Vec<CypherValue>, Vec<CypherValue>, Vec<u64>, _)> =
            vec![(vec![node_val(start)], Vec::new(), Vec::new(), start)];
        while let Some((nodes, edges, used, tail)) = stack.pop() {
            let depth = edges.len() as i64;
            if depth >= min_level {
                results.push((nodes.clone(), edges.clone()));
            }
            if depth >= cap {
                continue;
            }
            for (eid, other) in step(tail) {
                if used.contains(&eid.0) || !node_ok(other) {
                    continue;
                }
                let Some(ev) = edge_val(eid) else { continue };
                let mut n2 = nodes.clone();
                n2.push(node_val(other));
                let mut e2 = edges.clone();
                e2.push(ev);
                let mut u2 = used.clone();
                u2.push(eid.0);
                stack.push((n2, e2, u2, other));
            }
            crate::query::check_interrupt()?;
        }
    } else {
        // BFS, node-global uniqueness; first-visit path per node
        let mut visited: std::collections::HashSet<u64> = [start.0].into();
        let mut queue: std::collections::VecDeque<(Vec<CypherValue>, Vec<CypherValue>, _)> =
            [(vec![node_val(start)], Vec::new(), start)].into();
        while let Some((nodes, edges, tail)) = queue.pop_front() {
            let depth = edges.len() as i64;
            if depth >= min_level.max(0) {
                results.push((nodes.clone(), edges.clone()));
            }
            if depth >= cap {
                continue;
            }
            for (eid, other) in step(tail) {
                if visited.contains(&other.0) || !node_ok(other) {
                    continue;
                }
                visited.insert(other.0);
                let Some(ev) = edge_val(eid) else { continue };
                let mut n2 = nodes.clone();
                n2.push(node_val(other));
                let mut e2 = edges.clone();
                e2.push(ev);
                queue.push_back((n2, e2, other));
            }
            crate::query::check_interrupt()?;
        }
    }
    Ok(results)
}

/// Adds a label unless present (Graph::add_label errors on duplicates).
fn ensure_label(
    graph: &mut dyn GraphView,
    nid: traverse_core::graph::types::NodeId,
    label: &str,
) -> Result<(), CypherError> {
    let has = graph
        .get_node(nid)
        .map(|n| {
            n.labels
                .iter()
                .any(|l| graph.strings().resolve(l.0) == label)
        })
        .unwrap_or(false);
    if !has {
        graph.add_label(nid, label).map_err(CypherError::from)?;
        crate::executor::inc_stat(|s| s.labels_added += 1);
    }
    Ok(())
}

/// Edge id of an edge-ish value.
fn edge_ref_of(v: &CypherValue) -> Option<traverse_core::graph::types::EdgeId> {
    match v {
        CypherValue::EdgeRef { id, .. } => Some(*id),
        CypherValue::Edge { id, .. } => Some(*id),
        _ => None,
    }
}

/// Node id of a node-ish value.
fn node_ref_of(v: &CypherValue) -> Option<traverse_core::graph::types::NodeId> {
    match v {
        CypherValue::NodeRef(id) => Some(*id),
        CypherValue::Node { id, .. } => Some(*id),
        _ => None,
    }
}
