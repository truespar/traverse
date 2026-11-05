//! Schema introspection: `db.labels()`, `db.propertyKeys()`, `db.relationshipTypes()` and
//! the node / relationship type property listings.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::types::CypherValue;

use super::ProcCall;

/// Built-in schema-introspection procedures (`db.labels` /
/// `db.propertyKeys` / `db.relationshipTypes`). Emits one output row per
/// token, cross-joined onto each input frame, binding either the explicit
/// `YIELD` target or the default output-column slot (for a standalone
/// `CALL db.labels()`).
pub(super) fn call(
    cx: &ProcCall,
    cols: &'static [&'static str],
    graph: &dyn GraphView,
) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        name: procedure_name,
        args,
        yield_columns,
        frames,
        symtab,
        ..
    } = *cx;

    // Void procedures (e.g. db.awaitIndexes) - no output; pass input rows
    // through unchanged (Traverse indexes populate synchronously).
    if cols.is_empty() {
        return Ok(frames.to_vec());
    }

    // dbms.components() - one (name, versions, edition) row per input frame.
    if procedure_name == "dbms.components" {
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            let mut nf = frame.clone();
            if let Some(s) = cx.slot_for("name") {
                nf[s] = CypherValue::String("Traverse".to_string()).into();
            }
            if let Some(s) = cx.slot_for("versions") {
                nf[s] = CypherValue::List(vec![CypherValue::String(
                    env!("CARGO_PKG_VERSION").to_string(),
                )])
                .into();
            }
            if let Some(s) = cx.slot_for("edition") {
                nf[s] = CypherValue::String("enterprise".to_string()).into();
            }
            out.push(nf);
        }
        return Ok(out);
    }

    // db.schema.visualization() - the schema meta-graph as one row of
    // (nodes, relationships): one virtual node per label, one virtual
    // relationship per observed (srcLabel, type, dstLabel) triple.
    if procedure_name == "db.schema.visualization" {
        use traverse_core::graph::property::PropertyValue;
        use traverse_core::graph::types::{EdgeId, NodeId};

        let labels = graph.label_names();
        let mut label_id: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let mut nodes: Vec<CypherValue> = Vec::with_capacity(labels.len());
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
        // Discover connectivity by sampling edges per type. Sampling bounds
        // the cost on large graphs (rare label pairings may be missed).
        const SAMPLE_CAP: usize = 10_000;
        let mut triples: std::collections::BTreeSet<(String, String, String)> = Default::default();
        for rel_type in graph.relationship_type_names() {
            crate::query::check_interrupt()?;
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
        let mut rels: Vec<CypherValue> = Vec::with_capacity(triples.len());
        for (j, (sl, rt, dl)) in triples.into_iter().enumerate() {
            let (Some(&s), Some(&t)) = (label_id.get(&sl), label_id.get(&dl)) else {
                continue;
            };
            rels.push(CypherValue::Edge {
                id: EdgeId(j as u64),
                edge_type: rt,
                source: NodeId(s),
                target: NodeId(t),
                properties: std::collections::BTreeMap::new(),
            });
        }
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            let mut nf = frame.clone();
            if let Some(s) = cx.slot_for("nodes") {
                nf[s] = CypherValue::List(nodes.clone()).into();
            }
            if let Some(s) = cx.slot_for("relationships") {
                nf[s] = CypherValue::List(rels.clone()).into();
            }
            out.push(nf);
        }
        return Ok(out);
    }

    // -- multi-column emitters for the operational/introspection procedures; rows are
    // cross-joined onto each input frame like the token lists below
    let eval_str_arg = |idx: usize, frame: &Frame| -> Result<String, CypherError> {
        match args.and_then(|a| a.get(idx)) {
            Some(e) => match eval_expr(e, frame, graph, symtab)? {
                CypherValue::String(s) => Ok(s),
                CypherValue::Null => Ok(String::new()),
                other => Ok(other.to_string()),
            },
            None => Ok(String::new()),
        }
    };
    // Emit `rows` (col -> value maps) cross-joined per input frame.
    let emit = |frames: &[Frame], rows: Vec<Vec<(&str, CypherValue)>>| -> Vec<Frame> {
        let mut out = Vec::with_capacity(frames.len() * rows.len().max(1));
        for frame in frames {
            for row in &rows {
                let mut nf = frame.clone();
                for (col, val) in row {
                    if let Some(s) = cx.slot_for(col) {
                        nf[s] = val.clone().into();
                    }
                }
                out.push(nf);
            }
        }
        out
    };
    match procedure_name {
        "db.clearQueryCaches" => {
            // Plan caches are per-session and invalidated by DDL
            // generation counters; nothing to clear process-wide.
            return Ok(emit(
                frames,
                vec![vec![(
                    "value",
                    CypherValue::String("Query cache already cleared.".into()),
                )]],
            ));
        }
        "db.info" | "dbms.info" => {
            return Ok(emit(
                frames,
                vec![vec![
                    ("id", CypherValue::String("traverse".into())),
                    ("name", CypherValue::String("traverse".into())),
                    ("creationDate", CypherValue::String(String::new())),
                ]],
            ));
        }
        "dbms.upgrade" => {
            return Ok(emit(
                frames,
                vec![vec![
                    ("status", CypherValue::String("CURRENT".into())),
                    (
                        "upgradeResult",
                        CypherValue::String("Upgrade not required".into()),
                    ),
                ]],
            ));
        }
        "dbms.upgradeStatus" => {
            return Ok(emit(
                frames,
                vec![vec![
                    ("status", CypherValue::String("CURRENT".into())),
                    (
                        "description",
                        CypherValue::String("The database is at the current version.".into()),
                    ),
                    ("resolution", CypherValue::String(String::new())),
                ]],
            ));
        }
        "dbms.listCapabilities"
        | "db.stats.status"
        | "db.stats.retrieveAllAnonymized"
        | "db.index.fulltext.queryRelationships"
        | "db.index.vector.queryRelationships" => {
            // Empty result sets: no capability registry / stats sections /
            // relationship-scoped fulltext-vector indexes (node-only).
            return Ok(Vec::new());
        }
        "dbms.listConfig" => {
            let mut out = Vec::new();
            for frame in frames {
                let needle = eval_str_arg(0, frame)?;
                for s in crate::settings::all_settings() {
                    if !needle.is_empty() && !s.name.contains(&needle) {
                        continue;
                    }
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("name", CypherValue::String(s.name.clone())),
                        ("description", CypherValue::String(s.description.clone())),
                        ("value", CypherValue::String(s.value.clone())),
                        ("dynamic", CypherValue::Bool(s.is_dynamic)),
                        ("defaultValue", CypherValue::String(s.default_value.clone())),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                    out.push(nf);
                }
            }
            return Ok(out);
        }
        "db.index.fulltext.listAvailableAnalyzers" => {
            let stop: Vec<CypherValue> = traverse_core::graph::fulltext::english_stopwords()
                .iter()
                .map(|s| CypherValue::String((*s).to_string()))
                .collect();
            let rows = vec![
                vec![
                    ("analyzer", CypherValue::String("standard".into())),
                    (
                        "description",
                        CypherValue::String(
                            "Unicode word tokeniser + lowercase; no stop-words, no stemming."
                                .into(),
                        ),
                    ),
                    ("stopwords", CypherValue::List(Vec::new())),
                ],
                vec![
                    ("analyzer", CypherValue::String("english".into())),
                    (
                        "description",
                        CypherValue::String(
                            "Standard + English stop-word removal + Porter stemming.".into(),
                        ),
                    ),
                    ("stopwords", CypherValue::List(stop)),
                ],
                vec![
                    ("analyzer", CypherValue::String("keyword".into())),
                    (
                        "description",
                        CypherValue::String(
                            "Whole field value as a single, un-lowercased token.".into(),
                        ),
                    ),
                    ("stopwords", CypherValue::List(Vec::new())),
                ],
                vec![
                    ("analyzer", CypherValue::String("simple".into())),
                    (
                        "description",
                        CypherValue::String(
                            "Unicode word tokeniser + lowercase (alias of standard).".into(),
                        ),
                    ),
                    ("stopwords", CypherValue::List(Vec::new())),
                ],
                vec![
                    ("analyzer", CypherValue::String("whitespace".into())),
                    (
                        "description",
                        CypherValue::String(
                            "Split on whitespace only; no lowercasing or filtering.".into(),
                        ),
                    ),
                    ("stopwords", CypherValue::List(Vec::new())),
                ],
            ];
            return Ok(emit(frames, rows));
        }
        "db.stats.collect" | "db.stats.stop" | "db.stats.clear" => {
            let mut out = Vec::new();
            for frame in frames {
                let section = eval_str_arg(0, frame)?;
                let mut nf = frame.clone();
                for (col, val) in [
                    ("section", CypherValue::String(section)),
                    ("success", CypherValue::Bool(true)),
                    ("message", CypherValue::String(String::new())),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            return Ok(out);
        }
        "db.stats.retrieve" => {
            let mut out = Vec::new();
            for frame in frames {
                let section = eval_str_arg(0, frame)?;
                let mut nf = frame.clone();
                for (col, val) in [
                    ("section", CypherValue::String(section)),
                    ("data", CypherValue::Map(std::collections::BTreeMap::new())),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            return Ok(out);
        }
        "dbms.listConnections" => {
            // Single embedded connection - report the caller, matching
            // the SHOW TRANSACTIONS single-node semantics.
            return Ok(emit(
                frames,
                vec![vec![
                    ("connectionId", CypherValue::String("bolt-1".into())),
                    ("connectTime", CypherValue::String(String::new())),
                    ("connector", CypherValue::String("bolt".into())),
                    ("username", CypherValue::String(String::new())),
                    ("userAgent", CypherValue::String(String::new())),
                    ("serverAddress", CypherValue::String(String::new())),
                    ("clientAddress", CypherValue::String(String::new())),
                ]],
            ));
        }
        "dbms.killConnection" | "dbms.killConnections" => {
            // Cross-connection kill isn't possible in the embedded
            // engine; report not-found, like Neo4j does for unknown ids.
            let mut out = Vec::new();
            for frame in frames {
                let ids: Vec<String> = match args.and_then(|a| a.first()) {
                    Some(e) => match eval_expr(e, frame, graph, symtab)? {
                        CypherValue::List(items) => items
                            .iter()
                            .map(|v| match v {
                                CypherValue::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .collect(),
                        CypherValue::String(s) => vec![s],
                        CypherValue::Null => Vec::new(),
                        other => vec![other.to_string()],
                    },
                    None => Vec::new(),
                };
                for id in ids {
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("connectionId", CypherValue::String(id.clone())),
                        ("username", CypherValue::String(String::new())),
                        (
                            "message",
                            CypherValue::String("Connection not found".into()),
                        ),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                    out.push(nf);
                }
            }
            return Ok(out);
        }
        "db.schema.nodeTypeProperties" | "db.schema.relTypeProperties" => {
            return Ok(emit(
                frames,
                schema_type_property_rows(graph, procedure_name == "db.schema.relTypeProperties")?,
            ));
        }
        _ => {}
    }

    let values: Vec<String> = match procedure_name {
        "db.labels" => graph.label_names(),
        "db.propertyKeys" => graph.property_key_names(),
        "db.relationshipTypes" => graph.relationship_type_names(),
        _ => Vec::new(),
    };
    let col = cols[0];
    // Resolve the target slot: explicit `YIELD col AS bound`, else the
    // default output-column name.
    let target = yield_columns
        .iter()
        .find(|(o, _)| o == col)
        .map(|(_, b)| b.as_str())
        .unwrap_or(col);
    let slot = symtab.slot(target);
    let mut out = Vec::with_capacity(frames.len().saturating_mul(values.len()));
    for frame in frames {
        for v in &values {
            let mut nf = frame.clone();
            if let Some(s) = slot {
                nf[s] = CypherValue::String(v.clone()).into();
            }
            out.push(nf);
        }
    }
    Ok(out)
}

/// Rows for `db.schema.nodeTypeProperties` / `relTypeProperties`: samples entities per label /
/// relationship type (bounded, like `db.schema.visualization`), grouping property occurrence +
/// Neo4j-style type names; `mandatory` = on every sampled entity of the group.
pub(super) fn schema_type_property_rows(
    graph: &dyn GraphView,
    relationships: bool,
) -> Result<Vec<Vec<(&'static str, CypherValue)>>, CypherError> {
    use std::collections::{BTreeMap, BTreeSet};
    use traverse_core::graph::property::PropertyValue;

    const SAMPLE_CAP: usize = 10_000;

    fn type_name(pv: &PropertyValue) -> &'static str {
        match pv {
            PropertyValue::Null => "Null",
            PropertyValue::Bool(_) => "Boolean",
            PropertyValue::Int(_) => "Long",
            PropertyValue::Float(_) => "Double",
            PropertyValue::String(_) => "String",
            PropertyValue::Bytes(_) => "ByteArray",
            PropertyValue::List(items) => match items.first() {
                Some(PropertyValue::Int(_)) => "LongArray",
                Some(PropertyValue::Float(_)) => "DoubleArray",
                Some(PropertyValue::String(_)) => "StringArray",
                Some(PropertyValue::Bool(_)) => "BooleanArray",
                _ => "AnyArray",
            },
            PropertyValue::Map(_) => "Map",
            PropertyValue::DateTime(_) => "DateTime",
            PropertyValue::Duration(_) => "Duration",
            PropertyValue::Point2D { .. } | PropertyValue::Point3D { .. } => "Point",
            PropertyValue::Vector(_) => "FloatArray",
        }
    }

    // group key (label set / rel type) -> (entity count, prop -> (types, count))
    let mut groups: BTreeMap<
        Vec<String>,
        (usize, BTreeMap<String, (BTreeSet<&'static str>, usize)>),
    > = BTreeMap::new();

    if relationships {
        for rt in graph.relationship_type_names() {
            crate::query::check_interrupt()?;
            for eid in graph.edges_by_type(&rt).into_iter().take(SAMPLE_CAP) {
                let Ok(edge) = graph.get_edge(eid) else {
                    continue;
                };
                let entry = groups.entry(vec![rt.clone()]).or_default();
                entry.0 += 1;
                for (kid, pv) in edge.properties.iter() {
                    let name = graph.strings().resolve(kid.0).to_string();
                    let slot = entry.1.entry(name).or_default();
                    slot.0.insert(type_name(&pv));
                    slot.1 += 1;
                }
            }
        }
    } else {
        for label in graph.label_names() {
            crate::query::check_interrupt()?;
            for nid in graph.nodes_by_label(&label).into_iter().take(SAMPLE_CAP) {
                let Ok(node) = graph.get_node(nid) else {
                    continue;
                };
                let mut labels: Vec<String> = node
                    .labels
                    .iter()
                    .map(|l| graph.strings().resolve(l.0).to_string())
                    .collect();
                labels.sort();
                // multi-label nodes are visited per label; count them under their label-set group
                // only on the FIRST label
                if labels.first().map(String::as_str) != Some(label.as_str()) {
                    continue;
                }
                let entry = groups.entry(labels).or_default();
                entry.0 += 1;
                for (kid, pv) in node.properties.iter() {
                    let name = graph.strings().resolve(kid.0).to_string();
                    let slot = entry.1.entry(name).or_default();
                    slot.0.insert(type_name(&pv));
                    slot.1 += 1;
                }
            }
        }
    }

    let mut rows = Vec::new();
    for (key, (total, props)) in groups {
        let type_str = format!(
            ":{}",
            key.iter()
                .map(|l| format!("`{l}`"))
                .collect::<Vec<_>>()
                .join(":")
        );
        if props.is_empty() {
            // entities without properties still give a row with null property info, as in Neo4j
            let mut row: Vec<(&'static str, CypherValue)> = vec![
                ("propertyName", CypherValue::Null),
                ("propertyTypes", CypherValue::Null),
                ("mandatory", CypherValue::Bool(false)),
            ];
            if relationships {
                row.insert(0, ("relType", CypherValue::String(type_str.clone())));
            } else {
                row.insert(0, ("nodeType", CypherValue::String(type_str.clone())));
                row.insert(
                    1,
                    (
                        "nodeLabels",
                        CypherValue::List(
                            key.iter().map(|l| CypherValue::String(l.clone())).collect(),
                        ),
                    ),
                );
            }
            rows.push(row);
            continue;
        }
        for (prop, (types, count)) in props {
            let mut row: Vec<(&'static str, CypherValue)> = vec![
                ("propertyName", CypherValue::String(prop)),
                (
                    "propertyTypes",
                    CypherValue::List(
                        types
                            .iter()
                            .map(|t| CypherValue::String((*t).to_string()))
                            .collect(),
                    ),
                ),
                ("mandatory", CypherValue::Bool(count == total)),
            ];
            if relationships {
                row.insert(0, ("relType", CypherValue::String(type_str.clone())));
            } else {
                row.insert(0, ("nodeType", CypherValue::String(type_str.clone())));
                row.insert(
                    1,
                    (
                        "nodeLabels",
                        CypherValue::List(
                            key.iter().map(|l| CypherValue::String(l.clone())).collect(),
                        ),
                    ),
                );
            }
            rows.push(row);
        }
    }
    Ok(rows)
}
