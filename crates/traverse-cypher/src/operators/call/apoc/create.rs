//! `apoc.create.*`: nodes, relationships, labels and properties, virtual entities and paths.

use traverse_core::tx::view::GraphView;

use crate::apoc::{key_string, next_virtual_id};
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::operators::call::ProcCall;
use crate::types::CypherValue;

use super::{edge_ref_of, ensure_label, labels_of, node_ref_of};

/// `apoc.create.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall { frames, symtab, .. } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.create.node(labels, props) :: (node)
        // apoc.create.nodes(labels, propsList) :: (node)
        "apoc.create.node" | "apoc.create.nodes" => {
            let many = name_lower.ends_with('s');
            let mut out = Vec::new();
            for frame in frames {
                let labels_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let labels = labels_of(&labels_v);
                let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let props_v = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                let prop_maps: Vec<CypherValue> = if many {
                    match props_v {
                        CypherValue::List(l) => l,
                        CypherValue::Null => Vec::new(),
                        other => vec![other],
                    }
                } else {
                    vec![props_v]
                };
                for pm in prop_maps {
                    let props = crate::executor::cypher_value_to_property_map(&pm)?;
                    let nid = graph
                        .create_node(&label_refs, props)
                        .map_err(CypherError::from)?;
                    crate::executor::inc_stat(|s| s.nodes_created += 1);
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("node") {
                        nf[sl] = CypherValue::NodeRef(nid).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // apoc.create.relationship(from, type, props, to) :: (rel)
        "apoc.create.relationship" => {
            let mut out = Vec::new();
            for frame in frames {
                let from = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let rtype = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    o => {
                        return Err(CypherError::type_error(format!(
                            "relationship type must be a STRING, got {}",
                            o.type_name()
                        )));
                    }
                };
                let props_v = eval_expr(cx.arg(2)?, frame, &*graph, symtab)?;
                let to = eval_expr(cx.arg(3)?, frame, &*graph, symtab)?;
                let (Some(src_id), Some(dst_id)) = (node_ref_of(&from), node_ref_of(&to)) else {
                    return Err(CypherError::type_error(
                        "apoc.create.relationship needs NODE start and end".to_string(),
                    ));
                };
                let props = crate::executor::cypher_value_to_property_map(&props_v)?;
                let eid = graph
                    .create_edge(src_id, dst_id, &rtype, props)
                    .map_err(CypherError::from)?;
                crate::executor::inc_stat(|s| s.relationships_created += 1);
                let tid = traverse_core::graph::types::EdgeTypeId(
                    graph.strings().get(&rtype).unwrap_or_default(),
                );
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("rel") {
                    nf[sl] = CypherValue::EdgeRef {
                        id: eid,
                        edge_type_id: tid,
                        source: src_id,
                        target: dst_id,
                    }
                    .into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        // apoc.create.uuids(count) :: (row, uuid)
        "apoc.create.uuids" => {
            let mut out = Vec::new();
            for frame in frames {
                let count = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::Integer(n) if n >= 0 => n,
                    _ => 0,
                };
                for i in 0..count {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("row") {
                        nf[sl] = CypherValue::Integer(i).into();
                    }
                    if let Some(sl) = cx.slot_for("uuid") {
                        let uuid = crate::functions::call_function("randomuuid", &[])?;
                        nf[sl] = uuid.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // apoc.create.addLabels / removeLabels / setLabels :: (node)
        "apoc.create.addlabels" | "apoc.create.removelabels" | "apoc.create.setlabels" => {
            let mut out = Vec::new();
            for frame in frames {
                let nodes_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let node_items = match nodes_v {
                    CypherValue::List(l) => l,
                    other => vec![other],
                };
                let labels_v = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                let labels: Vec<String> = match labels_v {
                    CypherValue::List(l) => l
                        .iter()
                        .filter_map(|v| match v {
                            CypherValue::String(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect(),
                    CypherValue::String(s) => vec![s],
                    _ => Vec::new(),
                };
                for item in &node_items {
                    let Some(nid) = node_ref_of(item) else {
                        continue;
                    };
                    match name_lower.as_str() {
                        "apoc.create.addlabels" => {
                            for l in &labels {
                                ensure_label(graph, nid, l)?;
                            }
                        }
                        "apoc.create.removelabels" => {
                            for l in &labels {
                                graph.remove_label(nid, l).map_err(CypherError::from)?;
                                crate::executor::inc_stat(|s| s.labels_removed += 1);
                            }
                        }
                        _ => {
                            // setLabels: replace ALL labels
                            let current: Vec<String> = graph
                                .get_node(nid)
                                .map(|n| {
                                    n.labels
                                        .iter()
                                        .map(|l| graph.strings().resolve(l.0).to_string())
                                        .collect()
                                })
                                .unwrap_or_default();
                            for l in current {
                                graph.remove_label(nid, &l).map_err(CypherError::from)?;
                            }
                            for l in &labels {
                                ensure_label(graph, nid, l)?;
                            }
                        }
                    }
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("node") {
                        nf[sl] = CypherValue::NodeRef(nid).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // apoc.create.setProperty/setProperties/removeProperties (+Rel)
        "apoc.create.setproperty"
        | "apoc.create.setproperties"
        | "apoc.create.removeproperties"
        | "apoc.create.setrelproperty"
        | "apoc.create.setrelproperties"
        | "apoc.create.removerelproperties" => {
            let is_rel = name_lower.contains("rel");
            let remove = name_lower.contains("remove");
            let plural = name_lower.ends_with("properties") && !remove;
            let mut out = Vec::new();
            for frame in frames {
                let targets = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l,
                    other => vec![other],
                };
                // (keys, values) to apply
                let pairs: Vec<(String, Option<CypherValue>)> = if remove {
                    match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                        CypherValue::List(keys) => {
                            keys.iter().map(|k| (key_string(k), None)).collect()
                        }
                        _ => Vec::new(),
                    }
                } else if plural {
                    let keys = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                        CypherValue::List(l) => l,
                        _ => Vec::new(),
                    };
                    let vals = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                        CypherValue::List(l) => l,
                        _ => Vec::new(),
                    };
                    keys.iter()
                        .enumerate()
                        .map(|(i, k)| (key_string(k), vals.get(i).cloned()))
                        .collect()
                } else {
                    let k = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                        CypherValue::String(s) => s,
                        o => o.to_string(),
                    };
                    let v = eval_expr(cx.arg(2)?, frame, &*graph, symtab)?;
                    vec![(k, Some(v))]
                };
                for t in &targets {
                    if is_rel {
                        let Some(eid) = edge_ref_of(t) else { continue };
                        graph.unindex_edge(eid);
                        for (k, v) in &pairs {
                            let pid = graph.intern_property_key(k);
                            if let Ok(e) = graph.get_edge_mut(eid) {
                                match v.as_ref().and_then(|x| x.to_property()) {
                                    Some(pv) => {
                                        e.properties.insert(pid, pv);
                                    }
                                    None => {
                                        e.properties.remove(pid);
                                    }
                                }
                            }
                            crate::executor::inc_stat(|s| s.properties_set += 1);
                        }
                        graph.index_edge(eid);
                        let e = graph.get_edge(eid).map_err(CypherError::from)?;
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
                    } else {
                        let Some(nid) = node_ref_of(t) else { continue };
                        graph.unindex_node(nid);
                        for (k, v) in &pairs {
                            let pid = graph.intern_property_key(k);
                            if let Ok(n) = graph.get_node_mut(nid) {
                                match v.as_ref().and_then(|x| x.to_property()) {
                                    Some(pv) => {
                                        n.properties.insert(pid, pv);
                                    }
                                    None => {
                                        n.properties.remove(pid);
                                    }
                                }
                            }
                            crate::executor::inc_stat(|s| s.properties_set += 1);
                        }
                        graph.index_node(nid);
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("node") {
                            nf[sl] = CypherValue::NodeRef(nid).into();
                        }
                        out.push(nf);
                    }
                }
            }
            Ok(out)
        }
        // -- virtual entities ---------------------------------
        "apoc.create.vnode" | "apoc.create.vnodes" => {
            let many = name_lower.ends_with('s');
            let mut out = Vec::new();
            for frame in frames {
                let labels: Vec<String> = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l
                        .iter()
                        .map(|v| match v {
                            CypherValue::String(s) => s.clone(),
                            o => o.to_string(),
                        })
                        .collect(),
                    CypherValue::String(s) => vec![s],
                    _ => Vec::new(),
                };
                let props_v = cx.opt_arg(1, frame, &*graph).unwrap_or(CypherValue::Null);
                let prop_maps = if many {
                    match props_v {
                        CypherValue::List(l) => l,
                        other => vec![other],
                    }
                } else {
                    vec![props_v]
                };
                for pm in prop_maps {
                    let props = match pm {
                        CypherValue::Map(m) => m
                            .into_iter()
                            .filter_map(|(k, v)| v.to_property().map(|p| (k, p)))
                            .collect(),
                        _ => Default::default(),
                    };
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("node") {
                        nf[sl] = CypherValue::Node {
                            id: next_virtual_id(),
                            labels: labels.clone(),
                            properties: props,
                        }
                        .into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.create.vrelationship" => {
            let mut out = Vec::new();
            for frame in frames {
                let from = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let rtype = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let props = cx.opt_arg(2, frame, &*graph).unwrap_or(CypherValue::Null);
                let to = eval_expr(cx.arg(3)?, frame, &*graph, symtab)?;
                let (Some(s), Some(t)) = (node_ref_of(&from), node_ref_of(&to)) else {
                    continue;
                };
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("rel") {
                    nf[sl] = CypherValue::Edge {
                        id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                        edge_type: rtype,
                        source: s,
                        target: t,
                        properties: match props {
                            CypherValue::Map(m) => m
                                .into_iter()
                                .filter_map(|(k, v)| v.to_property().map(|p| (k, p)))
                                .collect(),
                            _ => Default::default(),
                        },
                    }
                    .into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.create.virtualpath" => {
            let mut out = Vec::new();
            for frame in frames {
                let mk_node = |labels_v: CypherValue, props_v: CypherValue| {
                    let labels = match labels_v {
                        CypherValue::List(l) => l
                            .iter()
                            .map(|v| match v {
                                CypherValue::String(s) => s.clone(),
                                o => o.to_string(),
                            })
                            .collect(),
                        CypherValue::String(s) => vec![s],
                        _ => Vec::new(),
                    };
                    CypherValue::Node {
                        id: next_virtual_id(),
                        labels,
                        properties: match props_v {
                            CypherValue::Map(m) => m
                                .into_iter()
                                .filter_map(|(k, v)| v.to_property().map(|p| (k, p)))
                                .collect(),
                            _ => Default::default(),
                        },
                    }
                };
                let n = mk_node(
                    eval_expr(cx.arg(0)?, frame, &*graph, symtab)?,
                    eval_expr(cx.arg(1)?, frame, &*graph, symtab)?,
                );
                let rtype = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let rprops = eval_expr(cx.arg(3)?, frame, &*graph, symtab)?;
                let m = mk_node(
                    eval_expr(cx.arg(4)?, frame, &*graph, symtab)?,
                    eval_expr(cx.arg(5)?, frame, &*graph, symtab)?,
                );
                let (CypherValue::Node { id: nid, .. }, CypherValue::Node { id: mid, .. }) =
                    (&n, &m)
                else {
                    continue;
                };
                let rel = CypherValue::Edge {
                    id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                    edge_type: rtype,
                    source: *nid,
                    target: *mid,
                    properties: match rprops {
                        CypherValue::Map(pm) => pm
                            .into_iter()
                            .filter_map(|(k, v)| v.to_property().map(|p| (k, p)))
                            .collect(),
                        _ => Default::default(),
                    },
                };
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("from") {
                    nf[sl] = n.clone().into();
                }
                if let Some(sl) = cx.slot_for("rel") {
                    nf[sl] = rel.into();
                }
                if let Some(sl) = cx.slot_for("to") {
                    nf[sl] = m.into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.create.clonepathtovirtual" | "apoc.create.clonepathstovirtual" => {
            let mut out = Vec::new();
            for frame in frames {
                let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let paths = match v {
                    CypherValue::List(l) => l,
                    other => vec![other],
                };
                for p in paths {
                    if let CypherValue::Path { .. } = &p {
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("path") {
                            nf[sl] = p.clone().into();
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
