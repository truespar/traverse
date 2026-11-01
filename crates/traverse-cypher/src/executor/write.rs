//! SET, REMOVE and MERGE.

use std::collections::BTreeMap;

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{Direction, EdgeId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{
    Expr, Literal, NodePattern, Pattern, RelDirection, RelPattern, RemoveItem, SetClause, SetItem,
};
use crate::error::CypherError;
use crate::types::CypherValue;

use super::Record;
use super::constraints::{
    check_all_edge_constraints, check_all_node_constraints, check_node_property_constraint,
};
use super::context::inc_stat;
use super::eval::eval_expr_rec;
use super::values::{
    cypher_value_to_property_map, edge_to_cypher_value, eval_property_map, node_to_cypher_value,
};

// -- SET / REMOVE helpers ----------------------------------------------

pub(crate) fn exec_set_item(
    item: &SetItem,
    rec: &mut Record,
    graph: &mut dyn GraphView,
) -> Result<(), CypherError> {
    match item {
        SetItem::Property(access, expr) => {
            let val = eval_expr_rec(expr, rec, graph)?;
            if !is_storable_property(&val) {
                return Err(CypherError::type_error("InvalidPropertyType".to_string()));
            }
            let pval = val.to_property().unwrap_or(PropertyValue::Null);
            if let Some(entity) = rec.get(&access.expr_or_var) {
                match entity {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                        let nid = *id;
                        // in-place mutation: bracket with index maintenance (old entries out, new
                        // in)
                        graph.unindex_node_prop(nid, &access.key);
                        if matches!(pval, PropertyValue::Null) {
                            if let Some(pid) = graph.strings().get(&access.key).map(PropertyKeyId) {
                                let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                                node.properties.remove(pid);
                            }
                        } else {
                            let pid = graph.intern_property_key(&access.key);
                            let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                            node.properties.insert(pid, pval);
                        }
                        graph.index_node_prop(nid, &access.key);
                        inc_stat(|s| s.properties_set += 1);
                        let updated = {
                            let n = graph.get_node(nid).map_err(CypherError::from)?;
                            node_to_cypher_value(&n, graph)
                        };
                        rec.insert(access.expr_or_var.clone(), updated);
                        check_node_property_constraint(graph, nid, &access.key)?;
                    }
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                        let eid = *id;
                        graph.unindex_edge_prop(eid, &access.key);
                        if matches!(pval, PropertyValue::Null) {
                            if let Some(pid) = graph.strings().get(&access.key).map(PropertyKeyId) {
                                let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                                edge.properties.remove(pid);
                            }
                        } else {
                            let pid = graph.intern_property_key(&access.key);
                            let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                            edge.properties.insert(pid, pval);
                        }
                        graph.index_edge_prop(eid, &access.key);
                        inc_stat(|s| s.properties_set += 1);
                        let updated = {
                            let e = graph.get_edge(eid).map_err(CypherError::from)?;
                            edge_to_cypher_value(&e, graph)
                        };
                        rec.insert(access.expr_or_var.clone(), updated);
                        check_all_edge_constraints(graph, eid)?;
                    }
                    CypherValue::Null => {}
                    other => {
                        return Err(CypherError::type_error(format!(
                            "SET expected a node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        SetItem::AllProperties(var, expr) => {
            let val = eval_expr_rec(expr, rec, graph)?;
            let props = cypher_value_to_property_map(&val)?;
            let prop_count = props.len() as i64;
            if let Some(entity) = rec.get(var) {
                match entity {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                        let nid = *id;
                        graph
                            .update_node_properties(nid, props)
                            .map_err(CypherError::from)?;
                        inc_stat(|s| s.properties_set += prop_count);
                        let updated = {
                            let n = graph.get_node(nid).map_err(CypherError::from)?;
                            node_to_cypher_value(&n, graph)
                        };
                        rec.insert(var.clone(), updated);
                        check_all_node_constraints(graph, nid)?;
                    }
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                        let eid = *id;
                        graph
                            .update_edge_properties(eid, props)
                            .map_err(CypherError::from)?;
                        inc_stat(|s| s.properties_set += prop_count);
                        let updated = {
                            let e = graph.get_edge(eid).map_err(CypherError::from)?;
                            edge_to_cypher_value(&e, graph)
                        };
                        rec.insert(var.clone(), updated);
                    }
                    CypherValue::Null => {}
                    other => {
                        return Err(CypherError::type_error(format!(
                            "SET expected a node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        SetItem::MergeProperties(var, expr) => {
            let val = eval_expr_rec(expr, rec, graph)?;
            if let Some(entity) = rec.get(var) {
                let map = match &val {
                    CypherValue::Map(m) => m.clone(),
                    CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
                        properties
                            .iter()
                            .map(|(k, v)| (k.clone(), CypherValue::from_property(v)))
                            .collect()
                    }
                    _ => BTreeMap::new(),
                };
                let merge_count = map.len() as i64;
                match entity {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                        let nid = *id;
                        // intern every key, then apply
                        let interned_ops: Vec<(PropertyKeyId, Option<PropertyValue>)> = map
                            .iter()
                            .map(|(k, v)| {
                                let pid = graph.intern_property_key(k);
                                if matches!(v, CypherValue::Null) {
                                    (pid, None)
                                } else {
                                    (pid, Some(v.to_property().unwrap_or(PropertyValue::Null)))
                                }
                            })
                            .collect();
                        graph.unindex_node(nid);
                        let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                        for (pid, pv_opt) in interned_ops {
                            match pv_opt {
                                Some(pv) => {
                                    node.properties.insert(pid, pv);
                                }
                                None => {
                                    node.properties.remove(pid);
                                }
                            }
                        }
                        graph.index_node(nid);
                        inc_stat(|s| s.properties_set += merge_count);
                        let updated = {
                            let n = graph.get_node(nid).map_err(CypherError::from)?;
                            node_to_cypher_value(&n, graph)
                        };
                        rec.insert(var.clone(), updated);
                        check_all_node_constraints(graph, nid)?;
                    }
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                        let eid = *id;
                        let interned_ops: Vec<(PropertyKeyId, Option<PropertyValue>)> = map
                            .iter()
                            .map(|(k, v)| {
                                let pid = graph.intern_property_key(k);
                                if matches!(v, CypherValue::Null) {
                                    (pid, None)
                                } else {
                                    (pid, Some(v.to_property().unwrap_or(PropertyValue::Null)))
                                }
                            })
                            .collect();
                        graph.unindex_edge(eid);
                        let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                        for (pid, pv_opt) in interned_ops {
                            match pv_opt {
                                Some(pv) => {
                                    edge.properties.insert(pid, pv);
                                }
                                None => {
                                    edge.properties.remove(pid);
                                }
                            }
                        }
                        graph.index_edge(eid);
                        inc_stat(|s| s.properties_set += merge_count);
                        let updated = {
                            let e = graph.get_edge(eid).map_err(CypherError::from)?;
                            edge_to_cypher_value(&e, graph)
                        };
                        rec.insert(var.clone(), updated);
                    }
                    CypherValue::Null => {}
                    other => {
                        return Err(CypherError::type_error(format!(
                            "SET expected a node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        SetItem::Label(var, labels, dynamic_labels) => {
            let nid = match rec.get(var) {
                Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => Some(*id),
                Some(CypherValue::Null) | None => None,
                Some(other) => {
                    return Err(CypherError::type_error(format!(
                        "SET label expected a node, got {}",
                        other.type_name()
                    )));
                }
            };
            if let Some(nid) = nid {
                for label in labels {
                    let _ = graph.add_label(nid, label);
                    inc_stat(|s| s.labels_added += 1);
                }
                // dynamic labels resolved per row; NULL is a no-op (Cypher tolerates absent dynamic
                // values)
                for de in dynamic_labels {
                    let v = eval_expr_rec(de, rec, graph)?;
                    match v {
                        CypherValue::String(s) => {
                            let _ = graph.add_label(nid, &s);
                            inc_stat(|s| s.labels_added += 1);
                        }
                        CypherValue::Null => {}
                        other => {
                            return Err(CypherError::type_error(format!(
                                "SET dynamic label must evaluate to STRING, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                let updated = {
                    let n = graph.get_node(nid).map_err(CypherError::from)?;
                    node_to_cypher_value(&n, graph)
                };
                rec.insert(var.clone(), updated);
                check_all_node_constraints(graph, nid)?;
            }
        }
        SetItem::DynamicProperty {
            variable,
            key,
            value,
        } => {
            // `n[$key] = expr`: resolve the key at execute time, then the same write path as
            // `n.staticKey = expr`
            let key_val = eval_expr_rec(key, rec, graph)?;
            let key_str = match key_val {
                CypherValue::String(s) => s,
                CypherValue::Null => return Ok(()),
                other => {
                    return Err(CypherError::type_error(format!(
                        "SET n[key] = ...: key must evaluate to STRING, got {}",
                        other.type_name()
                    )));
                }
            };
            let val = eval_expr_rec(value, rec, graph)?;
            if !is_storable_property(&val) {
                return Err(CypherError::type_error("InvalidPropertyType".to_string()));
            }
            let pval = val.to_property().unwrap_or(PropertyValue::Null);
            if let Some(entity) = rec.get(variable) {
                match entity {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                        let nid = *id;
                        graph.unindex_node_prop(nid, &key_str);
                        if matches!(pval, PropertyValue::Null) {
                            if let Some(pid) = graph.strings().get(&key_str).map(PropertyKeyId) {
                                let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                                node.properties.remove(pid);
                            }
                        } else {
                            let pid = graph.intern_property_key(&key_str);
                            let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                            node.properties.insert(pid, pval);
                        }
                        graph.index_node_prop(nid, &key_str);
                        inc_stat(|s| s.properties_set += 1);
                        let updated = {
                            let n = graph.get_node(nid).map_err(CypherError::from)?;
                            node_to_cypher_value(&n, graph)
                        };
                        rec.insert(variable.clone(), updated);
                        check_node_property_constraint(graph, nid, &key_str)?;
                    }
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                        let eid = *id;
                        graph.unindex_edge_prop(eid, &key_str);
                        if matches!(pval, PropertyValue::Null) {
                            if let Some(pid) = graph.strings().get(&key_str).map(PropertyKeyId) {
                                let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                                edge.properties.remove(pid);
                            }
                        } else {
                            let pid = graph.intern_property_key(&key_str);
                            let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                            edge.properties.insert(pid, pval);
                        }
                        graph.index_edge_prop(eid, &key_str);
                        inc_stat(|s| s.properties_set += 1);
                        let updated = {
                            let e = graph.get_edge(eid).map_err(CypherError::from)?;
                            edge_to_cypher_value(&e, graph)
                        };
                        rec.insert(variable.clone(), updated);
                    }
                    CypherValue::Null => {}
                    other => {
                        return Err(CypherError::type_error(format!(
                            "SET expected a node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn exec_remove_item(
    item: &RemoveItem,
    rec: &mut Record,
    graph: &mut dyn GraphView,
) -> Result<(), CypherError> {
    match item {
        RemoveItem::Property(access) => {
            if let Some(entity) = rec.get(&access.expr_or_var) {
                match entity {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                        let nid = *id;
                        graph.unindex_node_prop(nid, &access.key);
                        if let Some(pid) = graph.strings().get(&access.key).map(PropertyKeyId) {
                            let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
                            node.properties.remove(pid);
                        }
                        graph.index_node_prop(nid, &access.key);
                        inc_stat(|s| s.properties_set += 1);
                        let updated = {
                            let n = graph.get_node(nid).map_err(CypherError::from)?;
                            node_to_cypher_value(&n, graph)
                        };
                        rec.insert(access.expr_or_var.clone(), updated);
                        check_node_property_constraint(graph, nid, &access.key)?;
                    }
                    CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                        let eid = *id;
                        graph.unindex_edge_prop(eid, &access.key);
                        if let Some(pid) = graph.strings().get(&access.key).map(PropertyKeyId) {
                            let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
                            edge.properties.remove(pid);
                        }
                        graph.index_edge_prop(eid, &access.key);
                        inc_stat(|s| s.properties_set += 1);
                        let updated = {
                            let e = graph.get_edge(eid).map_err(CypherError::from)?;
                            edge_to_cypher_value(&e, graph)
                        };
                        rec.insert(access.expr_or_var.clone(), updated);
                        check_all_edge_constraints(graph, eid)?;
                    }
                    _ => {}
                }
            }
        }
        RemoveItem::Label(var, labels, dynamic_labels) => {
            let nid = match rec.get(var) {
                Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => Some(*id),
                _ => None,
            };
            if let Some(nid) = nid {
                for label in labels {
                    let _ = graph.remove_label(nid, label);
                    inc_stat(|s| s.labels_removed += 1);
                }
                for de in dynamic_labels {
                    let v = eval_expr_rec(de, rec, graph)?;
                    match v {
                        CypherValue::String(s) => {
                            let _ = graph.remove_label(nid, &s);
                            inc_stat(|s| s.labels_removed += 1);
                        }
                        CypherValue::Null => {}
                        other => {
                            return Err(CypherError::type_error(format!(
                                "REMOVE dynamic label must evaluate to STRING, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                let updated = {
                    let n = graph.get_node(nid).map_err(CypherError::from)?;
                    node_to_cypher_value(&n, graph)
                };
                rec.insert(var.clone(), updated);
            }
        }
    }
    Ok(())
}

// -- MERGE -------------------------------------------------------------

pub(crate) fn exec_merge(
    records: Vec<Record>,
    pattern: &Pattern,
    on_create: &Option<SetClause>,
    on_match: &Option<SetClause>,
    graph: &mut dyn GraphView,
) -> Result<Vec<Record>, CypherError> {
    // null properties in a MERGE pattern (MergeReadOwnWrites)
    for part in &pattern.parts {
        if let Some(Expr::MapLiteral(pairs)) = &part.element.node.properties {
            for (_, v) in pairs {
                if matches!(v, Expr::Literal(Literal::Null)) {
                    return Err(CypherError::semantic(
                        "MERGE does not support null property values".to_string(),
                    ));
                }
            }
        }
        for chain in &part.element.chain {
            if let Some(Expr::MapLiteral(pairs)) = &chain.rel.properties {
                for (_, v) in pairs {
                    if matches!(v, Expr::Literal(Literal::Null)) {
                        return Err(CypherError::semantic(
                            "MERGE does not support null property values".to_string(),
                        ));
                    }
                }
            }
        }
    }

    let mut result = Vec::new();

    for rec in records {
        let part = &pattern.parts[0];
        let node_pat = &part.element.node;
        let has_rel = !part.element.chain.is_empty();

        if has_rel {
            // MERGE with a relationship: (a)-[r:TYPE]->(b)
            let chain = &part.element.chain[0];
            let rel_pat = &chain.rel;
            let target_pat = &chain.node;
            let edge_type = rel_pat.types.first().cloned().unwrap_or_default();

            // source node: bound variable or match
            let from_id = merge_resolve_node(node_pat, &rec, graph)?;
            let to_id = merge_resolve_node(target_pat, &rec, graph)?;

            match (from_id, to_id) {
                (Some(fid), Some(tid)) => {
                    // both nodes exist: look for matching relationships
                    let undirected = matches!(rel_pat.direction, RelDirection::Both);
                    let (actual_from, actual_to) = match rel_pat.direction {
                        RelDirection::Incoming => (tid, fid),
                        _ => (fid, tid),
                    };
                    let matched_edges = merge_find_edges(
                        graph,
                        actual_from,
                        actual_to,
                        &edge_type,
                        rel_pat,
                        &rec,
                        undirected,
                    );

                    if matched_edges.is_empty() {
                        // no match: CREATE the relationship
                        let props = if let Some(props_expr) = &rel_pat.properties {
                            eval_property_map(props_expr, &rec, graph)?
                        } else {
                            BTreeMap::new()
                        };
                        let prop_count = props.len() as i64;
                        let eid = graph
                            .create_edge(actual_from, actual_to, &edge_type, props)
                            .map_err(CypherError::from)?;
                        check_all_edge_constraints(&*graph, eid)?;
                        inc_stat(|s| s.relationships_created += 1);
                        inc_stat(|s| s.properties_set += prop_count);
                        let mut new_rec = rec.clone();
                        merge_bind_node(node_pat, fid, &mut new_rec, graph)?;
                        merge_bind_node(target_pat, tid, &mut new_rec, graph)?;
                        if let Some(var) = &rel_pat.variable {
                            let cv = {
                                let e = graph.get_edge(eid).map_err(CypherError::from)?;
                                edge_to_cypher_value(&e, graph)
                            };
                            new_rec.insert(var.clone(), cv);
                        }
                        if let Some(set_clause) = on_create {
                            for item in &set_clause.items {
                                exec_set_item(item, &mut new_rec, graph)?;
                            }
                        }
                        result.push(new_rec);
                    } else {
                        // matches: ON MATCH for each
                        for eid in matched_edges {
                            let mut new_rec = rec.clone();
                            merge_bind_node(node_pat, fid, &mut new_rec, graph)?;
                            merge_bind_node(target_pat, tid, &mut new_rec, graph)?;
                            if let Some(var) = &rel_pat.variable {
                                let cv = {
                                    let e = graph.get_edge(eid).map_err(CypherError::from)?;
                                    edge_to_cypher_value(&e, graph)
                                };
                                new_rec.insert(var.clone(), cv);
                            }
                            if let Some(set_clause) = on_match {
                                for item in &set_clause.items {
                                    exec_set_item(item, &mut new_rec, graph)?;
                                }
                            }
                            result.push(new_rec);
                        }
                    }
                }
                _ => {
                    // one or both nodes need creating
                    let fid = if let Some(id) = from_id {
                        id
                    } else {
                        merge_create_node(node_pat, &rec, graph)?
                    };
                    let tid = if let Some(id) = to_id {
                        id
                    } else {
                        merge_create_node(target_pat, &rec, graph)?
                    };
                    let (actual_from, actual_to) = match rel_pat.direction {
                        RelDirection::Incoming => (tid, fid),
                        _ => (fid, tid),
                    };
                    let props = if let Some(props_expr) = &rel_pat.properties {
                        eval_property_map(props_expr, &rec, graph)?
                    } else {
                        BTreeMap::new()
                    };
                    let prop_count = props.len() as i64;
                    let eid = graph
                        .create_edge(actual_from, actual_to, &edge_type, props)
                        .map_err(CypherError::from)?;
                    check_all_edge_constraints(&*graph, eid)?;
                    inc_stat(|s| s.relationships_created += 1);
                    inc_stat(|s| s.properties_set += prop_count);
                    let mut new_rec = rec.clone();
                    merge_bind_node(node_pat, fid, &mut new_rec, graph)?;
                    merge_bind_node(target_pat, tid, &mut new_rec, graph)?;
                    if let Some(var) = &rel_pat.variable {
                        let cv = {
                            let e = graph.get_edge(eid).map_err(CypherError::from)?;
                            edge_to_cypher_value(&e, graph)
                        };
                        new_rec.insert(var.clone(), cv);
                    }
                    if let Some(set_clause) = on_create {
                        for item in &set_clause.items {
                            exec_set_item(item, &mut new_rec, graph)?;
                        }
                    }
                    result.push(new_rec);
                }
            }
        } else {
            // node-only MERGE; already-bound variable -> use the binding
            let already_bound = node_pat.variable.as_ref().and_then(|v| match rec.get(v) {
                Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => Some(*id),
                _ => None,
            });
            if let Some(nid) = already_bound {
                let mut new_rec = rec;
                merge_bind_node(node_pat, nid, &mut new_rec, graph)?;
                if let Some(set_clause) = on_match {
                    for item in &set_clause.items {
                        exec_set_item(item, &mut new_rec, graph)?;
                    }
                }
                result.push(new_rec);
            } else {
                let matched = merge_match_all_nodes(node_pat, &rec, graph)?;
                if matched.is_empty() {
                    let nid = merge_create_node(node_pat, &rec, graph)?;
                    let mut new_rec = rec;
                    merge_bind_node(node_pat, nid, &mut new_rec, graph)?;
                    if let Some(set_clause) = on_create {
                        for item in &set_clause.items {
                            exec_set_item(item, &mut new_rec, graph)?;
                        }
                    }
                    result.push(new_rec);
                } else {
                    for nid in matched {
                        let mut new_rec = rec.clone();
                        merge_bind_node(node_pat, nid, &mut new_rec, graph)?;
                        if let Some(set_clause) = on_match {
                            for item in &set_clause.items {
                                exec_set_item(item, &mut new_rec, graph)?;
                            }
                        }
                        result.push(new_rec);
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Node for a pattern: the bound variable's node if bound, else a matching graph node.
fn merge_resolve_node(
    pat: &NodePattern,
    rec: &Record,
    graph: &dyn GraphView,
) -> Result<Option<NodeId>, CypherError> {
    if let Some(var) = &pat.variable {
        match rec.get(var) {
            Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => {
                return Ok(Some(*id));
            }
            _ => {}
        }
    }
    merge_match_node(pat, rec, graph)
}

/// First graph node matching the pattern.
fn merge_match_node(
    pat: &NodePattern,
    rec: &Record,
    graph: &dyn GraphView,
) -> Result<Option<NodeId>, CypherError> {
    let matches = merge_match_all_nodes(pat, rec, graph)?;
    Ok(matches.into_iter().next())
}

/// All graph nodes matching the pattern.
fn merge_match_all_nodes(
    pat: &NodePattern,
    rec: &Record,
    graph: &dyn GraphView,
) -> Result<Vec<NodeId>, CypherError> {
    let label = pat.labels.first().map(|s| s.as_str());
    let candidates: Vec<NodeId> = match label {
        Some(l) => graph.nodes_by_label(l),
        None => graph.nodes_vec(),
    };

    let mut result = Vec::new();
    for nid in candidates {
        if let Ok(node) = graph.get_node(nid) {
            if let Some(Expr::MapLiteral(entries)) = &pat.properties {
                let all_match = entries.iter().all(|(k, v)| {
                    if let Ok(expected) = eval_expr_rec(v, rec, graph) {
                        let actual = graph
                            .strings()
                            .get(k)
                            .map(PropertyKeyId)
                            .and_then(|pid| node.properties.get(pid))
                            .map(|pv| CypherValue::from_property(&pv))
                            .unwrap_or(CypherValue::Null);
                        actual == expected
                    } else {
                        false
                    }
                });
                if !all_match {
                    continue;
                }
            }
            let all_labels = pat.labels.iter().all(|l| {
                if let Some(lid_raw) = graph.strings().get(l) {
                    let lid = traverse_core::graph::types::LabelId(lid_raw);
                    node.has_label(lid)
                } else {
                    false
                }
            });
            if all_labels {
                result.push(nid);
            }
        }
    }
    Ok(result)
}

/// Creates a node from a pattern.
fn merge_create_node(
    pat: &NodePattern,
    rec: &Record,
    graph: &mut dyn GraphView,
) -> Result<NodeId, CypherError> {
    let props = if let Some(props_expr) = &pat.properties {
        eval_property_map(props_expr, rec, graph)?
    } else {
        BTreeMap::new()
    };
    let label_strs: Vec<&str> = pat.labels.iter().map(|s| s.as_str()).collect();
    let prop_count = props.len() as i64;
    let nid = graph
        .create_node(&label_strs, props)
        .map_err(CypherError::from)?;
    inc_stat(|s| s.nodes_created += 1);
    inc_stat(|s| s.labels_added += label_strs.len() as i64);
    inc_stat(|s| s.properties_set += prop_count);
    check_all_node_constraints(graph, nid)?;
    Ok(nid)
}

/// Binds a node variable in the record.
fn merge_bind_node(
    pat: &NodePattern,
    nid: NodeId,
    rec: &mut Record,
    graph: &dyn GraphView,
) -> Result<(), CypherError> {
    if let Some(var) = &pat.variable {
        let n = graph.get_node(nid).map_err(CypherError::from)?;
        let cv = node_to_cypher_value(&n, graph);
        rec.insert(var.clone(), cv);
    }
    Ok(())
}

/// Matching edges between two nodes.
fn merge_find_edges(
    graph: &dyn GraphView,
    from: NodeId,
    to: NodeId,
    edge_type: &str,
    rel_pat: &RelPattern,
    rec: &Record,
    undirected: bool,
) -> Vec<EdgeId> {
    let type_filter = if edge_type.is_empty() {
        None
    } else {
        Some(edge_type)
    };
    let direction = if undirected {
        Direction::Both
    } else {
        Direction::Outgoing
    };
    let edge_ids = match graph.edges_of(from, direction, type_filter) {
        Ok(ids) => ids,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for eid in edge_ids {
        let e = match graph.get_edge(eid) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // undirected: either endpoint may be the target
        let matches_target = if undirected {
            (e.source == from && e.target == to) || (e.source == to && e.target == from)
        } else {
            e.target == to
        };
        if !matches_target {
            continue;
        }
        // filter by relationship properties, if given
        if let Some(ref props_expr) = rel_pat.properties
            && let Ok(expected) = eval_property_map(props_expr, rec, graph)
        {
            let mut matches = true;
            for (k, v) in &expected {
                let actual = graph
                    .strings()
                    .get(k)
                    .map(PropertyKeyId)
                    .and_then(|pid| e.properties.get(pid));
                match actual {
                    Some(pv) if pv == *v => {}
                    _ => {
                        matches = false;
                        break;
                    }
                }
            }
            if !matches {
                continue;
            }
        }
        result.push(eid);
    }
    result
}

/// Storable as a property (no maps, nodes, edges or paths)?
fn is_storable_property(val: &CypherValue) -> bool {
    match val {
        CypherValue::Null
        | CypherValue::Bytes(_)
        | CypherValue::Bool(_)
        | CypherValue::Integer(_)
        | CypherValue::Float(_)
        | CypherValue::String(_)
        | CypherValue::Date(_)
        | CypherValue::LocalTime(_)
        | CypherValue::Time(_)
        | CypherValue::LocalDateTime(_)
        | CypherValue::DateTime(_)
        | CypherValue::Duration(_) => true,
        CypherValue::List(items) => items.iter().all(is_storable_property),
        // points arrive as `{x, y, z?, srid, crs}` maps and are storable; other maps aren't (Cypher
        // has no map property type)
        CypherValue::Map(m) => {
            m.contains_key("x")
                && m.contains_key("y")
                && m.contains_key("srid")
                && m.contains_key("crs")
        }
        CypherValue::Node { .. }
        | CypherValue::Edge { .. }
        | CypherValue::NodeRef(_)
        | CypherValue::EdgeRef { .. }
        | CypherValue::Path { .. } => false,
    }
}
