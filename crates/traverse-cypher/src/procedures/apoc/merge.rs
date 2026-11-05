//! `apoc.merge.*`: MERGE with dynamic labels, types and keys.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::ProcCall;
use crate::types::CypherValue;

use super::{labels_of, node_ref_of};

/// `apoc.merge.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.merge.node(labels, identProps [, onCreateProps [, onMatchProps]]) :: (node)
        "apoc.merge.node" => {
            let mut out = Vec::new();
            for frame in frames {
                let labels_v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let labels = labels_of(&labels_v);
                let ident_v = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                let CypherValue::Map(ident) = ident_v.clone() else {
                    return Err(CypherError::type_error(
                        "identProps must be a MAP".to_string(),
                    ));
                };
                // existing node with ALL labels + ident props
                let candidates = match labels.first() {
                    Some(l) => graph.nodes_by_label(l),
                    None => Vec::new(),
                };
                let strings = |g: &dyn GraphView, id: u32| g.strings().resolve(id).to_string();
                let mut found = None;
                'cand: for nid in candidates {
                    let Ok(node) = graph.get_node(nid) else {
                        continue;
                    };
                    for l in &labels {
                        if !node.labels.iter().any(|lid| strings(&*graph, lid.0) == *l) {
                            continue 'cand;
                        }
                    }
                    for (k, v) in &ident {
                        let Ok(Some(pv)) = graph.get_node_property(nid, k) else {
                            continue 'cand;
                        };
                        if CypherValue::from_property(&pv) != *v {
                            continue 'cand;
                        }
                    }
                    found = Some(nid);
                    break;
                }
                let nid = match found {
                    Some(nid) => {
                        // ON MATCH props (arg 3)
                        if let Some(e) = args.and_then(|a| a.get(3)) {
                            let extra = eval_expr(e, frame, &*graph, symtab)?;
                            apply_props(graph, nid, &extra)?;
                        }
                        nid
                    }
                    None => {
                        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                        let mut all = ident.clone();
                        if let Some(e) = args.and_then(|a| a.get(2))
                            && let CypherValue::Map(oc) = eval_expr(e, frame, &*graph, symtab)?
                        {
                            for (k, v) in oc {
                                all.insert(k, v);
                            }
                        }
                        let props =
                            crate::executor::cypher_value_to_property_map(&CypherValue::Map(all))?;
                        let nid = graph
                            .create_node(&label_refs, props)
                            .map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.nodes_created += 1);
                        nid
                    }
                };
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("node") {
                    nf[sl] = CypherValue::NodeRef(nid).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        // apoc.merge.relationship(start, type, identProps, props, end [, onMatchProps]) ::
        // (rel)
        "apoc.merge.relationship" => {
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
                let ident_v = eval_expr(cx.arg(2)?, frame, &*graph, symtab)?;
                let props_v = eval_expr(cx.arg(3)?, frame, &*graph, symtab)?;
                let to = eval_expr(cx.arg(4)?, frame, &*graph, symtab)?;
                let (Some(src_id), Some(dst_id)) = (node_ref_of(&from), node_ref_of(&to)) else {
                    return Err(CypherError::type_error(
                        "apoc.merge.relationship needs NODE start and end".to_string(),
                    ));
                };
                let ident = match &ident_v {
                    CypherValue::Map(m) => m.clone(),
                    _ => Default::default(),
                };
                // existing edge with the same endpoints + type + ident props?
                let mut found = None;
                for eid in graph
                    .edges_of(
                        src_id,
                        traverse_core::graph::types::Direction::Outgoing,
                        Some(&rtype),
                    )
                    .unwrap_or_default()
                {
                    let Ok(edge) = graph.get_edge(eid) else {
                        continue;
                    };
                    if edge.target != dst_id {
                        continue;
                    }
                    let mut all_match = true;
                    for (k, v) in &ident {
                        let Ok(Some(pv)) = graph.get_edge_property(eid, k) else {
                            all_match = false;
                            break;
                        };
                        if CypherValue::from_property(&pv) != *v {
                            all_match = false;
                            break;
                        }
                    }
                    if all_match {
                        found = Some(eid);
                        break;
                    }
                }
                let eid = match found {
                    Some(eid) => eid,
                    None => {
                        let mut all = ident.clone();
                        if let CypherValue::Map(p) = &props_v {
                            for (k, v) in p {
                                all.insert(k.clone(), v.clone());
                            }
                        }
                        let props =
                            crate::executor::cypher_value_to_property_map(&CypherValue::Map(all))?;
                        let eid = graph
                            .create_edge(src_id, dst_id, &rtype, props)
                            .map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.relationships_created += 1);
                        eid
                    }
                };
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
        // -- merge with stats / eager aliases -----------------
        "apoc.merge.node.eager" | "apoc.merge.nodewithstats" | "apoc.merge.nodewithstats.eager" => {
            let with_stats = name_lower.contains("withstats");
            let mut out = Vec::new();
            for frame in frames {
                let labels: Vec<String> = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
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
                let before = crate::executor::get_query_stats().nodes_created;
                let ident = cx.opt_arg(1, frame, &*graph).unwrap_or(CypherValue::Null);
                let on_create = cx.opt_arg(2, frame, &*graph);
                let on_match = cx.opt_arg(3, frame, &*graph);
                let node = apoc_merge_node(graph, &labels, &ident, on_create, on_match)?;
                let created = crate::executor::get_query_stats().nodes_created - before;
                let mut nf = frame.clone();
                if with_stats {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("onCreate".to_string(), CypherValue::Integer(created));
                    m.insert("onMatch".to_string(), CypherValue::Integer(1 - created));
                    if let Some(sl) = cx.slot_for("stats") {
                        nf[sl] = CypherValue::Map(m).into();
                    }
                }
                if let Some(sl) = cx.slot_for("node") {
                    nf[sl] = CypherValue::NodeRef(node).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.merge.relationship.eager"
        | "apoc.merge.relationshipwithstats"
        | "apoc.merge.relationshipwithstats.eager" => {
            let with_stats = name_lower.contains("withstats");
            let mut out = Vec::new();
            for frame in frames {
                let from = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let rtype = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let ident = cx.opt_arg(2, frame, &*graph).unwrap_or(CypherValue::Null);
                let props = cx.opt_arg(3, frame, &*graph).unwrap_or(CypherValue::Null);
                let to = eval_expr(cx.arg(4)?, frame, &*graph, symtab)?;
                let before = crate::executor::get_query_stats().relationships_created;
                let (eid, src_id, dst_id, tid) =
                    apoc_merge_relationship(graph, &from, &rtype, &ident, &props, &to)?;
                let created = crate::executor::get_query_stats().relationships_created - before;
                let mut nf = frame.clone();
                if with_stats {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("onCreate".to_string(), CypherValue::Integer(created));
                    m.insert("onMatch".to_string(), CypherValue::Integer(1 - created));
                    if let Some(sl) = cx.slot_for("stats") {
                        nf[sl] = CypherValue::Map(m).into();
                    }
                }
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
        other => Err(super::unknown(other)),
    }
}

/// Applies a property MAP onto an existing node (APOC merge ON MATCH).
fn apply_props(
    graph: &mut dyn GraphView,
    nid: traverse_core::graph::types::NodeId,
    props: &CypherValue,
) -> Result<(), CypherError> {
    let CypherValue::Map(m) = props else {
        return Ok(());
    };
    graph.unindex_node(nid);
    for (k, v) in m {
        let pid = graph.intern_property_key(k);
        let Some(pv) = v.to_property() else { continue };
        if let Ok(node) = graph.get_node_mut(nid) {
            node.properties.insert(pid, pv);
        }
        crate::executor::inc_stat(|s| s.properties_set += 1);
    }
    graph.index_node(nid);
    Ok(())
}

/// Shared merge-node core (apoc.merge.node + WithStats/eager variants).
fn apoc_merge_node(
    graph: &mut dyn GraphView,
    labels: &[String],
    ident_v: &CypherValue,
    on_create: Option<CypherValue>,
    on_match: Option<CypherValue>,
) -> Result<traverse_core::graph::types::NodeId, CypherError> {
    let CypherValue::Map(ident) = ident_v.clone() else {
        return Err(CypherError::type_error(
            "identProps must be a MAP".to_string(),
        ));
    };
    let candidates = match labels.first() {
        Some(l) => graph.nodes_by_label(l),
        None => Vec::new(),
    };
    let mut found = None;
    'cand: for nid in candidates {
        let Ok(node) = graph.get_node(nid) else {
            continue;
        };
        for l in labels {
            if !node
                .labels
                .iter()
                .any(|lid| graph.strings().resolve(lid.0) == *l)
            {
                continue 'cand;
            }
        }
        for (k, v) in &ident {
            let Ok(Some(pv)) = graph.get_node_property(nid, k) else {
                continue 'cand;
            };
            if CypherValue::from_property(&pv) != *v {
                continue 'cand;
            }
        }
        found = Some(nid);
        break;
    }
    match found {
        Some(nid) => {
            if let Some(om) = on_match {
                apply_props(graph, nid, &om)?;
            }
            Ok(nid)
        }
        None => {
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let mut all = ident.clone();
            if let Some(CypherValue::Map(oc)) = on_create {
                for (k, v) in oc {
                    all.insert(k, v);
                }
            }
            let props = crate::executor::cypher_value_to_property_map(&CypherValue::Map(all))?;
            let nid = graph
                .create_node(&label_refs, props)
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.nodes_created += 1);
            Ok(nid)
        }
    }
}

/// Shared merge-relationship core: (edge, src, dst, type id).
fn apoc_merge_relationship(
    graph: &mut dyn GraphView,
    from: &CypherValue,
    rtype: &str,
    ident_v: &CypherValue,
    props_v: &CypherValue,
    to: &CypherValue,
) -> Result<
    (
        traverse_core::graph::types::EdgeId,
        traverse_core::graph::types::NodeId,
        traverse_core::graph::types::NodeId,
        traverse_core::graph::types::EdgeTypeId,
    ),
    CypherError,
> {
    let (Some(src_id), Some(dst_id)) = (node_ref_of(from), node_ref_of(to)) else {
        return Err(CypherError::type_error(
            "merge.relationship needs NODE start and end".to_string(),
        ));
    };
    let ident = match ident_v {
        CypherValue::Map(m) => m.clone(),
        _ => Default::default(),
    };
    let mut found = None;
    for eid in graph
        .edges_of(
            src_id,
            traverse_core::graph::types::Direction::Outgoing,
            Some(rtype),
        )
        .unwrap_or_default()
    {
        let Ok(edge) = graph.get_edge(eid) else {
            continue;
        };
        if edge.target != dst_id {
            continue;
        }
        let mut all_match = true;
        for (k, v) in &ident {
            let Ok(Some(pv)) = graph.get_edge_property(eid, k) else {
                all_match = false;
                break;
            };
            if CypherValue::from_property(&pv) != *v {
                all_match = false;
                break;
            }
        }
        if all_match {
            found = Some(eid);
            break;
        }
    }
    let eid = match found {
        Some(eid) => eid,
        None => {
            let mut all = ident.clone();
            if let CypherValue::Map(p) = props_v {
                for (k, v) in p {
                    all.insert(k.clone(), v.clone());
                }
            }
            let props = crate::executor::cypher_value_to_property_map(&CypherValue::Map(all))?;
            let eid = graph
                .create_edge(src_id, dst_id, rtype, props)
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.relationships_created += 1);
            eid
        }
    };
    let tid =
        traverse_core::graph::types::EdgeTypeId(graph.strings().get(rtype).unwrap_or_default());
    Ok((eid, src_id, dst_id, tid))
}
