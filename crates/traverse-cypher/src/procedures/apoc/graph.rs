//! `apoc.graph.*`, `apoc.generate.*` and the tree converters: virtual graphs from data, generated
//! graphs, trees from paths.

use traverse_core::tx::view::GraphView;

use crate::apoc::next_virtual_id;
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::{ProcCall, run_inner_cypher};
use crate::types::CypherValue;

use super::node_ref_of;

/// `apoc.graph.*`, `apoc.generate.*`, `apoc.convert.toTree` and
/// `apoc.paths.toJsonTree`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall { frames, symtab, .. } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        "apoc.convert.totree" | "apoc.paths.tojsontree" => {
            let mut out = Vec::new();
            for frame in frames {
                let CypherValue::List(paths) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                else {
                    continue;
                };
                let lower = !matches!(
                    cx.opt_arg(1, frame, &*graph),
                    Some(CypherValue::Bool(false))
                );
                let trees = paths_to_tree(&*graph, &paths, lower)?;
                for t in trees {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = t.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.graph.from"
        | "apoc.graph.fromdata"
        | "apoc.graph.frompath"
        | "apoc.graph.frompaths"
        | "apoc.graph.fromdb"
        | "apoc.graph.fromcypher" => {
            let mut out = Vec::new();
            for frame in frames {
                let (nodes, rels, name_idx, props_idx) = match name_lower.as_str() {
                    "apoc.graph.fromdata" => {
                        let n = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                            CypherValue::List(l) => l,
                            _ => Vec::new(),
                        };
                        let r = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                            CypherValue::List(l) => l,
                            _ => Vec::new(),
                        };
                        (n, r, 2usize, 3usize)
                    }
                    "apoc.graph.frompath" => {
                        let (mut n, mut r) = (Vec::new(), Vec::new());
                        if let CypherValue::Path { nodes, edges } =
                            eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                        {
                            n = nodes;
                            r = edges;
                        }
                        (n, r, 1, 2)
                    }
                    "apoc.graph.frompaths" => {
                        let (mut n, mut r) = (Vec::new(), Vec::new());
                        if let CypherValue::List(paths) =
                            eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                        {
                            for p in paths {
                                if let CypherValue::Path { nodes, edges } = p {
                                    n.extend(nodes);
                                    r.extend(edges);
                                }
                            }
                        }
                        (n, r, 1, 2)
                    }
                    "apoc.graph.fromdb" => {
                        let n: Vec<CypherValue> = graph
                            .nodes_vec()
                            .into_iter()
                            .take(10_000)
                            .map(CypherValue::NodeRef)
                            .collect();
                        let mut r = Vec::new();
                        for t in graph.relationship_type_names() {
                            for eid in graph.edges_by_type(&t).into_iter().take(10_000) {
                                if let Ok(e) = graph.get_edge(eid) {
                                    r.push(CypherValue::EdgeRef {
                                        id: eid,
                                        edge_type_id: e.edge_type,
                                        source: e.source,
                                        target: e.target,
                                    });
                                }
                            }
                        }
                        (n, r, 0, 1)
                    }
                    "apoc.graph.fromcypher" => {
                        let stmt = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => continue,
                        };
                        let params = match cx.opt_arg(1, frame, &*graph) {
                            Some(CypherValue::Map(m)) => m.into_iter().collect(),
                            _ => Default::default(),
                        };
                        let result = run_inner_cypher(graph, &stmt, params)?;
                        let mut n = Vec::new();
                        let mut r = Vec::new();
                        for row in &result.rows {
                            for v in row {
                                crate::executor::collect_graph_parts(v, &mut n, &mut r);
                            }
                        }
                        (n, r, 2, 3)
                    }
                    _ => {
                        // apoc.graph.from(data, name, props)
                        let mut n = Vec::new();
                        let mut r = Vec::new();
                        let data = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                        crate::executor::collect_graph_parts(&data, &mut n, &mut r);
                        (n, r, 1, 2)
                    }
                };
                let gname = match cx.opt_arg(name_idx, frame, &*graph) {
                    Some(CypherValue::String(s)) => s,
                    _ => "graph".to_string(),
                };
                let props = match cx.opt_arg(props_idx, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let mut g = std::collections::BTreeMap::new();
                g.insert("name".to_string(), CypherValue::String(gname));
                g.insert("nodes".to_string(), CypherValue::List(nodes));
                g.insert("relationships".to_string(), CypherValue::List(rels));
                g.insert("properties".to_string(), CypherValue::Map(props));
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("graph") {
                    nf[sl] = CypherValue::Map(g).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.graph.fromdocument" | "apoc.graph.validatedocument" => {
            let validate = name_lower.ends_with("validatedocument");
            let mut out = Vec::new();
            for frame in frames {
                let doc = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let doc = match doc {
                    CypherValue::String(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                        Ok(j) => crate::apoc::json_to_cypher(&j),
                        Err(e) => {
                            return Err(CypherError::semantic(format!("invalid JSON: {e}")));
                        }
                    },
                    other => other,
                };
                if validate {
                    // Minimal validation: every object needs an id-ish
                    // and type-ish field to become a node.
                    let mut issues: Vec<CypherValue> = Vec::new();
                    validate_document(&doc, &mut issues);
                    for issue in issues {
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("row") {
                            nf[sl] = issue.into();
                        }
                        out.push(nf);
                    }
                } else {
                    let (nodes, rels) = document_to_virtual_graph(&doc);
                    let mut g = std::collections::BTreeMap::new();
                    g.insert("name".to_string(), CypherValue::String("Graph".to_string()));
                    g.insert("nodes".to_string(), CypherValue::List(nodes));
                    g.insert("relationships".to_string(), CypherValue::List(rels));
                    g.insert(
                        "properties".to_string(),
                        CypherValue::Map(Default::default()),
                    );
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("graph") {
                        nf[sl] = CypherValue::Map(g).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // -- apoc.generate.* (APOC extended: graph generators) --
        "apoc.generate.ba"
        | "apoc.generate.ws"
        | "apoc.generate.er"
        | "apoc.generate.complete"
        | "apoc.generate.simple" => {
            for frame in frames {
                let (label, rtype, spec) = match name_lower.as_str() {
                    "apoc.generate.complete" => (
                        cx.opt_arg(1, frame, &*graph),
                        cx.opt_arg(2, frame, &*graph),
                        GenerateSpec::Complete {
                            n: int_of(&eval_expr(cx.arg(0)?, frame, &*graph, symtab)?),
                        },
                    ),
                    "apoc.generate.simple" => {
                        let degrees: Vec<i64> = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                        {
                            CypherValue::List(l) => l
                                .iter()
                                .filter_map(|v| match v {
                                    CypherValue::Integer(i) => Some(*i),
                                    _ => None,
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        (
                            cx.opt_arg(1, frame, &*graph),
                            cx.opt_arg(2, frame, &*graph),
                            GenerateSpec::Simple { degrees },
                        )
                    }
                    "apoc.generate.ba" => (
                        cx.opt_arg(2, frame, &*graph),
                        cx.opt_arg(3, frame, &*graph),
                        GenerateSpec::Ba {
                            n: int_of(&eval_expr(cx.arg(0)?, frame, &*graph, symtab)?),
                            m: int_of(&eval_expr(cx.arg(1)?, frame, &*graph, symtab)?),
                        },
                    ),
                    "apoc.generate.er" => (
                        cx.opt_arg(2, frame, &*graph),
                        cx.opt_arg(3, frame, &*graph),
                        GenerateSpec::Er {
                            n: int_of(&eval_expr(cx.arg(0)?, frame, &*graph, symtab)?),
                            edges: int_of(&eval_expr(cx.arg(1)?, frame, &*graph, symtab)?),
                        },
                    ),
                    _ => (
                        cx.opt_arg(3, frame, &*graph),
                        cx.opt_arg(4, frame, &*graph),
                        GenerateSpec::Ws {
                            n: int_of(&eval_expr(cx.arg(0)?, frame, &*graph, symtab)?),
                            degree: int_of(&eval_expr(cx.arg(1)?, frame, &*graph, symtab)?),
                            beta: match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                                CypherValue::Float(f) => f,
                                CypherValue::Integer(i) => i as f64,
                                _ => 0.5,
                            },
                        },
                    ),
                };
                let label = match label {
                    Some(CypherValue::String(s)) => s,
                    // APOC's generators default to Person/FRIEND_OF.
                    _ => "Person".to_string(),
                };
                let rtype = match rtype {
                    Some(CypherValue::String(s)) => s,
                    _ => "FRIEND_OF".to_string(),
                };
                generate_graph(graph, &label, &rtype, spec)?;
            }
            Ok(frames.to_vec())
        }
        other => Err(super::unknown(other)),
    }
}

/// apoc.convert.toTree: nests paths into trees, root = first node of each path, children grouped
/// under relationship-type keys (lowercased by default). One tree map per distinct root.
fn paths_to_tree(
    graph: &dyn GraphView,
    paths: &[CypherValue],
    lower: bool,
) -> Result<Vec<CypherValue>, CypherError> {
    use std::collections::BTreeMap;
    // node id -> materialized map (props + _type + _id)
    let mut node_maps: BTreeMap<u64, BTreeMap<String, CypherValue>> = BTreeMap::new();
    // parent id -> (rel key -> child ids, ordered, deduped)
    let mut children: BTreeMap<u64, BTreeMap<String, Vec<u64>>> = BTreeMap::new();
    let mut roots: Vec<u64> = Vec::new();
    let mut is_child: std::collections::HashSet<u64> = Default::default();
    let mat = |graph: &dyn GraphView, id: traverse_core::graph::types::NodeId| {
        let mut m = BTreeMap::new();
        if let Ok(n) = graph.get_node(id) {
            m.insert("_id".to_string(), CypherValue::Integer(id.0 as i64));
            m.insert(
                "_type".to_string(),
                CypherValue::String(
                    n.labels
                        .iter()
                        .map(|l| graph.strings().resolve(l.0).to_string())
                        .collect::<Vec<_>>()
                        .join(":"),
                ),
            );
            for (k, pv) in n.properties.iter() {
                m.insert(
                    graph.strings().resolve(k.0).to_string(),
                    CypherValue::from_property(&pv),
                );
            }
        }
        m
    };
    for p in paths {
        let CypherValue::Path { nodes, edges } = p else {
            continue;
        };
        let ids: Vec<_> = nodes.iter().filter_map(node_ref_of).collect();
        for id in &ids {
            node_maps.entry(id.0).or_insert_with(|| mat(graph, *id));
        }
        if let Some(first) = ids.first()
            && !roots.contains(&first.0)
        {
            roots.push(first.0);
        }
        for (i, ev) in edges.iter().enumerate() {
            let (Some(parent), Some(child)) = (ids.get(i), ids.get(i + 1)) else {
                continue;
            };
            let key = match ev {
                CypherValue::EdgeRef { edge_type_id, .. } => {
                    graph.strings().resolve(edge_type_id.0).to_string()
                }
                CypherValue::Edge { edge_type, .. } => edge_type.clone(),
                _ => "REL".to_string(),
            };
            let key = if lower { key.to_lowercase() } else { key };
            let kids = children
                .entry(parent.0)
                .or_default()
                .entry(key)
                .or_default();
            if !kids.contains(&child.0) {
                kids.push(child.0);
            }
            is_child.insert(child.0);
        }
    }
    #[recursive::recursive]
    fn build(
        id: u64,
        node_maps: &std::collections::BTreeMap<
            u64,
            std::collections::BTreeMap<String, CypherValue>,
        >,
        children: &std::collections::BTreeMap<u64, std::collections::BTreeMap<String, Vec<u64>>>,
        depth: usize,
    ) -> CypherValue {
        let mut m = node_maps.get(&id).cloned().unwrap_or_default();
        if depth < 32
            && let Some(kids) = children.get(&id)
        {
            for (key, ids) in kids {
                m.insert(
                    key.clone(),
                    CypherValue::List(
                        ids.iter()
                            .map(|c| build(*c, node_maps, children, depth + 1))
                            .collect(),
                    ),
                );
            }
        }
        CypherValue::Map(m)
    }
    Ok(roots
        .into_iter()
        .filter(|r| !is_child.contains(r))
        .map(|r| build(r, &node_maps, &children, 0))
        .collect())
}

/// Minimal fromDocument: objects become virtual nodes typed by their `type` field (default 'Node');
/// nested objects/arrays become children linked by virtual relationships named after the key.
fn document_to_virtual_graph(doc: &CypherValue) -> (Vec<CypherValue>, Vec<CypherValue>) {
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    #[recursive::recursive]
    fn walk(
        v: &CypherValue,
        nodes: &mut Vec<CypherValue>,
        rels: &mut Vec<CypherValue>,
    ) -> Option<traverse_core::graph::types::NodeId> {
        let CypherValue::Map(m) = v else { return None };
        let vid = next_virtual_id();
        let label = match m.get("type") {
            Some(CypherValue::String(t)) => t.clone(),
            _ => "Node".to_string(),
        };
        let mut props = traverse_core::graph::property::PropertyMap::new();
        for (k, val) in m {
            match val {
                CypherValue::Map(_) => {
                    if let Some(child) = walk(val, nodes, rels) {
                        rels.push(CypherValue::Edge {
                            id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                            edge_type: k.to_uppercase(),
                            source: vid,
                            target: child,
                            properties: Default::default(),
                        });
                    }
                }
                CypherValue::List(items)
                    if items.iter().any(|i| matches!(i, CypherValue::Map(_))) =>
                {
                    for item in items {
                        if let Some(child) = walk(item, nodes, rels) {
                            rels.push(CypherValue::Edge {
                                id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                                edge_type: k.to_uppercase(),
                                source: vid,
                                target: child,
                                properties: Default::default(),
                            });
                        }
                    }
                }
                other => {
                    if let Some(pv) = other.to_property() {
                        props.insert(k.clone(), pv);
                    }
                }
            }
        }
        nodes.push(CypherValue::Node {
            id: vid,
            labels: vec![label],
            properties: props,
        });
        Some(vid)
    }
    match doc {
        CypherValue::List(items) => {
            for item in items {
                walk(item, &mut nodes, &mut rels);
            }
        }
        other => {
            walk(other, &mut nodes, &mut rels);
        }
    }
    (nodes, rels)
}

/// Minimal validateDocument: reports objects missing `id` or `type`.
#[recursive::recursive]
fn validate_document(doc: &CypherValue, issues: &mut Vec<CypherValue>) {
    match doc {
        CypherValue::Map(m) => {
            let mut missing: Vec<CypherValue> = Vec::new();
            if !m.contains_key("id") {
                missing.push(CypherValue::String("id".to_string()));
            }
            if !m.contains_key("type") {
                missing.push(CypherValue::String("type".to_string()));
            }
            if !missing.is_empty() {
                let mut row = std::collections::BTreeMap::new();
                row.insert(
                    "message".to_string(),
                    CypherValue::String("The object needs the following field(s)".to_string()),
                );
                row.insert("missing".to_string(), CypherValue::List(missing));
                issues.push(CypherValue::Map(row));
            }
            for v in m.values() {
                validate_document(v, issues);
            }
        }
        CypherValue::List(l) => {
            for v in l {
                validate_document(v, issues);
            }
        }
        _ => {}
    }
}

fn int_of(v: &CypherValue) -> i64 {
    match v {
        CypherValue::Integer(i) => *i,
        CypherValue::Float(f) => *f as i64,
        _ => 0,
    }
}

enum GenerateSpec {
    Ba { n: i64, m: i64 },
    Ws { n: i64, degree: i64, beta: f64 },
    Er { n: i64, edges: i64 },
    Complete { n: i64 },
    Simple { degrees: Vec<i64> },
}

/// APOC extended graph generators (apoc.generate.*): `n` nodes with `label`, connected per the
/// chosen model with `rtype` edges.
fn generate_graph(
    graph: &mut dyn GraphView,
    label: &str,
    rtype: &str,
    spec: GenerateSpec,
) -> Result<(), CypherError> {
    let mut rng: u64 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1
    };
    let mut next = |bound: usize| -> usize {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((rng >> 33) as usize) % bound.max(1)
    };
    let mk_nodes = |graph: &mut dyn GraphView,
                    n: usize|
     -> Result<Vec<traverse_core::graph::types::NodeId>, CypherError> {
        let mut ids = Vec::with_capacity(n);
        for _ in 0..n {
            let id = graph
                .create_node(&[label], Default::default())
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.nodes_created += 1);
            ids.push(id);
        }
        Ok(ids)
    };
    let connect = |graph: &mut dyn GraphView,
                   a: traverse_core::graph::types::NodeId,
                   b: traverse_core::graph::types::NodeId|
     -> Result<(), CypherError> {
        graph
            .create_edge(a, b, rtype, Default::default())
            .map_err(CypherError::from)?;
        crate::executor::inc_stat(|s| s.relationships_created += 1);
        Ok(())
    };
    match spec {
        GenerateSpec::Complete { n } => {
            let ids = mk_nodes(graph, n.max(0) as usize)?;
            for i in 0..ids.len() {
                for j in i + 1..ids.len() {
                    connect(graph, ids[i], ids[j])?;
                }
            }
        }
        GenerateSpec::Er { n, edges } => {
            let ids = mk_nodes(graph, n.max(0) as usize)?;
            if ids.len() >= 2 {
                let mut used: std::collections::HashSet<(usize, usize)> = Default::default();
                let max_edges = ids.len() * (ids.len() - 1) / 2;
                let want = (edges.max(0) as usize).min(max_edges);
                while used.len() < want {
                    let (mut i, mut j) = (next(ids.len()), next(ids.len()));
                    if i == j {
                        continue;
                    }
                    if i > j {
                        std::mem::swap(&mut i, &mut j);
                    }
                    if used.insert((i, j)) {
                        connect(graph, ids[i], ids[j])?;
                    }
                }
            }
        }
        GenerateSpec::Ba { n, m } => {
            let n = n.max(2) as usize;
            let m = (m.max(1) as usize).min(n - 1);
            let ids = mk_nodes(graph, n)?;
            // degree-proportional attachment via the repeated-endpoint list
            let mut endpoints: Vec<usize> = Vec::new();
            // seed: connect node 1 to node 0
            connect(graph, ids[0], ids[1])?;
            endpoints.extend([0, 1]);
            for i in 2..n {
                let mut chosen: std::collections::HashSet<usize> = Default::default();
                let want = m.min(i);
                let mut guard = 0;
                while chosen.len() < want && guard < 10_000 {
                    guard += 1;
                    let pick = endpoints[next(endpoints.len())];
                    if pick != i {
                        chosen.insert(pick);
                    }
                }
                for &t in &chosen {
                    connect(graph, ids[i], ids[t])?;
                    endpoints.extend([i, t]);
                }
            }
        }
        GenerateSpec::Ws { n, degree, beta } => {
            let n = n.max(3) as usize;
            // ring lattice degree must be even and < n
            let k = ((degree.max(2) as usize) & !1usize).min(n - 1);
            let ids = mk_nodes(graph, n)?;
            for i in 0..n {
                for d in 1..=k / 2 {
                    let j = (i + d) % n;
                    // rewire with probability beta to a uniform random non-self node
                    let target = if (next(1_000_000) as f64) / 1_000_000.0 < beta {
                        let mut t = next(n);
                        let mut guard = 0;
                        while t == i && guard < 100 {
                            t = next(n);
                            guard += 1;
                        }
                        t
                    } else {
                        j
                    };
                    if target != i {
                        connect(graph, ids[i], ids[target])?;
                    }
                }
            }
        }
        GenerateSpec::Simple { degrees } => {
            // Havel-Hakimi; errors on non-graphical sequences
            let n = degrees.len();
            let ids = mk_nodes(graph, n)?;
            let mut remaining: Vec<(i64, usize)> = degrees.iter().copied().zip(0..n).collect();
            loop {
                remaining.sort_by_key(|r| std::cmp::Reverse(r.0));
                let Some(&(d, v)) = remaining.first() else {
                    break;
                };
                if d == 0 {
                    break;
                }
                if (d as usize) >= remaining.len() {
                    return Err(CypherError::semantic(
                        "apoc.generate.simple: degree sequence is not graphical".to_string(),
                    ));
                }
                remaining[0].0 = 0;
                for idx in 1..=(d as usize) {
                    if remaining[idx].0 == 0 {
                        return Err(CypherError::semantic(
                            "apoc.generate.simple: degree sequence is not graphical".to_string(),
                        ));
                    }
                    remaining[idx].0 -= 1;
                    let u = remaining[idx].1;
                    connect(graph, ids[v], ids[u])?;
                }
            }
        }
    }
    Ok(())
}
