//! `gds.*`: Neo4j GDS compatibility.

use traverse_core::tx::view::GraphView;

use crate::apoc::key_string;
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::types::CypherValue;

use super::{ProcCall, run_inner_cypher};

/// `gds.*` compatibility: the graph catalog plus the mapped algorithm surface (delegating to the
/// native `traverse.*` engines with GDS argument shape and column names).
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    use crate::gds_catalog as cat;
    let ProcCall {
        name: procedure_name,
        frames,
        symtab,
        ..
    } = *cx;
    let proj_row =
        |frames: &[Frame], p: &cat::GdsProjection, extra: &[(&str, CypherValue)]| -> Vec<Frame> {
            let mut out = Vec::new();
            for frame in frames {
                let mut nf = frame.clone();
                for (col, val) in [
                    ("graphName", CypherValue::String(p.name.clone())),
                    ("nodeProjection", p.node_projection.clone()),
                    ("relationshipProjection", p.rel_projection.clone()),
                    ("nodeCount", CypherValue::Integer(p.node_count)),
                    ("relationshipCount", CypherValue::Integer(p.rel_count)),
                    ("projectMillis", CypherValue::Integer(0)),
                    ("database", CypherValue::String("traverse".to_string())),
                    ("configuration", CypherValue::Map(Default::default())),
                    ("schema", CypherValue::Map(Default::default())),
                    ("degreeDistribution", CypherValue::Map(Default::default())),
                    (
                        "memoryUsage",
                        CypherValue::String(
                            p.materialized
                                .as_ref()
                                .map(|m| format!("{} Bytes", m.size_in_bytes()))
                                .unwrap_or_default(),
                        ),
                    ),
                    (
                        "sizeInBytes",
                        CypherValue::Integer(
                            p.materialized
                                .as_ref()
                                .map(|m| m.size_in_bytes() as i64)
                                .unwrap_or(-1),
                        ),
                    ),
                    ("creationTime", CypherValue::Integer(0)),
                    ("modificationTime", CypherValue::Integer(0)),
                ]
                .into_iter()
                .chain(extra.iter().cloned())
                {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            out
        };

    match procedure_name {
        // -- Catalog -------------------------------------------
        "gds.graph.project.cypher" => {
            let mut out = Vec::new();
            for frame in frames {
                let name = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let node_q = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => {
                        return Err(CypherError::type_error(
                            "nodeQuery must be a STRING".to_string(),
                        ));
                    }
                };
                let rel_q = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => {
                        return Err(CypherError::type_error(
                            "relationshipQuery must be a STRING".to_string(),
                        ));
                    }
                };
                let mat = build_cypher_projection(graph, &node_q, &rel_q)?;
                let p = cat::GdsProjection {
                    name: name.clone(),
                    node_labels: vec![],
                    rel_types: vec![],
                    rel_weight_property: mat.has_weights().then(|| "weight".to_string()),
                    node_count: mat.orig_ids.len() as i64,
                    rel_count: crate::algorithms::projection::AlgorithmGraph::edge_count(&mat)
                        as i64,
                    node_projection: CypherValue::String(node_q.clone()),
                    rel_projection: CypherValue::String(rel_q.clone()),
                    materialized: Some(std::sync::Arc::new(mat)),
                };
                let (nc, rc) = (p.node_count, p.rel_count);
                cat::put(p);
                let mut nf = frame.clone();
                for (col, val) in [
                    ("graphName", CypherValue::String(name.clone())),
                    ("nodeQuery", CypherValue::String(node_q.clone())),
                    ("relationshipQuery", CypherValue::String(rel_q.clone())),
                    ("nodeCount", CypherValue::Integer(nc)),
                    ("relationshipCount", CypherValue::Integer(rc)),
                    ("projectMillis", CypherValue::Integer(0)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        "gds.graph.project" => {
            let mut out = Vec::new();
            for frame in frames {
                let name = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let node_proj_v = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                let rel_proj_v = eval_expr(cx.arg(2)?, frame, &*graph, symtab)?;
                let node_labels = cat::parse_node_projection(&node_proj_v)?;
                let (rel_types, mut weight, undirected) = cat::parse_rel_projection(&rel_proj_v)?;
                // 4-arg form: gds.graph.project(name, nodes, rels,
                // {relationshipProperties: 'distance'}) - the weight
                // arrives in the trailing CONFIG map (graph-examples
                // GDS guide shape), not the rel projection.
                if weight.is_none()
                    && let Some(CypherValue::Map(cfg)) = cx.opt_arg(3, frame, &*graph)
                {
                    weight = match cfg.get("relationshipProperties") {
                        Some(CypherValue::String(s)) => Some(s.clone()),
                        Some(CypherValue::List(l)) => l.iter().find_map(|v| {
                            if let CypherValue::String(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        }),
                        _ => None,
                    };
                }
                // Counts under the projection's filters.
                let node_count = if node_labels.is_empty() {
                    graph.node_count() as i64
                } else {
                    let mut ids: Vec<_> = node_labels
                        .iter()
                        .flat_map(|l| graph.nodes_by_label(l))
                        .collect();
                    ids.sort_unstable_by_key(|n| n.0);
                    ids.dedup();
                    ids.len() as i64
                };
                let rel_count = if rel_types.is_empty() {
                    graph.edge_count() as i64
                } else {
                    rel_types
                        .iter()
                        .map(|t| graph.edges_by_type(t).len() as i64)
                        .sum()
                };
                // GDS counts an UNDIRECTED relationship once per direction
                let rel_count = if undirected { 2 * rel_count } else { rel_count };
                // Materialize the snapshot NOW - every stream/stats
                // call reuses it (O(projection) allocations instead of
                // O(database) per call; snapshot semantics like GDS).
                let mat = crate::algorithms::materialized::MaterializedGraph::build(
                    &*graph,
                    &node_labels,
                    &rel_types,
                    weight.as_deref(),
                    undirected,
                )?;
                let p = cat::GdsProjection {
                    name,
                    node_labels,
                    rel_types,
                    rel_weight_property: weight,
                    node_count,
                    rel_count,
                    node_projection: node_proj_v,
                    rel_projection: rel_proj_v,
                    materialized: Some(std::sync::Arc::new(mat)),
                };
                cat::put(p.clone());
                out.extend(proj_row(std::slice::from_ref(frame), &p, &[]));
            }
            Ok(out)
        }
        "gds.graph.exists" => {
            let mut out = Vec::new();
            for frame in frames {
                let name = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let exists = cat::get(&name).is_some();
                let mut nf = frame.clone();
                for (col, val) in [
                    ("graphName", CypherValue::String(name)),
                    ("exists", CypherValue::Bool(exists)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        "gds.graph.list" => {
            let mut out = Vec::new();
            for frame in frames {
                let filter = cx.opt_arg(0, frame, &*graph);
                for p in cat::list() {
                    if let Some(CypherValue::String(want)) = &filter
                        && &p.name != want
                    {
                        continue;
                    }
                    out.extend(proj_row(std::slice::from_ref(frame), &p, &[]));
                }
            }
            Ok(out)
        }
        "gds.graph.drop" => {
            let mut out = Vec::new();
            for frame in frames {
                let name = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let fail_if_missing = !matches!(
                    cx.opt_arg(1, frame, &*graph),
                    Some(CypherValue::Bool(false))
                );
                match cat::drop_projection(&name) {
                    Some(p) => out.extend(proj_row(std::slice::from_ref(frame), &p, &[])),
                    None if fail_if_missing => {
                        return Err(CypherError::semantic(format!(
                            "Graph with name `{name}` does not exist on database `traverse`."
                        )));
                    }
                    None => {}
                }
            }
            Ok(out)
        }
        // -- gds.graph.nodeProperties.* - projection overlays --
        "gds.graph.nodeProperties.stream" | "gds.graph.nodeProperty.stream" => {
            let singular = procedure_name.ends_with("nodeProperty.stream");
            let mut out = Vec::new();
            for frame in frames {
                let gname = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let Some(p) = cat::get(&gname) else {
                    return Err(CypherError::semantic(format!(
                        "Graph with name `{gname}` does not exist on database `traverse`."
                    )));
                };
                let props: Vec<String> = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => vec![s],
                    CypherValue::List(l) => l.iter().map(key_string).collect(),
                    _ => Vec::new(),
                };
                let Some(mat) = &p.materialized else {
                    return Err(CypherError::semantic(format!(
                        "{procedure_name}: graph `{gname}` has no materialized projection"
                    )));
                };
                let overlay = mat.overlays.read().unwrap_or_else(|e| e.into_inner());
                let labels_val = CypherValue::List(
                    p.node_labels
                        .iter()
                        .map(|l| CypherValue::String(l.clone()))
                        .collect(),
                );
                for prop in &props {
                    let Some(values) = overlay.get(prop) else {
                        return Err(CypherError::semantic(format!(
                            "{procedure_name}: property `{prop}` does not exist on graph `{gname}` \
                             (mutate it first: CALL gds.<algo>.mutate('{gname}', {{mutateProperty: '{prop}'}}))"
                        )));
                    };
                    for (dense, v) in values.iter().enumerate() {
                        if matches!(v, CypherValue::Null) {
                            continue;
                        }
                        let orig = mat.orig_ids[dense] as i64;
                        let mut nf = frame.clone();
                        for (col, val) in [
                            ("nodeId", CypherValue::Integer(orig)),
                            ("nodeProperty", CypherValue::String(prop.clone())),
                            ("propertyValue", v.clone()),
                            ("nodeLabels", labels_val.clone()),
                        ] {
                            // The singular form has no nodeProperty column.
                            if singular && col == "nodeProperty" {
                                continue;
                            }
                            if let Some(sl) = cx.slot_for(col) {
                                nf[sl] = val.into();
                            }
                        }
                        out.push(nf);
                    }
                }
            }
            Ok(out)
        }
        "gds.graph.nodeProperties.write" => {
            let mut out = Vec::new();
            for frame in frames {
                let t0 = web_time::Instant::now();
                let gname = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let Some(p) = cat::get(&gname) else {
                    return Err(CypherError::semantic(format!(
                        "Graph with name `{gname}` does not exist on database `traverse`."
                    )));
                };
                let props: Vec<String> = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => vec![s],
                    CypherValue::List(l) => l.iter().map(key_string).collect(),
                    _ => Vec::new(),
                };
                let Some(mat) = p.materialized.clone() else {
                    return Err(CypherError::semantic(format!(
                        "{procedure_name}: graph `{gname}` has no materialized projection"
                    )));
                };
                // Snapshot the overlay values first (read lock must not
                // be held across graph mutation).
                let to_write: Vec<(String, Vec<CypherValue>)> = {
                    let overlay = mat.overlays.read().unwrap_or_else(|e| e.into_inner());
                    props
                        .iter()
                        .map(|prop| {
                            overlay
                                .get(prop)
                                .map(|v| (prop.clone(), v.clone()))
                                .ok_or_else(|| {
                                    CypherError::semantic(format!(
                                        "{procedure_name}: property `{prop}` does not exist \
                                         on graph `{gname}`"
                                    ))
                                })
                        })
                        .collect::<Result<_, _>>()?
                };
                let mut written: i64 = 0;
                for (prop, values) in &to_write {
                    for (dense, v) in values.iter().enumerate() {
                        let Some(pv) = v.to_property() else { continue };
                        if matches!(v, CypherValue::Null) {
                            continue;
                        }
                        let orig = traverse_core::graph::types::NodeId(mat.orig_ids[dense]);
                        graph.unindex_node_prop(orig, prop);
                        let pid = graph.intern_property_key(prop);
                        if let Ok(n) = graph.get_node_mut(orig) {
                            n.properties.insert(pid, pv);
                            written += 1;
                        }
                        graph.index_node_prop(orig, prop);
                    }
                    crate::executor::inc_stat(|s| {
                        s.properties_set += values.len() as i64;
                    });
                }
                let mut nf = frame.clone();
                for (col, val) in [
                    (
                        "writeMillis",
                        CypherValue::Integer(t0.elapsed().as_millis() as i64),
                    ),
                    ("graphName", CypherValue::String(gname.clone())),
                    (
                        "nodeProperties",
                        CypherValue::List(
                            props
                                .iter()
                                .map(|s| CypherValue::String(s.clone()))
                                .collect(),
                        ),
                    ),
                    ("propertiesWritten", CypherValue::Integer(written)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        "gds.graph.nodeProperties.drop" => {
            let mut out = Vec::new();
            for frame in frames {
                let gname = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let Some(p) = cat::get(&gname) else {
                    return Err(CypherError::semantic(format!(
                        "Graph with name `{gname}` does not exist on database `traverse`."
                    )));
                };
                let props: Vec<String> = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => vec![s],
                    CypherValue::List(l) => l.iter().map(key_string).collect(),
                    _ => Vec::new(),
                };
                let Some(mat) = &p.materialized else {
                    return Err(CypherError::semantic(format!(
                        "{procedure_name}: graph `{gname}` has no materialized projection"
                    )));
                };
                let mut removed: i64 = 0;
                {
                    let mut overlay = mat.overlays.write().unwrap_or_else(|e| e.into_inner());
                    for prop in &props {
                        if let Some(vals) = overlay.remove(prop) {
                            removed += vals
                                .iter()
                                .filter(|v| !matches!(v, CypherValue::Null))
                                .count() as i64;
                        }
                    }
                }
                let mut nf = frame.clone();
                for (col, val) in [
                    ("graphName", CypherValue::String(gname.clone())),
                    (
                        "nodeProperties",
                        CypherValue::List(
                            props
                                .iter()
                                .map(|s| CypherValue::String(s.clone()))
                                .collect(),
                        ),
                    ),
                    ("propertiesRemoved", CypherValue::Integer(removed)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        // -- Algorithms: gds.<algo>.<mode> ---------------------
        other => {
            let rest = other.strip_prefix("gds.").unwrap_or(other);
            // `.estimate` suffix -> memory-estimation stub.
            if let Some(base) = rest.strip_suffix(".estimate") {
                let _ = base;
                let mut out = Vec::new();
                for frame in frames {
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("requiredMemory", CypherValue::String("0 Bytes".to_string())),
                        ("treeView", CypherValue::String(String::new())),
                        ("mapView", CypherValue::Map(Default::default())),
                        ("bytesMin", CypherValue::Integer(0)),
                        ("bytesMax", CypherValue::Integer(0)),
                        ("nodeCount", CypherValue::Integer(graph.node_count() as i64)),
                        (
                            "relationshipCount",
                            CypherValue::Integer(graph.edge_count() as i64),
                        ),
                        ("heapPercentageMin", CypherValue::Float(0.0)),
                        ("heapPercentageMax", CypherValue::Float(0.0)),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                    out.push(nf);
                }
                return Ok(out);
            }
            let (algo_path, mode) = match rest.rsplit_once('.') {
                Some((a, m)) if matches!(m, "stream" | "stats" | "write" | "mutate") => (a, m),
                _ => {
                    return Err(CypherError::semantic(format!(
                        "Unknown procedure: {other} (not in Traverse's GDS compatibility subset)"
                    ))
                    .with_status(crate::status::GqlStatus::UnknownProcedure));
                }
            };

            let Some((native, renames, extra_nulls)) = cat::algo_mapping(algo_path) else {
                return Err(CypherError::semantic(format!(
                    "gds.{algo_path}: no Traverse engine mapping yet (mapped: pageRank, wcc, \
                     louvain, labelPropagation, betweenness, degree, nodeSimilarity, knn, \
                     fastRP, shortestPath.dijkstra)"
                )));
            };

            let mut out = Vec::new();
            for frame in frames {
                // (graphName, config) - graphName resolves projection
                // filters; an anonymous config-only call also works.
                let mut config: std::collections::BTreeMap<String, CypherValue> =
                    Default::default();
                let mut materialized: Option<
                    std::sync::Arc<crate::algorithms::materialized::MaterializedGraph>,
                > = None;

                let gname = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                match gname {
                    CypherValue::String(name) => {
                        let Some(p) = cat::get(&name) else {
                            return Err(CypherError::semantic(format!(
                                "Graph with name `{name}` does not exist on database `traverse`. \
                                 Project it first: CALL gds.graph.project('{name}', '*', '*')"
                            )));
                        };
                        if let Some(CypherValue::Map(m)) = cx.opt_arg(1, frame, &*graph) {
                            config = m.into_iter().collect();
                        }
                        // Stream/stats run on the materialized snapshot
                        // (O(projection) work); write mode and the
                        // property-reading aStar heuristic path use the
                        // live graph with merged filters.
                        let use_mat = matches!(mode, "stream" | "stats" | "mutate")
                            && algo_path != "shortestPath.astar";
                        check_weight_property(&config, p.rel_weight_property.as_deref())
                            .map_err(|e| CypherError::semantic(format!("{other}: {e}")))?;
                        if use_mat {
                            materialized = p.materialized.clone();
                        }
                        if materialized.is_none() {
                            cat::merge_projection_into_config(&p, &mut config);
                        }
                    }
                    CypherValue::Map(m) => {
                        // Anonymous syntax: config as the only argument.
                        config = m.into_iter().collect();
                        // Cypher projection: nodeQuery + relationshipQuery
                        // build a one-shot materialized graph.
                        if let (Some(CypherValue::String(nq)), Some(CypherValue::String(rq))) =
                            (config.get("nodeQuery"), config.get("relationshipQuery"))
                        {
                            let (nq, rq) = (nq.clone(), rq.clone());
                            let mat = build_cypher_projection(graph, &nq, &rq)?;
                            config.remove("nodeQuery");
                            config.remove("relationshipQuery");
                            check_weight_property(&config, mat.has_weights().then_some("weight"))
                                .map_err(|e| CypherError::semantic(format!("{other}: {e}")))?;
                            materialized = Some(std::sync::Arc::new(mat));
                        }
                    }
                    other_v => {
                        return Err(CypherError::type_error(format!(
                            "{other}: first argument must be a graph name or config map, got {}",
                            other_v.type_name()
                        )));
                    }
                }
                // GDS's sourceNode/targetNode may be Node values, and the
                // native engines name the keys sourceNodeId/targetNodeId.
                // On the materialized path the engine speaks DENSE ids.
                for (gds_key, native_key) in [
                    ("sourceNode", "sourceNodeId"),
                    ("targetNode", "targetNodeId"),
                ] {
                    if let Some(v) = config.remove(gds_key) {
                        let mut id = match &v {
                            CypherValue::NodeRef(id) => id.0 as i64,
                            CypherValue::Node { id, .. } => id.0 as i64,
                            CypherValue::Integer(i) => *i,
                            _ => {
                                config.insert(native_key.to_string(), v);
                                continue;
                            }
                        };
                        if let Some(mat) = &materialized {
                            let Some(&dense) = mat.orig_to_dense.get(&(id as u64)) else {
                                return Err(CypherError::semantic(format!(
                                    "{other}: node {id} is not in the projected graph"
                                )));
                            };
                            id = dense as i64;
                        }
                        config.insert(native_key.to_string(), CypherValue::Integer(id));
                    }
                }

                // GDS nodeSimilarity runs on the projection's
                // orientation (NATURAL by default) and emits BOTH
                // orientations of every pair.
                if algo_path == "nodeSimilarity" {
                    config
                        .entry("orientation".to_string())
                        .or_insert_with(|| CypherValue::String("NATURAL".to_string()));
                }
                // GDS's knn is PROPERTY-based (nodeProperties config);
                // route it to the native knnProperty engine. Without
                // nodeProperties, keep the topological-knn mapping.
                let native = if algo_path == "knn" && config.contains_key("nodeProperties") {
                    "knnProperty"
                } else {
                    native
                };
                // GDS's `scaler` (centralities): the native engines don't know it; applied to the
                // final scores below
                let scaler = match config.remove("scaler") {
                    Some(v) if matches!(algo_path, "pageRank" | "articleRank" | "eigenvector") => {
                        let s = Scaler::parse(&v)
                            .map_err(|e| CypherError::semantic(format!("{other}: {e}")))?;
                        if s.is_some() && !matches!(mode, "stream" | "mutate") {
                            return Err(CypherError::semantic(format!(
                                "{other}: scaler is supported in stream and mutate mode"
                            )));
                        }
                        s
                    }
                    Some(v) => {
                        config.insert("scaler".to_string(), v);
                        None
                    }
                    None => None,
                };
                // pageRank runs the exact GDS iteration (the native one is normalized), mutate too:
                // a rescale can't reproduce GDS on graphs with dangling nodes, whose sink mass GDS
                // evaporates (graph-examples airports: sum 1992 != N 3503)
                if algo_path == "pageRank" {
                    config.insert("__gds_mode".to_string(), CypherValue::Bool(true));
                }
                // -- mutate: run stream on the snapshot, store the value
                // column as a projection overlay, emit the summary row.
                if mode == "mutate" {
                    let Some(mat) = &materialized else {
                        return Err(CypherError::semantic(format!(
                            "{other}: mutate requires a materialized catalog graph \
                             (CALL gds.graph.project first)"
                        )));
                    };
                    let Some(CypherValue::String(mutate_prop)) = config.remove("mutateProperty")
                    else {
                        return Err(CypherError::semantic(format!(
                            "{other}: mutateProperty is required for mutate mode"
                        )));
                    };
                    let t0 = web_time::Instant::now();
                    let stream_name = format!("traverse.{native}.stream");
                    let unweighted = crate::algorithms::materialized::Unweighted(&**mat);
                    let proj: &dyn crate::algorithms::projection::AlgorithmGraph = if mat
                        .has_weights()
                        && !config.contains_key("relationshipWeightProperty")
                    {
                        &unweighted
                    } else {
                        &**mat
                    };
                    let result = match crate::algorithms::procedure::run_for_call_on(
                        &stream_name,
                        &config,
                        proj,
                        None,
                    ) {
                        Some(Ok(r)) => r,
                        Some(Err(e)) => return Err(e.into_cypher(other)),
                        None => {
                            return Err(CypherError::semantic(format!(
                                "{other}: native engine `{stream_name}` is not registered"
                            )));
                        }
                    };
                    let mut result = result;
                    if let Some(s) = &scaler {
                        s.apply(&mut result.rows);
                    }
                    let compute_ms = t0.elapsed().as_millis() as i64;
                    // Value column: the per-node payload in NATIVE naming.
                    const VALUE_COLS: &[&str] =
                        &["score", "communityId", "embedding", "triangleCount"];
                    let n = mat.orig_ids.len();
                    let mut values = vec![CypherValue::Null; n];
                    let mut written: i64 = 0;
                    for row in &result.rows {
                        let dense = row.iter().find_map(|(k, v)| {
                            (k.as_ref() == "nodeId").then_some(match v {
                                CypherValue::Integer(i) => *i as usize,
                                _ => usize::MAX,
                            })
                        });
                        let val = row
                            .iter()
                            .find_map(|(k, v)| VALUE_COLS.contains(&k.as_ref()).then(|| v.clone()));
                        if let (Some(d), Some(v)) = (dense, val)
                            && d < n
                        {
                            values[d] = v;
                            written += 1;
                        }
                    }
                    if written == 0 && !result.rows.is_empty() {
                        return Err(CypherError::semantic(format!(
                            "{other}: this algorithm's output kind is not mutatable \
                             (per-node scalar, community, or embedding kinds only)"
                        )));
                    }
                    mat.overlays
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(mutate_prop, values);
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("nodePropertiesWritten", CypherValue::Integer(written)),
                        (
                            "mutateMillis",
                            CypherValue::Integer(t0.elapsed().as_millis() as i64),
                        ),
                        ("nodeCount", CypherValue::Integer(n as i64)),
                        ("preProcessingMillis", CypherValue::Integer(0)),
                        ("postProcessingMillis", CypherValue::Integer(0)),
                        ("computeMillis", CypherValue::Integer(compute_ms)),
                        (
                            "configuration",
                            CypherValue::Map(gds_visible_config(&config)),
                        ),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                    out.push(nf);
                    continue;
                }

                let native_name = format!("traverse.{native}.{mode}");
                // single-source paths read only the ends and the cost unless the query yields
                // nodeIds, costs, or a path built from them (bellmanFord's route): the engine then
                // keeps the distances instead of rebuilding every path
                const PATH_ENDS: &[&str] = &["sourceNodeId", "targetNodeId", "totalCost"];
                let native_yield = (mode == "stream"
                    && matches!(
                        algo_path,
                        "allShortestPaths.delta" | "allShortestPaths.dijkstra" | "bellmanFord"
                    )
                    && ["nodeIds", "costs", "path", "route"]
                        .iter()
                        .all(|c| cx.slot_for(c).is_none()))
                .then_some(PATH_ENDS);
                let dispatch = match &materialized {
                    // weights only when the algorithm's configuration asks for them, as in GDS
                    Some(mat)
                        if mat.has_weights()
                            && !config.contains_key("relationshipWeightProperty") =>
                    {
                        crate::algorithms::procedure::run_for_call_on(
                            &native_name,
                            &config,
                            &crate::algorithms::materialized::Unweighted(&**mat),
                            native_yield,
                        )
                    }
                    Some(mat) => crate::algorithms::procedure::run_for_call_on(
                        &native_name,
                        &config,
                        &**mat,
                        native_yield,
                    ),
                    None => crate::algorithms::procedure::run_for_call(
                        &native_name,
                        &config,
                        graph,
                        native_yield,
                    ),
                };
                let mut result = match dispatch {
                    Some(Ok(r)) => r,
                    Some(Err(e)) => return Err(e.into_cypher(other)),
                    None => {
                        return Err(CypherError::semantic(format!(
                            "{other}: native engine `{native_name}` is not registered"
                        )));
                    }
                };
                // GDS's single-source shortest paths include the source itself, at cost 0, even
                // with nothing else reachable (a projection holds the source: it was resolved
                // above; on the live graph a found path proves it). The engines stream every
                // reachable node but the source, so it is never there already
                if matches!(
                    algo_path,
                    "allShortestPaths.delta" | "allShortestPaths.dijkstra" | "bellmanFord"
                ) && mode == "stream"
                    // (a target, which only Traverse takes, asks for that one path)
                    && !config.contains_key("targetNodeId")
                    && let Some(CypherValue::Integer(src)) = config.get("sourceNodeId")
                    && (materialized.is_some() || !result.rows.is_empty())
                {
                    let id = CypherValue::Integer(*src);
                    result.rows.insert(
                        0,
                        vec![
                            (std::borrow::Cow::Borrowed("sourceNodeId"), id.clone()),
                            (std::borrow::Cow::Borrowed("targetNodeId"), id.clone()),
                            (
                                std::borrow::Cow::Borrowed("totalCost"),
                                CypherValue::Float(0.0),
                            ),
                            (
                                std::borrow::Cow::Borrowed("nodeIds"),
                                CypherValue::List(vec![id]),
                            ),
                            (
                                std::borrow::Cow::Borrowed("edgeIds"),
                                CypherValue::List(vec![]),
                            ),
                            (
                                std::borrow::Cow::Borrowed("costs"),
                                CypherValue::List(vec![CypherValue::Float(0.0)]),
                            ),
                        ],
                    );
                }
                // Materialized path: results speak DENSE ids - translate
                // back to original ids at the boundary. communityId is
                // included because the engines use representative node
                // ids as community labels.
                if let Some(mat) = &materialized {
                    let remap = |v: &mut CypherValue| {
                        if let CypherValue::Integer(i) = v
                            && *i >= 0
                            && let Some(&orig) = mat.orig_ids.get(*i as usize)
                        {
                            *v = CypherValue::Integer(orig as i64);
                        }
                    };
                    for row in &mut result.rows {
                        for (name, val) in row.iter_mut() {
                            match name.as_ref() {
                                // kCore reuses the communityId column
                                // for core DEPTH - not a node id.
                                "communityId" if algo_path != "kcore" => remap(val),
                                "nodeId" | "sourceNodeId" | "targetNodeId" => remap(val),
                                "nodeIds" => {
                                    if let CypherValue::List(items) = val {
                                        for item in items {
                                            remap(item);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                // GDS's bfs / dfs stream ONE row per traversal: the nodes in visit order and a path
                // through them over virtual NEXT relationships. The engine streams each visited
                // node's path, in visit order; the source alone still makes a row
                if matches!(algo_path, "bfs" | "dfs")
                    && mode == "stream"
                    && !config.contains_key("targetNodeId")
                    && let Some(CypherValue::Integer(src)) = config.get("sourceNodeId")
                    && (materialized.is_some() || !result.rows.is_empty())
                {
                    let src = match &materialized {
                        Some(mat) => mat.orig_ids.get(*src as usize).map_or(*src, |&o| o as i64),
                        None => *src,
                    };
                    let mut ids = vec![src];
                    for row in &result.rows {
                        if let Some((_, CypherValue::List(p))) =
                            row.iter().find(|(n, _)| n.as_ref() == "nodeIds")
                            && let Some(CypherValue::Integer(t)) = p.last()
                        {
                            ids.push(*t);
                        }
                    }
                    // the path only when the query takes it: every node is read in full
                    let path = cx.slot_for("path").map(|_| {
                        let nodes = ids
                            .iter()
                            .map(|&id| {
                                let nid = traverse_core::graph::types::NodeId(id as u64);
                                match graph.get_node(nid) {
                                    Ok(n) => crate::executor::node_to_cypher_value(&n, &*graph),
                                    Err(_) => CypherValue::NodeRef(nid),
                                }
                            })
                            .collect();
                        let edges = ids
                            .windows(2)
                            .enumerate()
                            .map(|(i, w)| CypherValue::Edge {
                                id: traverse_core::graph::types::EdgeId(u64::MAX - i as u64),
                                edge_type: "NEXT".to_string(),
                                source: traverse_core::graph::types::NodeId(w[0] as u64),
                                target: traverse_core::graph::types::NodeId(w[1] as u64),
                                properties: Default::default(),
                            })
                            .collect();
                        CypherValue::Path { nodes, edges }
                    });
                    result.rows = vec![vec![
                        (
                            std::borrow::Cow::Borrowed("sourceNodeId"),
                            CypherValue::Integer(src),
                        ),
                        (
                            std::borrow::Cow::Borrowed("nodeIds"),
                            CypherValue::List(ids.into_iter().map(CypherValue::Integer).collect()),
                        ),
                        (
                            std::borrow::Cow::Borrowed("path"),
                            path.unwrap_or(CypherValue::Null),
                        ),
                    ]];
                }
                // Path-returning algorithms: GDS yields `path` as a real
                // Path - full nodes joined by VIRTUAL `PATH_0`
                // relationships whose `cost` is the per-segment delta.
                // Build it from nodeIds + costs (original ids by now).
                // (only when the query takes it: every node of every path is read in full)
                let path_col = if algo_path == "bellmanFord" {
                    "route"
                } else {
                    "path"
                };
                if (algo_path.starts_with("shortestPath")
                    || algo_path.starts_with("allShortestPaths")
                    || algo_path == "bellmanFord")
                    && cx.slot_for(path_col).is_some()
                {
                    for row in &mut result.rows {
                        let node_ids: Option<Vec<u64>> = row
                            .iter()
                            .find(|(n, _)| n.as_ref() == "nodeIds")
                            .and_then(|(_, v)| match v {
                                CypherValue::List(l) => Some(
                                    l.iter()
                                        .filter_map(|x| match x {
                                            CypherValue::Integer(i) if *i >= 0 => Some(*i as u64),
                                            _ => None,
                                        })
                                        .collect(),
                                ),
                                _ => None,
                            });
                        let costs: Option<Vec<f64>> = row
                            .iter()
                            .find(|(n, _)| n.as_ref() == "costs")
                            .and_then(|(_, v)| match v {
                                CypherValue::List(l) => Some(
                                    l.iter()
                                        .filter_map(|x| match x {
                                            CypherValue::Float(f) => Some(*f),
                                            CypherValue::Integer(i) => Some(*i as f64),
                                            _ => None,
                                        })
                                        .collect(),
                                ),
                                _ => None,
                            });
                        let (Some(ids), Some(costs)) = (node_ids, costs) else {
                            continue;
                        };
                        if ids.is_empty() || ids.len() != costs.len() {
                            continue;
                        }
                        let mut nodes = Vec::with_capacity(ids.len());
                        for id in &ids {
                            let nid = traverse_core::graph::types::NodeId(*id);
                            if let Ok(n) = graph.get_node(nid) {
                                nodes.push(crate::executor::node_to_cypher_value(&n, &*graph));
                            } else {
                                nodes.push(CypherValue::NodeRef(nid));
                            }
                        }
                        let mut edges = Vec::with_capacity(ids.len().saturating_sub(1));
                        for i in 0..ids.len().saturating_sub(1) {
                            let mut props = traverse_core::graph::property::PropertyMap::new();
                            props.insert(
                                "cost".to_string(),
                                traverse_core::graph::property::PropertyValue::Float(
                                    costs[i + 1] - costs[i],
                                ),
                            );
                            edges.push(CypherValue::Edge {
                                id: traverse_core::graph::types::EdgeId(u64::MAX - i as u64),
                                edge_type: "PATH_0".to_string(),
                                source: traverse_core::graph::types::NodeId(ids[i]),
                                target: traverse_core::graph::types::NodeId(ids[i + 1]),
                                properties: props,
                            });
                        }
                        let path = CypherValue::Path { nodes, edges };
                        if let Some(slot) = row.iter_mut().find(|(n, _)| n.as_ref() == path_col) {
                            slot.1 = path;
                        } else {
                            row.push((std::borrow::Cow::Borrowed(path_col), path));
                        }
                    }
                }
                // GDS's hits.stream yields (nodeId, values MAP) - fold the
                // native auth/hub columns into that map.
                if algo_path == "hits" && mode == "stream" {
                    for row in &mut result.rows {
                        let mut values = std::collections::BTreeMap::new();
                        row.retain(|(n, v)| {
                            if matches!(n.as_ref(), "auth" | "hub") {
                                values.insert(n.to_string(), v.clone());
                                false
                            } else {
                                true
                            }
                        });
                        row.push((
                            std::borrow::Cow::Borrowed("values"),
                            CypherValue::Map(values),
                        ));
                    }
                }
                // GDS's triangleCount.stream yields (nodeId,
                // triangleCount) - drop the native LCC extra (it lives
                // in gds.localClusteringCoefficient upstream).
                if algo_path == "triangleCount" && mode == "stream" {
                    for row in &mut result.rows {
                        row.retain(|(n, _)| n.as_ref() != "localClusteringCoefficient");
                    }
                }
                // (nodeSimilarity's per-source directional emission now
                // happens in the ENGINE - each node yields its own
                // top-K rows, matching GDS. The adapter used to mirror
                // rows here when the engine deduped; that would double
                // the stream now.)
                // pageRank runs the exact GDS iteration (__gds_mode); articleRank's engine is GDS's
                // own formula, unscaled
                if let Some(s) = &scaler {
                    s.apply(&mut result.rows);
                }
                // the slot of each GDS column, by the native name; the rows' columns usually repeat
                // in the same order, so each position remembers its last name
                let slot_of = |name: &str| {
                    let gds_name = renames
                        .iter()
                        .find(|(t, _)| *t == name)
                        .map(|(_, g)| *g)
                        .unwrap_or(name);
                    cx.slot_for(gds_name)
                };
                let mut row_slots: Vec<(String, Option<usize>)> = Vec::new();
                let extra_slots: Vec<(&str, usize)> = extra_nulls
                    .iter()
                    .filter_map(|col| Some((*col, cx.slot_for(col)?)))
                    .collect();
                // GDS summary rows carry `configuration` + pre/post processing timings, the same on
                // every row: built once, and only when yielded
                let mut summary: Vec<(usize, CypherValue)> = Vec::new();
                if let Some(sl) = cx.slot_for("configuration") {
                    summary.push((sl, CypherValue::Map(gds_visible_config(&config))));
                }
                for col in ["preProcessingMillis", "postProcessingMillis"] {
                    if let Some(sl) = cx.slot_for(col) {
                        summary.push((sl, CypherValue::Integer(0)));
                    }
                }
                for (row_idx, row) in result.rows.into_iter().enumerate() {
                    let mut nf = frame.clone();
                    // which extra columns the row lacks, before its values move into the frame
                    let lacks: Vec<bool> = extra_slots
                        .iter()
                        .map(|&(col, _)| !row.iter().any(|(n, _)| n.as_ref() == col))
                        .collect();
                    for (pos, (name, val)) in row.into_iter().enumerate() {
                        let name = name.as_ref();
                        let sl = match row_slots.get(pos) {
                            Some((known, sl)) if known == name => *sl,
                            _ => {
                                let sl = slot_of(name);
                                row_slots.truncate(pos);
                                row_slots.push((name.to_string(), sl));
                                sl
                            }
                        };
                        if let Some(sl) = sl {
                            nf[sl] = val.into();
                        }
                    }
                    for (&(col, sl), lacks) in extra_slots.iter().zip(lacks) {
                        // Never clobber a column the row actually
                        // produced (e.g. hits' folded `values` map).
                        if !lacks {
                            continue;
                        }
                        let v = match col {
                            "index" => CypherValue::Integer(row_idx as i64),
                            // the engine fails on a negative cycle: every path it streams is
                            // free of one
                            "isNegativeCycle" => CypherValue::Bool(false),
                            _ => CypherValue::Null,
                        };
                        nf[sl] = v.into();
                    }
                    for (sl, v) in &summary {
                        nf[*sl] = v.clone().into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
    }
}

/// Materialized projection from GDS *Cypher projection* inputs: `nodeQuery`
/// returns an `id` column, `relationshipQuery` returns `source`, `target` and
/// optionally `weight`. For `gds.graph.project.cypher` and anonymous
/// `{nodeQuery, relationshipQuery}` algorithm configs.
fn build_cypher_projection(
    graph: &mut dyn GraphView,
    node_query: &str,
    rel_query: &str,
) -> Result<crate::algorithms::materialized::MaterializedGraph, CypherError> {
    let nodes_res = run_inner_cypher(graph, node_query, Default::default())?;
    let id_col = nodes_res
        .columns
        .iter()
        .position(|c| c == "id")
        .ok_or_else(|| CypherError::semantic("nodeQuery must return an `id` column".to_string()))?;
    let mut nodes = Vec::with_capacity(nodes_res.rows.len());
    for row in &nodes_res.rows {
        match row.get(id_col) {
            Some(CypherValue::Integer(i)) if *i >= 0 => nodes.push(*i as u64),
            Some(CypherValue::NodeRef(id)) => nodes.push(id.0),
            _ => {}
        }
    }
    let rels_res = run_inner_cypher(graph, rel_query, Default::default())?;
    let col = |name: &str| rels_res.columns.iter().position(|c| c == name);
    let (Some(sc), Some(tc)) = (col("source"), col("target")) else {
        return Err(CypherError::semantic(
            "relationshipQuery must return `source` and `target` columns".to_string(),
        ));
    };
    let wc = col("weight");
    let mut edges = Vec::with_capacity(rels_res.rows.len());
    for row in &rels_res.rows {
        let as_id = |v: Option<&CypherValue>| match v {
            Some(CypherValue::Integer(i)) if *i >= 0 => Some(*i as u64),
            Some(CypherValue::NodeRef(id)) => Some(id.0),
            _ => None,
        };
        let (Some(s), Some(t)) = (as_id(row.get(sc)), as_id(row.get(tc))) else {
            continue;
        };
        let w = wc.and_then(|c| match row.get(c) {
            Some(CypherValue::Float(f)) => Some(*f),
            Some(CypherValue::Integer(i)) => Some(*i as f64),
            _ => None,
        });
        edges.push((s, t, w));
    }
    crate::algorithms::materialized::MaterializedGraph::from_rows(nodes, &edges)
        .map_err(CypherError::semantic)
}

/// The configuration a summary row echoes: without the adapter's internal `__` keys.
fn gds_visible_config(
    config: &std::collections::BTreeMap<String, CypherValue>,
) -> std::collections::BTreeMap<String, CypherValue> {
    config
        .iter()
        .filter(|(k, _)| !k.starts_with("__"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// GDS weighs an algorithm only by a property its graph projected: `relationshipWeightProperty`
/// naming any other is an error (the snapshot holds the projected weights only).
fn check_weight_property(
    config: &std::collections::BTreeMap<String, CypherValue>,
    projected: Option<&str>,
) -> Result<(), String> {
    match config.get("relationshipWeightProperty") {
        Some(CypherValue::String(want)) if projected != Some(want.as_str()) => Err(format!(
            "Relationship weight property `{want}` not found in the projected relationships. \
             Properties existing on all relationship types: [{}]",
            projected.map(|p| format!("'{p}'")).unwrap_or_default()
        )),
        _ => Ok(()),
    }
}

/// GDS's score scalers (`scaler` of pageRank, articleRank and eigenvector), by name or as
/// `{type: name}` (Log also takes `offset`). A zero denominator scales to 0, as in GDS.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Scaler {
    MinMax,
    Max,
    Mean,
    Log(f64),
    L1Norm,
    L2Norm,
    StdScore,
    Center,
}

impl Scaler {
    /// `None` for `NONE`.
    fn parse(v: &CypherValue) -> Result<Option<Self>, String> {
        let (name, offset) = match v {
            CypherValue::String(s) => (s.to_string(), 0.0),
            CypherValue::Map(m) => {
                let name = match m.get("type") {
                    Some(CypherValue::String(s)) => s.to_string(),
                    _ => return Err("a scaler map needs a `type`".to_string()),
                };
                let offset = match m.get("offset") {
                    Some(CypherValue::Float(f)) => *f,
                    Some(CypherValue::Integer(i)) => *i as f64,
                    _ => 0.0,
                };
                (name, offset)
            }
            other => {
                return Err(format!(
                    "scaler must be a string or map, got {}",
                    other.type_name()
                ));
            }
        };
        Ok(Some(match name.to_ascii_lowercase().as_str() {
            "none" => return Ok(None),
            "minmax" => Self::MinMax,
            "max" => Self::Max,
            "mean" => Self::Mean,
            "log" => Self::Log(offset),
            "l1norm" => Self::L1Norm,
            "l2norm" => Self::L2Norm,
            "stdscore" => Self::StdScore,
            "center" => Self::Center,
            _ => {
                return Err(format!(
                    "unknown scaler `{name}` (NONE, MINMAX, MAX, MEAN, LOG, L1NORM, L2NORM, \
                     STDSCORE, CENTER)"
                ));
            }
        }))
    }

    fn scale(self, xs: &mut [f64]) {
        if xs.is_empty() {
            return;
        }
        let n = xs.len() as f64;
        let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
        let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = xs.iter().sum::<f64>() / n;
        let div = |num: f64, den: f64| if den == 0.0 { 0.0 } else { num / den };
        match self {
            Self::MinMax => xs.iter_mut().for_each(|x| *x = div(*x - min, max - min)),
            Self::Max => {
                let m = xs.iter().fold(0.0f64, |a, x| a.max(x.abs()));
                xs.iter_mut().for_each(|x| *x = div(*x, m));
            }
            Self::Mean => xs.iter_mut().for_each(|x| *x = div(*x - mean, max - min)),
            Self::Log(offset) => xs.iter_mut().for_each(|x| *x = (*x + offset).ln()),
            Self::L1Norm => {
                let s: f64 = xs.iter().map(|x| x.abs()).sum();
                xs.iter_mut().for_each(|x| *x = div(*x, s));
            }
            Self::L2Norm => {
                let s = xs.iter().map(|x| x * x).sum::<f64>().sqrt();
                xs.iter_mut().for_each(|x| *x = div(*x, s));
            }
            Self::StdScore => {
                let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
                xs.iter_mut().for_each(|x| *x = div(*x - mean, sd));
            }
            Self::Center => xs.iter_mut().for_each(|x| *x -= mean),
        }
    }

    /// Scales the `score` column of `rows` across all of them.
    fn apply(self, rows: &mut [Vec<(std::borrow::Cow<'static, str>, CypherValue)>]) {
        let mut xs: Vec<f64> = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            for (n, v) in row {
                if n.as_ref() == "score" {
                    xs.push(match v {
                        CypherValue::Float(f) => *f,
                        CypherValue::Integer(i) => *i as f64,
                        _ => f64::NAN,
                    });
                }
            }
        }
        self.scale(&mut xs);
        let mut it = xs.into_iter();
        for row in rows.iter_mut() {
            for (n, v) in row.iter_mut() {
                if n.as_ref() == "score"
                    && let Some(x) = it.next()
                {
                    *v = CypherValue::Float(x);
                }
            }
        }
    }
}
