//! `apoc.refactor.*`: graph refactoring (merge, clone, rename, invert, collapse, extract,
//! categorize).

use traverse_core::tx::view::GraphView;

use crate::apoc::key_string;
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::operators::call::ProcCall;
use crate::types::CypherValue;

use super::{edge_ref_of, ensure_label, node_ref_of};

/// `apoc.refactor.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.refactor.mergeNodes(nodes, {properties}) :: (node)
        "apoc.refactor.mergenodes" => {
            let mut out = Vec::new();
            for frame in frames {
                let nodes_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let CypherValue::List(items) = nodes_v else {
                    continue;
                };
                let ids: Vec<_> = items.iter().filter_map(node_ref_of).collect();
                let Some((&target, losers)) = ids.split_first() else {
                    continue;
                };
                let mode = match args.and_then(|a| a.get(1)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::Map(m) => match m.get("properties") {
                            Some(CypherValue::String(s)) => s.clone(),
                            _ => "combine".to_string(),
                        },
                        _ => "combine".to_string(),
                    },
                    None => "combine".to_string(),
                };
                for &loser in losers {
                    merge_node_into(graph, loser, target, &mode)?;
                }
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("node") {
                    nf[sl] = CypherValue::NodeRef(target).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        // apoc.refactor.cloneNodes(nodes [, withRels [, skipProps]]) :: (input, output, error)
        "apoc.refactor.clonenodes" => {
            let mut out = Vec::new();
            for frame in frames {
                let nodes_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let CypherValue::List(items) = nodes_v else {
                    continue;
                };
                let with_rels = matches!(
                    args.and_then(|a| a.get(1))
                        .map(|e| eval_expr(e, frame, &*graph, symtab))
                        .transpose()?,
                    Some(CypherValue::Bool(true))
                );
                let skip: Vec<String> = match args.and_then(|a| a.get(2)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::List(l) => l
                            .iter()
                            .filter_map(|v| match v {
                                CypherValue::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                    None => Vec::new(),
                };
                for item in &items {
                    let Some(nid) = node_ref_of(item) else {
                        continue;
                    };
                    let clone_id = clone_node(graph, nid, with_rels, &skip)?;
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("input") {
                        nf[sl] = CypherValue::Integer(nid.0 as i64).into();
                    }
                    if let Some(sl) = cx.slot_for("output") {
                        nf[sl] = CypherValue::NodeRef(clone_id).into();
                    }
                    if let Some(sl) = cx.slot_for("error") {
                        nf[sl] = CypherValue::Null.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // apoc.refactor.setType / invert / to / from :: (input, output, error)
        "apoc.refactor.settype"
        | "apoc.refactor.invert"
        | "apoc.refactor.to"
        | "apoc.refactor.from" => {
            let mut out = Vec::new();
            for frame in frames {
                let rel_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let Some(eid) = edge_ref_of(&rel_v) else {
                    continue;
                };
                let (new_type, new_src, new_dst) = match name_lower.as_str() {
                    "apoc.refactor.settype" => {
                        let t = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            o => {
                                return Err(CypherError::type_error(format!(
                                    "new type must be a STRING, got {}",
                                    o.type_name()
                                )));
                            }
                        };
                        (Some(t), None, None)
                    }
                    "apoc.refactor.invert" => {
                        let e = graph.get_edge(eid).map_err(CypherError::from)?;
                        (None, Some(e.target), Some(e.source))
                    }
                    "apoc.refactor.to" => {
                        let n = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                        let Some(nid) = node_ref_of(&n) else {
                            return Err(CypherError::type_error(
                                "apoc.refactor.to needs a NODE".to_string(),
                            ));
                        };
                        (None, None, Some(nid))
                    }
                    _ => {
                        let n = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                        let Some(nid) = node_ref_of(&n) else {
                            return Err(CypherError::type_error(
                                "apoc.refactor.from needs a NODE".to_string(),
                            ));
                        };
                        (None, Some(nid), None)
                    }
                };
                let new_eid = recreate_edge(graph, eid, new_type, new_src, new_dst)?;
                let e = graph.get_edge(new_eid).map_err(CypherError::from)?;
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("input") {
                    nf[sl] = CypherValue::Integer(eid.0 as i64).into();
                }
                if let Some(sl) = cx.slot_for("output") {
                    nf[sl] = CypherValue::EdgeRef {
                        id: new_eid,
                        edge_type_id: e.edge_type,
                        source: e.source,
                        target: e.target,
                    }
                    .into();
                }
                if let Some(sl) = cx.slot_for("error") {
                    nf[sl] = CypherValue::Null.into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        // apoc.refactor.rename.label(old, new) / rename.type(old, new)
        "apoc.refactor.rename.label" | "apoc.refactor.rename.type" => {
            let is_label = name_lower.ends_with("label");
            let mut out = Vec::new();
            for frame in frames {
                let old_name = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let new_name = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let t0 = web_time::Instant::now();
                let total = if is_label {
                    let ids = graph.nodes_by_label(&old_name);
                    for nid in &ids {
                        graph
                            .remove_label(*nid, &old_name)
                            .map_err(CypherError::from)?;
                        ensure_label(graph, *nid, &new_name)?;
                        crate::executor::inc_stat(|s| s.labels_removed += 1);
                    }
                    ids.len()
                } else {
                    let ids = graph.edges_by_type(&old_name);
                    for eid in &ids {
                        recreate_edge(graph, *eid, Some(new_name.clone()), None, None)?;
                    }
                    ids.len()
                };
                let mut nf = frame.clone();
                for (col, val) in [
                    ("batches", CypherValue::Integer(1)),
                    ("total", CypherValue::Integer(total as i64)),
                    (
                        "timeTaken",
                        CypherValue::Integer(t0.elapsed().as_secs() as i64),
                    ),
                    ("committedOperations", CypherValue::Integer(total as i64)),
                    ("failedOperations", CypherValue::Integer(0)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        // -- refactor extras ----------------------------------
        "apoc.refactor.mergerelationships" => {
            let mut out = Vec::new();
            for frame in frames {
                let rels = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l,
                    _ => Vec::new(),
                };
                let ids: Vec<_> = rels.iter().filter_map(edge_ref_of).collect();
                let Some((&keep, losers)) = ids.split_first() else {
                    continue;
                };
                for &l in losers {
                    // merge properties into the kept edge, first wins
                    let props = edge_property_map(&*graph, l);
                    graph.unindex_edge(keep);
                    for (k, v) in props {
                        let pid = graph.intern_property_key(&k);
                        if let Ok(e) = graph.get_edge_mut(keep)
                            && e.properties.get(pid).is_none()
                        {
                            e.properties.insert(pid, v);
                        }
                    }
                    graph.index_edge(keep);
                    graph.delete_edge(l).map_err(CypherError::from)?;
                    crate::executor::inc_stat(|s| s.relationships_deleted += 1);
                }
                let e = graph.get_edge(keep).map_err(CypherError::from)?;
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("rel") {
                    nf[sl] = CypherValue::EdgeRef {
                        id: keep,
                        edge_type_id: e.edge_type,
                        source: e.source,
                        target: e.target,
                    }
                    .into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.refactor.collapsenode" => {
            let mut out = Vec::new();
            for frame in frames {
                let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let rtype = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let nodes = match v {
                    CypherValue::List(l) => l,
                    other => vec![other],
                };
                for nv in &nodes {
                    let Some(nid) = node_ref_of(nv) else { continue };
                    use traverse_core::graph::types::Direction;
                    let ins = graph
                        .edges_of(nid, Direction::Incoming, None)
                        .unwrap_or_default();
                    let outs = graph
                        .edges_of(nid, Direction::Outgoing, None)
                        .unwrap_or_default();
                    let mut nf = frame.clone();
                    if ins.len() == 1 && outs.len() == 1 {
                        let (in_e, out_e) = (ins[0], outs[0]);
                        let src = graph.get_edge(in_e).map_err(CypherError::from)?.source;
                        let dst = graph.get_edge(out_e).map_err(CypherError::from)?.target;
                        // combined properties of both edges + the node
                        let mut props = edge_property_map(&*graph, in_e);
                        for (k, v) in edge_property_map(&*graph, out_e) {
                            props.entry(k).or_insert(v);
                        }
                        for (k, v) in node_property_map(&*graph, nid, &[]) {
                            props.entry(k).or_insert(v);
                        }
                        let new_eid = graph
                            .create_edge(src, dst, &rtype, props)
                            .map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.relationships_created += 1);
                        graph
                            .delete_node_cascading(nid)
                            .map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.nodes_deleted += 1);
                        let e = graph.get_edge(new_eid).map_err(CypherError::from)?;
                        if let Some(sl) = cx.slot_for("input") {
                            nf[sl] = CypherValue::Integer(nid.0 as i64).into();
                        }
                        if let Some(sl) = cx.slot_for("output") {
                            nf[sl] = CypherValue::EdgeRef {
                                id: new_eid,
                                edge_type_id: e.edge_type,
                                source: e.source,
                                target: e.target,
                            }
                            .into();
                        }
                        if let Some(sl) = cx.slot_for("error") {
                            nf[sl] = CypherValue::Null.into();
                        }
                    } else {
                        if let Some(sl) = cx.slot_for("input") {
                            nf[sl] = CypherValue::Integer(nid.0 as i64).into();
                        }
                        if let Some(sl) = cx.slot_for("error") {
                            nf[sl] = CypherValue::String(format!(
                                "node has {} incoming and {} outgoing relationships - collapse needs exactly 1+1",
                                ins.len(),
                                outs.len()
                            ))
                            .into();
                        }
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.refactor.extractnode" => {
            let mut out = Vec::new();
            for frame in frames {
                let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let labels: Vec<String> = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l
                        .iter()
                        .map(|x| match x {
                            CypherValue::String(s) => s.clone(),
                            o => o.to_string(),
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let out_t = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let in_t = match eval_expr(cx.arg(3)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let rels = match v {
                    CypherValue::List(l) => l,
                    other => vec![other],
                };
                for rv in &rels {
                    let Some(eid) = edge_ref_of(rv) else { continue };
                    let (src, dst) = {
                        let e = graph.get_edge(eid).map_err(CypherError::from)?;
                        (e.source, e.target)
                    };
                    let props = edge_property_map(&*graph, eid);
                    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let nid = graph
                        .create_node(&label_refs, props)
                        .map_err(CypherError::from)?;
                    graph
                        .create_edge(src, nid, &in_t, Default::default())
                        .map_err(CypherError::from)?;
                    graph
                        .create_edge(nid, dst, &out_t, Default::default())
                        .map_err(CypherError::from)?;
                    graph.delete_edge(eid).map_err(CypherError::from)?;
                    crate::executor::inc_stat(|s| {
                        s.nodes_created += 1;
                        s.relationships_created += 2;
                        s.relationships_deleted += 1;
                    });
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("input") {
                        nf[sl] = CypherValue::Integer(eid.0 as i64).into();
                    }
                    if let Some(sl) = cx.slot_for("output") {
                        nf[sl] = CypherValue::NodeRef(nid).into();
                    }
                    if let Some(sl) = cx.slot_for("error") {
                        nf[sl] = CypherValue::Null.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.refactor.normalizeasboolean" => {
            for frame in frames {
                let entity = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let key = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let trues = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l,
                    _ => Vec::new(),
                };
                let falses = match eval_expr(cx.arg(3)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l,
                    _ => Vec::new(),
                };
                let Some(nid) = node_ref_of(&entity) else {
                    continue;
                };
                let cur = graph
                    .get_node_property(nid, &key)
                    .ok()
                    .flatten()
                    .map(|pv| CypherValue::from_property(&pv));
                let Some(cur) = cur else { continue };
                let pid = graph.intern_property_key(&key);
                graph.unindex_node(nid);
                if trues.contains(&cur) {
                    if let Ok(n) = graph.get_node_mut(nid) {
                        n.properties.insert(
                            pid,
                            traverse_core::graph::property::PropertyValue::Bool(true),
                        );
                    }
                } else if falses.contains(&cur) {
                    if let Ok(n) = graph.get_node_mut(nid) {
                        n.properties.insert(
                            pid,
                            traverse_core::graph::property::PropertyValue::Bool(false),
                        );
                    }
                } else if let Ok(n) = graph.get_node_mut(nid) {
                    n.properties.remove(pid);
                }
                graph.index_node(nid);
                crate::executor::inc_stat(|s| s.properties_set += 1);
            }
            Ok(frames.to_vec())
        }
        "apoc.refactor.rename.nodeproperty" | "apoc.refactor.rename.typeproperty" => {
            let is_node = name_lower.contains("nodeproperty");
            let mut out = Vec::new();
            for frame in frames {
                let old_name = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let new_name = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let t0 = web_time::Instant::now();
                let mut total = 0usize;
                let old_pid = graph.intern_property_key(&old_name);
                let new_pid = graph.intern_property_key(&new_name);
                if is_node {
                    let scoped: Vec<_> = match cx.opt_arg(2, frame, &*graph) {
                        Some(CypherValue::List(l)) if !l.is_empty() => {
                            l.iter().filter_map(node_ref_of).collect()
                        }
                        _ => graph.nodes_vec(),
                    };
                    for nid in scoped {
                        let has = graph
                            .get_node(nid)
                            .map(|n| n.properties.get(old_pid).is_some())
                            .unwrap_or(false);
                        if !has {
                            continue;
                        }
                        graph.unindex_node(nid);
                        if let Ok(n) = graph.get_node_mut(nid)
                            && let Some(v) = n.properties.remove(old_pid)
                        {
                            n.properties.insert(new_pid, v);
                            total += 1;
                        }
                        graph.index_node(nid);
                    }
                } else {
                    let scoped: Vec<_> = match cx.opt_arg(2, frame, &*graph) {
                        Some(CypherValue::List(l)) if !l.is_empty() => {
                            l.iter().filter_map(edge_ref_of).collect()
                        }
                        _ => graph
                            .relationship_type_names()
                            .iter()
                            .flat_map(|t| graph.edges_by_type(t))
                            .collect(),
                    };
                    for eid in scoped {
                        let has = graph
                            .get_edge(eid)
                            .map(|e| e.properties.get(old_pid).is_some())
                            .unwrap_or(false);
                        if !has {
                            continue;
                        }
                        graph.unindex_edge(eid);
                        if let Ok(e) = graph.get_edge_mut(eid)
                            && let Some(v) = e.properties.remove(old_pid)
                        {
                            e.properties.insert(new_pid, v);
                            total += 1;
                        }
                        graph.index_edge(eid);
                    }
                }
                crate::executor::inc_stat(|s| s.properties_set += total as i64);
                let mut nf = frame.clone();
                for (col, val) in [
                    ("batches", CypherValue::Integer(1)),
                    ("total", CypherValue::Integer(total as i64)),
                    (
                        "timeTaken",
                        CypherValue::Integer(t0.elapsed().as_secs() as i64),
                    ),
                    ("committedOperations", CypherValue::Integer(total as i64)),
                    ("failedOperations", CypherValue::Integer(0)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.refactor.clonesubgraph" | "apoc.refactor.clonesubgraphfrompaths" => {
            let from_paths = name_lower.ends_with("frompaths");
            let mut out = Vec::new();
            for frame in frames {
                let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let (node_ids, rel_ids): (Vec<_>, Vec<_>) = if from_paths {
                    let mut ns = Vec::new();
                    let mut rs = Vec::new();
                    if let CypherValue::List(paths) = v {
                        for p in paths {
                            if let CypherValue::Path { nodes, edges } = p {
                                ns.extend(nodes.iter().filter_map(node_ref_of));
                                rs.extend(edges.iter().filter_map(edge_ref_of));
                            }
                        }
                    }
                    (ns, rs)
                } else {
                    let ns = match v {
                        CypherValue::List(l) => l.iter().filter_map(node_ref_of).collect(),
                        _ => Vec::new(),
                    };
                    let rs = match cx.opt_arg(1, frame, &*graph) {
                        Some(CypherValue::List(l)) => l.iter().filter_map(edge_ref_of).collect(),
                        _ => Vec::new(),
                    };
                    (ns, rs)
                };
                // clone nodes, then rels among the clones (the given list, or ALL rels among
                // the set)
                let mut mapping: std::collections::HashMap<u64, _> = Default::default();
                for nid in &node_ids {
                    if mapping.contains_key(&nid.0) {
                        continue;
                    }
                    let clone = clone_node(graph, *nid, false, &[])?;
                    mapping.insert(nid.0, clone);
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("input") {
                        nf[sl] = CypherValue::Integer(nid.0 as i64).into();
                    }
                    if let Some(sl) = cx.slot_for("output") {
                        nf[sl] = CypherValue::NodeRef(clone).into();
                    }
                    if let Some(sl) = cx.slot_for("error") {
                        nf[sl] = CypherValue::Null.into();
                    }
                    out.push(nf);
                }
                let rel_list: Vec<_> = if rel_ids.is_empty() {
                    use traverse_core::graph::types::Direction;
                    let mut rs = Vec::new();
                    for nid in &node_ids {
                        for eid in graph
                            .edges_of(*nid, Direction::Outgoing, None)
                            .unwrap_or_default()
                        {
                            if let Ok(e) = graph.get_edge(eid)
                                && mapping.contains_key(&e.target.0)
                            {
                                rs.push(eid);
                            }
                        }
                    }
                    rs
                } else {
                    rel_ids
                };
                for eid in rel_list {
                    let Ok(e) = graph.get_edge(eid) else { continue };
                    let (Some(&ns), Some(&nt)) =
                        (mapping.get(&e.source.0), mapping.get(&e.target.0))
                    else {
                        continue;
                    };
                    let t = graph.strings().resolve(e.edge_type.0).to_string();
                    let props = edge_property_map(&*graph, eid);
                    graph
                        .create_edge(ns, nt, &t, props)
                        .map_err(CypherError::from)?;
                    crate::executor::inc_stat(|s| s.relationships_created += 1);
                }
            }
            Ok(out)
        }
        "apoc.refactor.deleteandreconnect" => {
            let mut out = Vec::new();
            for frame in frames {
                let path = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let remove: Vec<_> = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l.iter().filter_map(node_ref_of).collect(),
                    _ => Vec::new(),
                };
                let CypherValue::Path { nodes, .. } = path else {
                    continue;
                };
                let path_ids: Vec<_> = nodes.iter().filter_map(node_ref_of).collect();
                let kept: Vec<_> = path_ids
                    .iter()
                    .copied()
                    .filter(|n| !remove.iter().any(|r| r.0 == n.0))
                    .collect();
                // reconnect kept nodes separated by a removed run with a `RELATED` relationship
                let rel_type = "RELATED".to_string();
                let mut new_rels: Vec<CypherValue> = Vec::new();
                for w in kept.windows(2) {
                    // Were they adjacent in the original path? If yes the
                    // original edge still exists post-delete only if no
                    // endpoint was removed - deletions below are cascade.
                    let a_pos = path_ids.iter().position(|x| x.0 == w[0].0).unwrap_or(0);
                    let b_pos = path_ids.iter().position(|x| x.0 == w[1].0).unwrap_or(0);
                    if b_pos > a_pos + 1 {
                        let eid = graph
                            .create_edge(w[0], w[1], &rel_type, Default::default())
                            .map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.relationships_created += 1);
                        let e = graph.get_edge(eid).map_err(CypherError::from)?;
                        new_rels.push(CypherValue::EdgeRef {
                            id: eid,
                            edge_type_id: e.edge_type,
                            source: e.source,
                            target: e.target,
                        });
                    }
                }
                for nid in &remove {
                    graph
                        .delete_node_cascading(*nid)
                        .map_err(CypherError::from)?;
                    crate::executor::inc_stat(|s| s.nodes_deleted += 1);
                }
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("nodes") {
                    nf[sl] =
                        CypherValue::List(kept.iter().map(|n| CypherValue::NodeRef(*n)).collect())
                            .into();
                }
                if let Some(sl) = cx.slot_for("relationships") {
                    nf[sl] = CypherValue::List(new_rels).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.refactor.categorize" => {
            for frame in frames {
                let source_key = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let rel_type = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let outgoing = matches!(
                    eval_expr(cx.arg(2)?, frame, &*graph, symtab)?,
                    CypherValue::Bool(true)
                );
                let label = match eval_expr(cx.arg(3)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let target_key = match eval_expr(cx.arg(4)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let mut categories: std::collections::HashMap<String, _> = Default::default();
                let src_pid = graph.intern_property_key(&source_key);
                for nid in graph.nodes_vec() {
                    let Some(val) = graph
                        .get_node(nid)
                        .ok()
                        .and_then(|n| n.properties.get(src_pid))
                    else {
                        continue;
                    };
                    let key_str = key_string(&CypherValue::from_property(&val));
                    let cat = match categories.get(&key_str) {
                        Some(c) => *c,
                        None => {
                            let mut props = traverse_core::graph::property::PropertyMap::new();
                            props.insert(target_key.clone(), val.clone());
                            let cat = graph
                                .create_node(&[label.as_str()], props)
                                .map_err(CypherError::from)?;
                            crate::executor::inc_stat(|s| s.nodes_created += 1);
                            categories.insert(key_str, cat);
                            cat
                        }
                    };
                    if outgoing {
                        graph
                            .create_edge(nid, cat, &rel_type, Default::default())
                            .map_err(CypherError::from)?;
                    } else {
                        graph
                            .create_edge(cat, nid, &rel_type, Default::default())
                            .map_err(CypherError::from)?;
                    }
                    crate::executor::inc_stat(|s| s.relationships_created += 1);
                    graph.unindex_node(nid);
                    if let Ok(n) = graph.get_node_mut(nid) {
                        n.properties.remove(src_pid);
                    }
                    graph.index_node(nid);
                }
            }
            Ok(frames.to_vec())
        }
        other => Err(super::unknown(other)),
    }
}

/// A node's properties as a string-keyed PropertyMap (for create calls).
fn node_property_map(
    graph: &dyn GraphView,
    nid: traverse_core::graph::types::NodeId,
    skip: &[String],
) -> traverse_core::graph::property::PropertyMap {
    let mut out = traverse_core::graph::property::PropertyMap::new();
    if let Ok(node) = graph.get_node(nid) {
        for (kid, pv) in node.properties.iter() {
            let k = graph.strings().resolve(kid.0).to_string();
            if !skip.contains(&k) {
                out.insert(k, pv.clone());
            }
        }
    }
    out
}

fn edge_property_map(
    graph: &dyn GraphView,
    eid: traverse_core::graph::types::EdgeId,
) -> traverse_core::graph::property::PropertyMap {
    let mut out = traverse_core::graph::property::PropertyMap::new();
    if let Ok(edge) = graph.get_edge(eid) {
        for (kid, pv) in edge.properties.iter() {
            out.insert(graph.strings().resolve(kid.0).to_string(), pv.clone());
        }
    }
    out
}

/// Recreates an edge with an optional new type / source / target, keeping its properties, then
/// deletes the original. Returns the new edge id.
fn recreate_edge(
    graph: &mut dyn GraphView,
    eid: traverse_core::graph::types::EdgeId,
    new_type: Option<String>,
    new_src: Option<traverse_core::graph::types::NodeId>,
    new_dst: Option<traverse_core::graph::types::NodeId>,
) -> Result<traverse_core::graph::types::EdgeId, CypherError> {
    let (src, dst, old_type) = {
        let e = graph.get_edge(eid).map_err(CypherError::from)?;
        (
            e.source,
            e.target,
            graph.strings().resolve(e.edge_type.0).to_string(),
        )
    };
    let props = edge_property_map(&*graph, eid);
    let t = new_type.unwrap_or(old_type);
    let new_eid = graph
        .create_edge(new_src.unwrap_or(src), new_dst.unwrap_or(dst), &t, props)
        .map_err(CypherError::from)?;
    graph.delete_edge(eid).map_err(CypherError::from)?;
    crate::executor::inc_stat(|s| {
        s.relationships_created += 1;
        s.relationships_deleted += 1;
    });
    Ok(new_eid)
}

/// Clones a node (labels + properties, optionally its relationships).
fn clone_node(
    graph: &mut dyn GraphView,
    nid: traverse_core::graph::types::NodeId,
    with_rels: bool,
    skip_props: &[String],
) -> Result<traverse_core::graph::types::NodeId, CypherError> {
    use traverse_core::graph::types::Direction;
    let labels: Vec<String> = graph
        .get_node(nid)
        .map(|n| {
            n.labels
                .iter()
                .map(|l| graph.strings().resolve(l.0).to_string())
                .collect()
        })
        .map_err(CypherError::from)?;
    let props = node_property_map(&*graph, nid, skip_props);
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let clone_id = graph
        .create_node(&label_refs, props)
        .map_err(CypherError::from)?;
    crate::executor::inc_stat(|s| s.nodes_created += 1);
    if with_rels {
        for eid in graph
            .edges_of(nid, Direction::Outgoing, None)
            .unwrap_or_default()
        {
            let Ok(e) = graph.get_edge(eid) else { continue };
            let t = graph.strings().resolve(e.edge_type.0).to_string();
            let target = e.target;
            let props = edge_property_map(&*graph, eid);
            graph
                .create_edge(clone_id, target, &t, props)
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.relationships_created += 1);
        }
        for eid in graph
            .edges_of(nid, Direction::Incoming, None)
            .unwrap_or_default()
        {
            let Ok(e) = graph.get_edge(eid) else { continue };
            let t = graph.strings().resolve(e.edge_type.0).to_string();
            let source = e.source;
            let props = edge_property_map(&*graph, eid);
            graph
                .create_edge(source, clone_id, &t, props)
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.relationships_created += 1);
        }
    }
    Ok(clone_id)
}

/// Merges `loser` into `target` (apoc.refactor.mergeNodes): moves every
/// relationship, combines properties per `mode` ("discard" keeps the target's,
/// "overwrite"/"override" prefers the loser's, "combine" makes collisions lists,
/// as APOC does), then deletes the loser.
fn merge_node_into(
    graph: &mut dyn GraphView,
    loser: traverse_core::graph::types::NodeId,
    target: traverse_core::graph::types::NodeId,
    mode: &str,
) -> Result<(), CypherError> {
    use traverse_core::graph::property::PropertyValue;
    use traverse_core::graph::types::Direction;
    if loser == target {
        return Ok(());
    }
    // move relationships (recreate on target, delete originals)
    for eid in graph
        .edges_of(loser, Direction::Outgoing, None)
        .unwrap_or_default()
    {
        recreate_edge(graph, eid, None, Some(target), None)?;
    }
    for eid in graph
        .edges_of(loser, Direction::Incoming, None)
        .unwrap_or_default()
    {
        // self-loops already moved by the outgoing pass
        if graph
            .get_edge(eid)
            .map(|e| e.source == loser)
            .unwrap_or(false)
        {
            continue;
        }
        recreate_edge(graph, eid, None, None, Some(target))?;
    }
    // union of labels
    let loser_labels: Vec<String> = graph
        .get_node(loser)
        .map(|n| {
            n.labels
                .iter()
                .map(|l| graph.strings().resolve(l.0).to_string())
                .collect()
        })
        .unwrap_or_default();
    for l in loser_labels {
        ensure_label(graph, target, &l)?;
    }
    // properties per mode
    let loser_props = node_property_map(&*graph, loser, &[]);
    graph.unindex_node(target);
    for (k, v) in loser_props {
        let pid = graph.intern_property_key(&k);
        let existing = graph
            .get_node(target)
            .ok()
            .and_then(|n| n.properties.get(pid));
        let merged = match (existing, mode) {
            (None, _) => Some(v),
            (Some(_), "discard") => None,
            (Some(_), "overwrite") | (Some(_), "override") => Some(v),
            (Some(old), _) => {
                // combine -> list of both (pre-existing lists flattened)
                let mut items = match old {
                    PropertyValue::List(l) => l.clone(),
                    other => vec![other.clone()],
                };
                match v {
                    PropertyValue::List(l) => items.extend(l),
                    other => items.push(other),
                }
                Some(PropertyValue::List(items))
            }
        };
        if let Some(pv) = merged
            && let Ok(node) = graph.get_node_mut(target)
        {
            node.properties.insert(pid, pv);
            crate::executor::inc_stat(|s| s.properties_set += 1);
        }
    }
    graph.index_node(target);
    graph
        .delete_node_cascading(loser)
        .map_err(CypherError::from)?;
    crate::executor::inc_stat(|s| s.nodes_deleted += 1);
    Ok(())
}
