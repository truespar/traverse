//! Vector and full-text search procedures (`db.index.vector.queryNodes`,
//! `db.index.fulltext.queryNodes`) and the vector write shims.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::{self, Frame};
use crate::types::CypherValue;

use super::ProcCall;

/// Write-capable procedure shims: the deprecated procedural vector-index
/// creation API and the vector-property setters. Vector values store as
/// `LIST<FLOAT>` (Traverse's uniform vector model), so the HNSW picks
/// them up through normal index maintenance.
pub(super) fn write(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    use traverse_core::graph::property::PropertyValue;
    use traverse_core::graph::types::{EdgeId, NodeId};
    let ProcCall {
        name: procedure_name,
        yield_columns,
        frames,
        symtab,
        ..
    } = *cx;

    if procedure_name == "db.index.vector.createNodeIndex" {
        // DDL: evaluate once against an empty frame; apply once.
        let f = frame::new_frame(symtab);
        let name = match eval_expr(cx.arg(0)?, &f, &*graph, symtab)? {
            CypherValue::String(s) => s,
            o => {
                return Err(CypherError::type_error(format!(
                    "indexName must be a STRING, got {}",
                    o.type_name()
                )));
            }
        };
        let label = match eval_expr(cx.arg(1)?, &f, &*graph, symtab)? {
            CypherValue::String(s) => s,
            o => {
                return Err(CypherError::type_error(format!(
                    "label must be a STRING, got {}",
                    o.type_name()
                )));
            }
        };
        let prop = match eval_expr(cx.arg(2)?, &f, &*graph, symtab)? {
            CypherValue::String(s) => s,
            o => {
                return Err(CypherError::type_error(format!(
                    "propertyKey must be a STRING, got {}",
                    o.type_name()
                )));
            }
        };
        let dim = match eval_expr(cx.arg(3)?, &f, &*graph, symtab)? {
            CypherValue::Integer(n) if n > 0 => n as usize,
            o => {
                return Err(CypherError::type_error(format!(
                    "vectorDimension must be a positive INTEGER, got {o:?}"
                )));
            }
        };
        let sim_str = match eval_expr(cx.arg(4)?, &f, &*graph, symtab)? {
            CypherValue::String(s) => s,
            CypherValue::Null => "cosine".to_string(),
            o => {
                return Err(CypherError::type_error(format!(
                    "vectorSimilarityFunction must be a STRING, got {}",
                    o.type_name()
                )));
            }
        };
        let sim = traverse_core::graph::vector::Similarity::parse(&sim_str).ok_or_else(|| {
            CypherError::semantic(format!("unknown vector similarity function '{sim_str}'"))
        })?;
        graph
            .create_vector_index(
                &name,
                &label,
                &prop,
                dim,
                sim,
                traverse_core::graph::vector::HnswParams::default(),
            )
            .map_err(CypherError::from)?;
        crate::executor::inc_stat(|s| s.indexes_added += 1);
        return Ok(frames.to_vec());
    }

    // Vector-property setters - per input row.
    let returns_node = procedure_name == "db.create.setVectorProperty";
    let on_relationship = procedure_name == "db.create.setRelationshipVectorProperty";
    let mut out = Vec::with_capacity(frames.len());
    for frame in frames {
        let entity = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
        let key = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
            CypherValue::String(s) => s,
            o => {
                return Err(CypherError::type_error(format!(
                    "key must be a STRING, got {}",
                    o.type_name()
                )));
            }
        };
        let vector = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
            CypherValue::List(items) => {
                let floats = vector_from_list(&items)?;
                PropertyValue::List(
                    floats
                        .into_iter()
                        .map(|v| PropertyValue::Float(v as f64))
                        .collect(),
                )
            }
            CypherValue::Null => PropertyValue::Null,
            o => {
                return Err(CypherError::type_error(format!(
                    "vector must be a LIST<FLOAT>, got {}",
                    o.type_name()
                )));
            }
        };
        let pid = graph.intern_property_key(&key);
        if on_relationship {
            let eid: EdgeId = match &entity {
                CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => *id,
                o => {
                    return Err(CypherError::type_error(format!(
                        "relationship argument must be a RELATIONSHIP, got {}",
                        o.type_name()
                    )));
                }
            };
            graph.unindex_edge(eid);
            let edge = graph.get_edge_mut(eid).map_err(CypherError::from)?;
            edge.properties.insert(pid, vector);
            graph.index_edge(eid);
        } else {
            let nid: NodeId = match &entity {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => *id,
                o => {
                    return Err(CypherError::type_error(format!(
                        "node argument must be a NODE, got {}",
                        o.type_name()
                    )));
                }
            };
            graph.unindex_node(nid);
            let node = graph.get_node_mut(nid).map_err(CypherError::from)?;
            node.properties.insert(pid, vector);
            graph.index_node(nid);
            if returns_node {
                let mut nf = frame.clone();
                let target = yield_columns
                    .iter()
                    .find(|(o, _)| o == "node")
                    .map(|(_, b)| b.as_str())
                    .unwrap_or("node");
                if let Some(sl) = symtab.slot(target) {
                    nf[sl] = CypherValue::NodeRef(nid).into();
                }
                out.push(nf);
                crate::executor::inc_stat(|s| s.properties_set += 1);
                continue;
            }
        }
        crate::executor::inc_stat(|s| s.properties_set += 1);
        out.push(frame.clone());
    }
    Ok(out)
}

/// `db.index.vector.queryNodes(indexName, k, query)` - Neo4j's vector
/// k-ANN procedure. Yields `(node, score)` best-first per input row.
pub(super) fn query_nodes(
    cx: &ProcCall,
    graph: &mut dyn GraphView,
) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        yield_columns,
        frames,
        symtab,
        ..
    } = *cx;
    let arg_exprs = args.ok_or_else(|| {
        CypherError::semantic(
            "db.index.vector.queryNodes requires (indexName, k, query) arguments".to_string(),
        )
    })?;
    if arg_exprs.len() != 3 {
        return Err(CypherError::semantic(format!(
            "db.index.vector.queryNodes expects 3 arguments (indexName, numberOfNearestNeighbours, query), got {}",
            arg_exprs.len()
        )));
    }

    let mut out = Vec::new();
    for frame in frames {
        let name = match eval_expr(&arg_exprs[0], frame, graph, symtab)? {
            CypherValue::String(s) => s,
            other => {
                return Err(CypherError::type_error(format!(
                    "indexName must be a string, got {}",
                    other.type_name()
                )));
            }
        };
        let k = match eval_expr(&arg_exprs[1], frame, graph, symtab)? {
            CypherValue::Integer(n) if n > 0 => n as usize,
            other => {
                return Err(CypherError::type_error(format!(
                    "numberOfNearestNeighbours must be a positive integer, got {other:?}"
                )));
            }
        };
        let query = match eval_expr(&arg_exprs[2], frame, graph, symtab)? {
            CypherValue::List(items) => vector_from_list(&items)?,
            other => {
                return Err(CypherError::type_error(format!(
                    "query must be a LIST<FLOAT>, got {}",
                    other.type_name()
                )));
            }
        };

        let hits = graph
            .query_vector_index(&name, &query, k, None)
            .ok_or_else(|| {
                CypherError::semantic(format!("there is no such vector schema index: {name}"))
            })?;

        for (node_id, score) in hits {
            let mut nf = frame.clone();
            // Default (no YIELD) -> node, score; else honor the aliases.
            let (node_var, score_var) = if yield_columns.is_empty() {
                ("node".to_string(), "score".to_string())
            } else {
                let mut nv = None;
                let mut sv = None;
                for (output, bound) in yield_columns {
                    match output.as_str() {
                        "node" => nv = Some(bound.clone()),
                        "score" => sv = Some(bound.clone()),
                        _ => {}
                    }
                }
                (nv.unwrap_or_default(), sv.unwrap_or_default())
            };
            if let Some(slot) = symtab.slot(&node_var) {
                nf[slot] = CypherValue::NodeRef(node_id).into();
            }
            if let Some(slot) = symtab.slot(&score_var) {
                nf[slot] = CypherValue::Float(score).into();
            }
            out.push(nf);
        }
    }
    Ok(out)
}

/// `db.index.fulltext.queryNodes(indexName, queryString [, limit])` -
/// BM25 search returning `(node, score)` best-first. An optional third
/// integer argument caps the number of results (default 100).
pub(super) fn fulltext_query_nodes(
    cx: &ProcCall,
    graph: &mut dyn GraphView,
) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        yield_columns,
        frames,
        symtab,
        ..
    } = *cx;
    let arg_exprs = args.ok_or_else(|| {
        CypherError::semantic(
            "db.index.fulltext.queryNodes requires (indexName, query) arguments".to_string(),
        )
    })?;
    if arg_exprs.len() < 2 || arg_exprs.len() > 3 {
        return Err(CypherError::semantic(format!(
            "db.index.fulltext.queryNodes expects 2 or 3 arguments \
             (indexName, query [, limit]), got {}",
            arg_exprs.len()
        )));
    }

    let mut out = Vec::new();
    for frame in frames {
        let name = match eval_expr(&arg_exprs[0], frame, graph, symtab)? {
            CypherValue::String(s) => s,
            other => {
                return Err(CypherError::type_error(format!(
                    "indexName must be a string, got {}",
                    other.type_name()
                )));
            }
        };
        let query = match eval_expr(&arg_exprs[1], frame, graph, symtab)? {
            CypherValue::String(s) => s,
            other => {
                return Err(CypherError::type_error(format!(
                    "query must be a string, got {}",
                    other.type_name()
                )));
            }
        };
        let limit = if let Some(e) = arg_exprs.get(2) {
            match eval_expr(e, frame, graph, symtab)? {
                CypherValue::Integer(n) if n > 0 => n as usize,
                other => {
                    return Err(CypherError::type_error(format!(
                        "limit must be a positive integer, got {other:?}"
                    )));
                }
            }
        } else {
            100
        };

        let hits = graph
            .query_fulltext_index(&name, &query, limit)
            .ok_or_else(|| {
                CypherError::semantic(format!("there is no such fulltext schema index: {name}"))
            })?;

        for (node_id, score) in hits {
            let mut nf = frame.clone();
            let (node_var, score_var) = if yield_columns.is_empty() {
                ("node".to_string(), "score".to_string())
            } else {
                let mut nv = None;
                let mut sv = None;
                for (output, bound) in yield_columns {
                    match output.as_str() {
                        "node" => nv = Some(bound.clone()),
                        "score" => sv = Some(bound.clone()),
                        _ => {}
                    }
                }
                (nv.unwrap_or_default(), sv.unwrap_or_default())
            };
            if let Some(slot) = symtab.slot(&node_var) {
                nf[slot] = CypherValue::NodeRef(node_id).into();
            }
            if let Some(slot) = symtab.slot(&score_var) {
                nf[slot] = CypherValue::Float(score).into();
            }
            out.push(nf);
        }
    }
    Ok(out)
}

/// Cypher `LIST<FLOAT | INTEGER>` -> query vector for a vector index. Errors on non-numeric
/// elements.
pub(crate) fn vector_from_list(items: &[CypherValue]) -> Result<Vec<f32>, CypherError> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        match it {
            CypherValue::Float(f) => out.push(*f as f32),
            CypherValue::Integer(i) => out.push(*i as f32),
            other => {
                return Err(CypherError::type_error(format!(
                    "vector query element must be numeric, got {}",
                    other.type_name()
                )));
            }
        }
    }
    Ok(out)
}
