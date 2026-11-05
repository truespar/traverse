//! `apoc.meta.*`, `apoc.schema.*` and `apoc.stats.*`: what the graph and its schema look like.

use traverse_core::tx::view::GraphView;

use crate::apoc::key_string;
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::{self, Frame};
use crate::procedures::ProcCall;
use crate::procedures::schema::schema_type_property_rows;
use crate::types::CypherValue;

use super::parse_rel_specs;

/// `apoc.meta.*`, `apoc.schema.*` and `apoc.stats.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.meta.stats() :: (labelCount, relTypeCount, ...)
        "apoc.meta.stats" => {
            let labels = graph.label_names();
            let rel_types = graph.relationship_type_names();
            let mut label_map = std::collections::BTreeMap::new();
            for l in &labels {
                label_map.insert(
                    l.clone(),
                    CypherValue::Integer(graph.nodes_by_label(l).len() as i64),
                );
            }
            let mut rel_map = std::collections::BTreeMap::new();
            for t in &rel_types {
                rel_map.insert(
                    format!("()-[:{t}]->()"),
                    CypherValue::Integer(graph.edges_by_type(t).len() as i64),
                );
            }
            let mut out = Vec::new();
            for frame in frames {
                let mut nf = frame.clone();
                for (col, val) in [
                    ("labelCount", CypherValue::Integer(labels.len() as i64)),
                    ("relTypeCount", CypherValue::Integer(rel_types.len() as i64)),
                    (
                        "propertyKeyCount",
                        CypherValue::Integer(graph.property_key_names().len() as i64),
                    ),
                    ("nodeCount", CypherValue::Integer(graph.node_count() as i64)),
                    ("relCount", CypherValue::Integer(graph.edge_count() as i64)),
                    ("labels", CypherValue::Map(label_map.clone())),
                    ("relTypes", CypherValue::Map(rel_map.clone())),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        // apoc.schema.assert(indexes, constraints [, dropExisting=true])
        "apoc.schema.assert" => schema_assert(cx, graph),
        "apoc.stats.degrees" => {
            let spec = args
                .and_then(|a| a.first())
                .map(|e| eval_expr(e, &frames[0].clone(), &*graph, symtab))
                .transpose()?
                .and_then(|v| match v {
                    CypherValue::String(s) => Some(s),
                    _ => None,
                })
                .unwrap_or_default();
            let types: Vec<Option<String>> = if spec.trim().is_empty() {
                graph
                    .relationship_type_names()
                    .into_iter()
                    .map(Some)
                    .collect()
            } else {
                parse_rel_specs(&spec).into_iter().map(|(t, _)| t).collect()
            };
            cx.emit_rows(&|_frame| {
                use traverse_core::graph::types::Direction;
                let mut rows = Vec::new();
                for t in &types {
                    for (dname, dir) in [
                        ("OUTGOING", Direction::Outgoing),
                        ("INCOMING", Direction::Incoming),
                    ] {
                        let mut degs: Vec<i64> = Vec::new();
                        for nid in graph.nodes_vec() {
                            degs.push(
                                graph
                                    .edges_of(nid, dir, t.as_deref())
                                    .map(|v| v.len() as i64)
                                    .unwrap_or(0),
                            );
                        }
                        degs.sort_unstable();
                        let total: i64 = degs.iter().sum();
                        let pct = |p: f64| -> i64 {
                            if degs.is_empty() {
                                0
                            } else {
                                degs[(((p * degs.len() as f64).ceil() as usize).max(1) - 1)
                                    .min(degs.len() - 1)]
                            }
                        };
                        rows.push(vec![
                            ("type", CypherValue::String(t.clone().unwrap_or_default())),
                            ("direction", CypherValue::String(dname.to_string())),
                            ("total", CypherValue::Integer(total)),
                            ("p50", CypherValue::Integer(pct(0.5))),
                            ("p75", CypherValue::Integer(pct(0.75))),
                            ("p90", CypherValue::Integer(pct(0.9))),
                            ("p95", CypherValue::Integer(pct(0.95))),
                            ("p99", CypherValue::Integer(pct(0.99))),
                            ("p999", CypherValue::Integer(pct(0.999))),
                            (
                                "max",
                                CypherValue::Integer(degs.last().copied().unwrap_or(0)),
                            ),
                            (
                                "min",
                                CypherValue::Integer(degs.first().copied().unwrap_or(0)),
                            ),
                            (
                                "mean",
                                CypherValue::Float(if degs.is_empty() {
                                    0.0
                                } else {
                                    total as f64 / degs.len() as f64
                                }),
                            ),
                        ]);
                    }
                }
                Ok(rows)
            })
        }
        "apoc.schema.nodes" | "apoc.schema.relationships" => {
            let want_rel = name_lower.ends_with("relationships");
            let strings = |g: &dyn GraphView, id: u32| g.strings().resolve(id).to_string();
            let mut rows: Vec<Vec<(&'static str, CypherValue)>> = Vec::new();
            if want_rel {
                for d in graph.edge_index_catalog().descriptors() {
                    let props: Vec<CypherValue> = d
                        .property_key
                        .map(|p| {
                            strings(&*graph, p.0)
                                .split('\u{1F}')
                                .map(|s| CypherValue::String(s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    rows.push(vec![
                        ("name", CypherValue::String(d.name.clone())),
                        (
                            "label",
                            CypherValue::String(strings(&*graph, d.edge_type_id.0)),
                        ),
                        ("type", CypherValue::String("RANGE".to_string())),
                        ("properties", CypherValue::List(props)),
                        ("status", CypherValue::String("ONLINE".to_string())),
                    ]);
                }
            } else {
                for d in graph.index_catalog().descriptors() {
                    let props: Vec<CypherValue> = strings(&*graph, d.property_key.0)
                        .split('\u{1F}')
                        .map(|s| CypherValue::String(s.to_string()))
                        .collect();
                    rows.push(vec![
                        ("name", CypherValue::String(d.name.clone())),
                        ("label", CypherValue::String(strings(&*graph, d.label_id.0))),
                        ("type", CypherValue::String("RANGE".to_string())),
                        ("properties", CypherValue::List(props)),
                        ("status", CypherValue::String("ONLINE".to_string())),
                    ]);
                }
            }
            let mut out = Vec::new();
            for frame in frames {
                for row in &rows {
                    let mut nf = frame.clone();
                    for (col, val) in row {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.clone().into();
                        }
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.schema.properties.distinct" | "apoc.schema.properties.distinctcount" => {
            let counted = name_lower.ends_with("count");
            let mut out = Vec::new();
            for frame in frames {
                let label = match cx.opt_arg(0, frame, &*graph) {
                    Some(CypherValue::String(s)) => s,
                    _ => String::new(),
                };
                let key = match cx.opt_arg(1, frame, &*graph) {
                    Some(CypherValue::String(s)) => s,
                    _ => String::new(),
                };
                let labels = if label.is_empty() {
                    graph.label_names()
                } else {
                    vec![label.clone()]
                };
                for l in labels {
                    let mut counts: std::collections::BTreeMap<String, (CypherValue, i64)> =
                        Default::default();
                    for nid in graph.nodes_by_label(&l) {
                        if let Ok(Some(pv)) = graph.get_node_property(nid, &key) {
                            let cv = CypherValue::from_property(&pv);
                            let e = counts
                                .entry(key_string(&cv))
                                .or_insert_with(|| (cv.clone(), 0));
                            e.1 += 1;
                        }
                    }
                    if counted {
                        for (_, (v, c)) in counts {
                            let mut nf = frame.clone();
                            for (col, val) in [
                                ("label", CypherValue::String(l.clone())),
                                ("key", CypherValue::String(key.clone())),
                                ("value", v),
                                ("count", CypherValue::Integer(c)),
                            ] {
                                if let Some(sl) = cx.slot_for(col) {
                                    nf[sl] = val.into();
                                }
                            }
                            out.push(nf);
                        }
                    } else {
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("value") {
                            nf[sl] =
                                CypherValue::List(counts.into_values().map(|(v, _)| v).collect())
                                    .into();
                        }
                        out.push(nf);
                    }
                }
            }
            Ok(out)
        }
        "apoc.meta.data" | "apoc.meta.data.of" => {
            let rows = meta_data_rows(&*graph)?;
            let mut out = Vec::new();
            for frame in frames {
                for row in &rows {
                    let mut nf = frame.clone();
                    for (col, val) in row {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.clone().into();
                        }
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.meta.schema" => {
            let mut schema = std::collections::BTreeMap::new();
            for row in meta_data_rows(&*graph)? {
                let m: std::collections::BTreeMap<&str, &CypherValue> =
                    row.iter().map(|(k, v)| (*k, v)).collect();
                let label = key_string(m["label"]);
                let entry = schema.entry(label).or_insert_with(|| {
                    let mut e = std::collections::BTreeMap::new();
                    e.insert("type".to_string(), m["elementType"].clone());
                    e.insert(
                        "properties".to_string(),
                        CypherValue::Map(Default::default()),
                    );
                    e
                });
                if let Some(CypherValue::Map(props)) = entry.get_mut("properties") {
                    let mut pm = std::collections::BTreeMap::new();
                    pm.insert("type".to_string(), m["type"].clone());
                    pm.insert("indexed".to_string(), m["index"].clone());
                    pm.insert("unique".to_string(), m["unique"].clone());
                    pm.insert("existence".to_string(), m["existence"].clone());
                    props.insert(key_string(m["property"]), CypherValue::Map(pm));
                }
            }
            let value = CypherValue::Map(
                schema
                    .into_iter()
                    .map(|(k, v)| (k, CypherValue::Map(v)))
                    .collect(),
            );
            Ok(cx.emit_one(vec![("value", value)]))
        }
        "apoc.meta.graph"
        | "apoc.meta.graph.of"
        | "apoc.meta.graphsample"
        | "apoc.meta.subgraph" => {
            // The schema meta-graph: reuse db.schema.visualization's
            // shape (virtual label nodes + observed type triples).
            let (nodes, rels) = schema_meta_graph(&*graph);
            Ok(cx.emit_one(vec![
                ("nodes", CypherValue::List(nodes)),
                ("relationships", CypherValue::List(rels)),
            ]))
        }
        "apoc.meta.nodetypeproperties" | "apoc.meta.reltypeproperties" => {
            let rel = name_lower.contains("reltype");
            let rows = schema_type_property_rows(&*graph, rel)?;
            let mut out = Vec::new();
            for frame in frames {
                for row in &rows {
                    let mut nf = frame.clone();
                    for (col, val) in row {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.clone().into();
                        }
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}

/// apoc.schema.assert(indexes, constraints [, dropExisting = true]) -
/// declaratively set the schema: create listed b-tree indexes and
/// UNIQUE constraints (string entry = single property, list entry =
/// composite); with dropExisting, drop the b-tree indexes and
/// unique constraints NOT listed. Yields one (label, key, keys,
/// unique, action) row per touched item, action in CREATED/KEPT/DROPPED.
fn schema_assert(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    use std::collections::BTreeMap;
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let f = frame::new_frame(symtab);
    let spec_of =
        |e: Option<&crate::ast::Expr>| -> Result<BTreeMap<String, Vec<Vec<String>>>, CypherError> {
            let mut out = BTreeMap::new();
            let Some(e) = e else { return Ok(out) };
            if let CypherValue::Map(m) = eval_expr(e, &f, &*graph, symtab)? {
                for (label, entries) in m {
                    let mut specs: Vec<Vec<String>> = Vec::new();
                    if let CypherValue::List(items) = entries {
                        for item in items {
                            match item {
                                CypherValue::String(s) => specs.push(vec![s]),
                                CypherValue::List(inner) => specs.push(
                                    inner
                                        .iter()
                                        .filter_map(|v| match v {
                                            CypherValue::String(s) => Some(s.clone()),
                                            _ => None,
                                        })
                                        .collect(),
                                ),
                                _ => {}
                            }
                        }
                    }
                    out.insert(label, specs);
                }
            }
            Ok(out)
        };
    let want_indexes = spec_of(args.and_then(|a| a.first()))?;
    let want_constraints = spec_of(args.and_then(|a| a.get(1)))?;
    let drop_existing = !matches!(
        args.and_then(|a| a.get(2))
            .map(|e| eval_expr(e, &f, &*graph, symtab))
            .transpose()?,
        Some(CypherValue::Bool(false))
    );

    // rows: (label, first-key, keys, unique, action)
    let mut rows: Vec<(String, String, Vec<String>, bool, &'static str)> = Vec::new();

    // Existing b-tree node indexes: (label, props-joined) -> name.
    let strings = |g: &dyn GraphView, id: u32| g.strings().resolve(id).to_string();
    let existing_indexes: Vec<(String, Vec<String>, String)> = graph
        .index_catalog()
        .descriptors()
        .iter()
        // Constraint-backing indexes are owned by their constraint's
        // lifecycle (created/dropped with it) - schema.assert manages
        // them through the CONSTRAINTS map, never as plain indexes.
        .filter(|d| !d.name.starts_with("__constraint_"))
        .map(|d| {
            let label = strings(&*graph, d.label_id.0);
            let props: Vec<String> = strings(&*graph, d.property_key.0)
                .split('\u{1F}')
                .map(str::to_string)
                .collect();
            (label, props, d.name.clone())
        })
        .collect();
    let existing_constraints: Vec<(String, Vec<String>, String)> = graph
        .constraint_registry()
        .descriptors()
        .iter()
        .filter(|d| {
            matches!(
                d.constraint_type,
                traverse_core::graph::constraint::ConstraintType::Unique
            )
        })
        .map(|d| {
            let label = strings(&*graph, d.label_id.0);
            let mut props = vec![strings(&*graph, d.property_key.0)];
            props.extend(d.extra_property_keys.iter().map(|k| strings(&*graph, k.0)));
            (label, props, d.name.clone())
        })
        .collect();

    // Indexes.
    for (label, specs) in &want_indexes {
        for props in specs {
            if props.is_empty() {
                continue;
            }
            let exists = existing_indexes
                .iter()
                .any(|(l, p, _)| l == label && p == props);
            if exists {
                rows.push((
                    label.clone(),
                    props[0].clone(),
                    props.clone(),
                    false,
                    "KEPT",
                ));
            } else {
                let name = format!("index_{}_{}", label, props.join("_"));
                if props.len() == 1 {
                    graph
                        .create_index(&name, label, &props[0])
                        .map_err(CypherError::from)?;
                } else {
                    graph
                        .create_composite_index(&name, label, props, false)
                        .map_err(CypherError::from)?;
                }
                crate::executor::inc_stat(|s| s.indexes_added += 1);
                rows.push((
                    label.clone(),
                    props[0].clone(),
                    props.clone(),
                    false,
                    "CREATED",
                ));
            }
        }
    }
    // Constraints (UNIQUE).
    for (label, specs) in &want_constraints {
        for props in specs {
            if props.is_empty() {
                continue;
            }
            let exists = existing_constraints
                .iter()
                .any(|(l, p, _)| l == label && p == props);
            if exists {
                rows.push((label.clone(), props[0].clone(), props.clone(), true, "KEPT"));
            } else {
                let name = format!("constraint_{}_{}", label, props.join("_"));
                if props.len() == 1 {
                    graph
                        .create_constraint(
                            &name,
                            label,
                            &props[0],
                            traverse_core::graph::constraint::ConstraintType::Unique,
                        )
                        .map_err(CypherError::from)?;
                } else {
                    graph
                        .create_composite_constraint(
                            &name,
                            label,
                            props,
                            traverse_core::graph::constraint::ConstraintType::Unique,
                            false,
                        )
                        .map_err(CypherError::from)?;
                }
                rows.push((
                    label.clone(),
                    props[0].clone(),
                    props.clone(),
                    true,
                    "CREATED",
                ));
            }
        }
    }
    // Drops.
    if drop_existing {
        for (label, props, name) in &existing_indexes {
            let wanted = want_indexes
                .get(label)
                .map(|specs| specs.iter().any(|p| p == props))
                .unwrap_or(false);
            if !wanted {
                graph.drop_index(name).map_err(CypherError::from)?;
                rows.push((
                    label.clone(),
                    props[0].clone(),
                    props.clone(),
                    false,
                    "DROPPED",
                ));
            }
        }
        for (label, props, name) in &existing_constraints {
            let wanted = want_constraints
                .get(label)
                .map(|specs| specs.iter().any(|p| p == props))
                .unwrap_or(false);
            if !wanted {
                graph.drop_constraint(name).map_err(CypherError::from)?;
                rows.push((
                    label.clone(),
                    props[0].clone(),
                    props.clone(),
                    true,
                    "DROPPED",
                ));
            }
        }
    }

    let mut out = Vec::new();
    for frame in frames {
        for (label, key, keys, unique, action) in &rows {
            let mut nf = frame.clone();
            for (col, val) in [
                ("label", CypherValue::String(label.clone())),
                ("key", CypherValue::String(key.clone())),
                (
                    "keys",
                    CypherValue::List(
                        keys.iter()
                            .map(|k| CypherValue::String(k.clone()))
                            .collect(),
                    ),
                ),
                ("unique", CypherValue::Bool(*unique)),
                ("action", CypherValue::String((*action).to_string())),
            ] {
                if let Some(sl) = cx.slot_for(col) {
                    nf[sl] = val.into();
                }
            }
            out.push(nf);
        }
    }
    Ok(out)
}

/// apoc.meta.data rows: per (label|type) x property, with type/index/constraint info (sampled, 10k
/// cap per label like db.schema.*).
fn meta_data_rows(
    graph: &dyn GraphView,
) -> Result<Vec<Vec<(&'static str, CypherValue)>>, CypherError> {
    let mut rows = Vec::new();
    let strings = |id: u32| graph.strings().resolve(id).to_string();
    // index/constraint lookups
    let indexed: std::collections::HashSet<(String, String)> = graph
        .index_catalog()
        .descriptors()
        .iter()
        .map(|d| (strings(d.label_id.0), strings(d.property_key.0)))
        .collect();
    let unique: std::collections::HashSet<(String, String)> = graph
        .constraint_registry()
        .descriptors()
        .iter()
        .filter(|d| {
            matches!(
                d.constraint_type,
                traverse_core::graph::constraint::ConstraintType::Unique
                    | traverse_core::graph::constraint::ConstraintType::NodeKey
            )
        })
        .map(|d| (strings(d.label_id.0), strings(d.property_key.0)))
        .collect();
    let existence: std::collections::HashSet<(String, String)> = graph
        .constraint_registry()
        .descriptors()
        .iter()
        .filter(|d| {
            matches!(
                d.constraint_type,
                traverse_core::graph::constraint::ConstraintType::Exists
                    | traverse_core::graph::constraint::ConstraintType::NodeKey
            )
        })
        .map(|d| (strings(d.label_id.0), strings(d.property_key.0)))
        .collect();
    for source in schema_type_property_rows(graph, false)?
        .into_iter()
        .map(|r| (r, "node"))
        .chain(
            schema_type_property_rows(graph, true)?
                .into_iter()
                .map(|r| (r, "relationship")),
        )
    {
        let (row, element) = source;
        let m: std::collections::BTreeMap<&str, CypherValue> = row.into_iter().collect();
        let label = match (m.get("nodeLabels"), m.get("relType")) {
            (Some(CypherValue::List(l)), _) if !l.is_empty() => key_string(&l[0]),
            (_, Some(CypherValue::String(t))) => {
                t.trim_start_matches(":`").trim_end_matches('`').to_string()
            }
            _ => continue,
        };
        let prop = match m.get("propertyName") {
            Some(CypherValue::String(p)) => p.clone(),
            _ => continue,
        };
        let ptype = match m.get("propertyTypes") {
            Some(CypherValue::List(l)) if !l.is_empty() => key_string(&l[0]),
            _ => "ANY".to_string(),
        };
        rows.push(vec![
            ("label", CypherValue::String(label.clone())),
            ("property", CypherValue::String(prop.clone())),
            ("count", CypherValue::Integer(0)),
            (
                "unique",
                CypherValue::Bool(unique.contains(&(label.clone(), prop.clone()))),
            ),
            (
                "index",
                CypherValue::Bool(indexed.contains(&(label.clone(), prop.clone()))),
            ),
            (
                "existence",
                CypherValue::Bool(existence.contains(&(label.clone(), prop.clone()))),
            ),
            ("type", CypherValue::String(ptype)),
            (
                "array",
                CypherValue::Bool(matches!(m.get("propertyTypes"), Some(CypherValue::List(l)) if l.iter().any(|t| key_string(t).ends_with("Array")))),
            ),
            ("elementType", CypherValue::String(element.to_string())),
        ]);
    }
    Ok(rows)
}

/// Schema meta-graph (virtual label nodes + observed type triples) for apoc.meta.graph*; same
/// construction as db.schema.visualization.
fn schema_meta_graph(graph: &dyn GraphView) -> (Vec<CypherValue>, Vec<CypherValue>) {
    use traverse_core::graph::property::PropertyValue;
    use traverse_core::graph::types::{EdgeId, NodeId};
    let labels = graph.label_names();
    let mut label_id: std::collections::HashMap<String, u64> = Default::default();
    let mut nodes = Vec::with_capacity(labels.len());
    for (i, label) in labels.iter().enumerate() {
        label_id.insert(label.clone(), i as u64);
        let mut props = std::collections::BTreeMap::new();
        props.insert("name".to_string(), PropertyValue::String(label.clone()));
        nodes.push(CypherValue::Node {
            id: NodeId(i as u64),
            labels: vec![label.clone()],
            properties: props,
        });
    }
    const SAMPLE_CAP: usize = 10_000;
    let mut triples: std::collections::BTreeSet<(String, String, String)> = Default::default();
    for rel_type in graph.relationship_type_names() {
        for eid in graph.edges_by_type(&rel_type).into_iter().take(SAMPLE_CAP) {
            let Ok(edge) = graph.get_edge(eid) else {
                continue;
            };
            let (Ok(src), Ok(dst)) = (graph.get_node(edge.source), graph.get_node(edge.target))
            else {
                continue;
            };
            for sl in &src.labels {
                let sname = graph.strings().resolve(sl.0).to_string();
                for dl in &dst.labels {
                    let dname = graph.strings().resolve(dl.0).to_string();
                    triples.insert((sname.clone(), rel_type.clone(), dname));
                }
            }
        }
    }
    let mut rels = Vec::with_capacity(triples.len());
    for (j, (sl, rt, dl)) in triples.into_iter().enumerate() {
        let (Some(&s), Some(&t)) = (label_id.get(&sl), label_id.get(&dl)) else {
            continue;
        };
        rels.push(CypherValue::Edge {
            id: EdgeId(j as u64),
            edge_type: rt,
            source: NodeId(s),
            target: NodeId(t),
            properties: Default::default(),
        });
    }
    (nodes, rels)
}
