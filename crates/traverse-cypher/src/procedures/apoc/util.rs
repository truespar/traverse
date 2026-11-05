//! The small namespaces: `apoc.util`, `apoc.lock`, `apoc.static`, `apoc.log`, `apoc.help`,
//! `apoc.warmup`, `apoc.example`, `apoc.coll`, `apoc.text`, `apoc.math`, `apoc.atomic` and
//! `apoc.convert.setJsonProperty`.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::{ProcCall, run_inner_cypher};
use crate::types::CypherValue;

use super::node_ref_of;

/// The small namespaces this module covers.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.util.validate(predicate, message, params): raises when true
        "apoc.util.validate" => {
            for frame in frames {
                if matches!(
                    eval_expr(cx.arg(0)?, frame, &*graph, symtab)?,
                    CypherValue::Bool(true)
                ) {
                    let msg = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                        CypherValue::String(s) => s,
                        o => o.to_string(),
                    };
                    return Err(CypherError::semantic(msg));
                }
            }
            Ok(frames.to_vec())
        }
        // locks mean nothing with a single writer
        "apoc.lock.all"
        | "apoc.lock.nodes"
        | "apoc.lock.rels"
        | "apoc.lock.read.nodes"
        | "apoc.lock.read.rels" => Ok(frames.to_vec()),
        "apoc.util.sleep" => {
            for frame in frames {
                if let CypherValue::Integer(ms) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    // cap at 60 s, respect the query deadline
                    let ms = ms.clamp(0, 60_000) as u64;
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            Ok(frames.to_vec())
        }
        "apoc.coll.pairwithoffset"
        | "apoc.coll.partition"
        | "apoc.coll.split"
        | "apoc.coll.ziptorows" => cx.emit_rows(&|frame| {
            // reuse the pure functions; unwrap the outer list into one row per element
            let fname = name_lower.replace("ziptorows", "zip");
            let a0 = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
            let a1 = args
                .and_then(|a| a.get(1))
                .map(|e| eval_expr(e, frame, &*graph, symtab))
                .transpose()?
                .unwrap_or(CypherValue::Null);
            let out = crate::apoc::eval(&fname, &[a0, a1]).unwrap_or(Ok(CypherValue::Null))?;
            Ok(match out {
                CypherValue::List(l) => l.into_iter().map(|v| vec![("value", v)]).collect(),
                _ => Vec::new(),
            })
        }),
        "apoc.coll.elements" => cx.emit_rows(&|frame| {
            let v = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
            let CypherValue::List(l) = v else {
                return Ok(Vec::new());
            };
            let limit = args
                .and_then(|a| a.get(1))
                .map(|e| eval_expr(e, frame, &*graph, symtab))
                .transpose()?
                .and_then(|v| match v {
                    CypherValue::Integer(n) => Some(n),
                    _ => None,
                })
                .unwrap_or(-1);
            let offset = args
                .and_then(|a| a.get(2))
                .map(|e| eval_expr(e, frame, &*graph, symtab))
                .transpose()?
                .and_then(|v| match v {
                    CypherValue::Integer(n) => Some(n.max(0) as usize),
                    _ => None,
                })
                .unwrap_or(0);
            let end = if limit < 0 {
                l.len()
            } else {
                (offset + limit as usize).min(l.len())
            };
            let window = l.get(offset..end).unwrap_or(&[]);
            const COLS: [&str; 10] = ["_1", "_2", "_3", "_4", "_5", "_6", "_7", "_8", "_9", "_10"];
            let mut row: Vec<(&'static str, CypherValue)> = Vec::new();
            for (i, col) in COLS.iter().enumerate() {
                row.push((col, window.get(i).cloned().unwrap_or(CypherValue::Null)));
            }
            row.push(("elements", CypherValue::Integer(window.len() as i64)));
            Ok(vec![row])
        }),
        "apoc.text.phoneticdelta" => cx.emit_rows(&|frame| {
            let a = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
            let b = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
            let p1 =
                crate::apoc::eval("apoc.text.phonetic", &[a]).unwrap_or(Ok(CypherValue::Null))?;
            let p2 =
                crate::apoc::eval("apoc.text.phonetic", &[b]).unwrap_or(Ok(CypherValue::Null))?;
            let delta = match (&p1, &p2) {
                (CypherValue::String(x), CypherValue::String(y)) => {
                    x.chars().zip(y.chars()).filter(|(a, b)| a == b).count() as i64
                }
                _ => 0,
            };
            Ok(vec![vec![
                ("phonetic1", p1),
                ("phonetic2", p2),
                ("delta", CypherValue::Integer(delta)),
            ]])
        }),
        "apoc.help" => cx.emit_rows(&|frame| {
            let needle = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                CypherValue::String(s) => s.to_lowercase(),
                _ => String::new(),
            };
            let mut rows = Vec::new();
            for (kind, name) in crate::apoc::help_catalog() {
                if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                    continue;
                }
                rows.push(vec![
                    ("type", CypherValue::String(kind.to_string())),
                    ("name", CypherValue::String(name.to_string())),
                    (
                        "text",
                        CypherValue::String("Traverse APOC compatibility layer".to_string()),
                    ),
                    ("signature", CypherValue::String(String::new())),
                    ("roles", CypherValue::List(Vec::new())),
                    ("writes", CypherValue::Bool(false)),
                    ("core", CypherValue::Bool(true)),
                    ("isDeprecated", CypherValue::Bool(false)),
                ]);
            }
            Ok(rows)
        }),
        "apoc.math.regr" => cx.emit_rows(&|frame| {
            let label = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                CypherValue::String(s) => s,
                _ => return Ok(Vec::new()),
            };
            let (py, px) = (
                match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => return Ok(Vec::new()),
                },
                match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => return Ok(Vec::new()),
                },
            );
            let mut xs = Vec::new();
            let mut ys = Vec::new();
            for nid in graph.nodes_by_label(&label) {
                let x = graph.get_node_property(nid, &px).ok().flatten();
                let y = graph.get_node_property(nid, &py).ok().flatten();
                if let (Some(x), Some(y)) = (x, y) {
                    let fx = match x {
                        traverse_core::graph::property::PropertyValue::Int(i) => i as f64,
                        traverse_core::graph::property::PropertyValue::Float(f) => f,
                        _ => continue,
                    };
                    let fy = match y {
                        traverse_core::graph::property::PropertyValue::Int(i) => i as f64,
                        traverse_core::graph::property::PropertyValue::Float(f) => f,
                        _ => continue,
                    };
                    xs.push(fx);
                    ys.push(fy);
                }
            }
            let n = xs.len() as f64;
            if n < 2.0 {
                return Ok(vec![vec![
                    ("r2", CypherValue::Float(0.0)),
                    ("avgX", CypherValue::Float(0.0)),
                    ("avgY", CypherValue::Float(0.0)),
                    ("slope", CypherValue::Float(0.0)),
                ]]);
            }
            let mx = xs.iter().sum::<f64>() / n;
            let my = ys.iter().sum::<f64>() / n;
            let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
            let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
            let syy: f64 = ys.iter().map(|y| (y - my) * (y - my)).sum();
            let slope = if sxx == 0.0 { 0.0 } else { sxy / sxx };
            let r2 = if sxx == 0.0 || syy == 0.0 {
                0.0
            } else {
                (sxy * sxy) / (sxx * syy)
            };
            Ok(vec![vec![
                ("r2", CypherValue::Float(r2)),
                ("avgX", CypherValue::Float(mx)),
                ("avgY", CypherValue::Float(my)),
                ("slope", CypherValue::Float(slope)),
            ]])
        }),
        "apoc.atomic.add"
        | "apoc.atomic.subtract"
        | "apoc.atomic.concat"
        | "apoc.atomic.insert"
        | "apoc.atomic.remove"
        | "apoc.atomic.update" => {
            let mut out = Vec::new();
            for frame in frames {
                let container = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let prop = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let mut operand = args
                    .and_then(|a| a.get(2))
                    .map(|e| eval_expr(e, frame, &*graph, symtab))
                    .transpose()?
                    .unwrap_or(CypherValue::Null);
                if name_lower.ends_with("insert") {
                    // insert(container, prop, position, value): pack position + value for the
                    // helper
                    let value = args
                        .and_then(|a| a.get(3))
                        .map(|e| eval_expr(e, frame, &*graph, symtab))
                        .transpose()?
                        .unwrap_or(CypherValue::Null);
                    operand = CypherValue::List(vec![operand, value]);
                }
                let (old_v, new_v) = atomic_apply(graph, &container, &prop, name_lower, &operand)?;
                let mut nf = frame.clone();
                for (col, val) in [
                    ("container", container.clone()),
                    ("property", CypherValue::String(prop.clone())),
                    ("oldValue", old_v),
                    ("newValue", new_v),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.convert.setjsonproperty" => {
            for frame in frames {
                let node = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let key = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let value = eval_expr(cx.arg(2)?, frame, &*graph, symtab)?;
                let Some(nid) = node_ref_of(&node) else {
                    continue;
                };
                let json = serde_json::to_string(&crate::apoc::cypher_to_json_pure(&value))
                    .unwrap_or_else(|_| "null".to_string());
                let pid = graph.intern_property_key(&key);
                graph.unindex_node(nid);
                if let Ok(n) = graph.get_node_mut(nid) {
                    n.properties.insert(
                        pid,
                        traverse_core::graph::property::PropertyValue::String(json),
                    );
                }
                graph.index_node(nid);
                crate::executor::inc_stat(|s| s.properties_set += 1);
            }
            Ok(frames.to_vec())
        }
        // -- apoc.static.* (server-lifetime value store) ------
        "apoc.static.get" => {
            let mut out = Vec::new();
            for frame in frames {
                let key = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let v = crate::apoc::static_store()
                    .read()
                    .ok()
                    .and_then(|m| m.get(&key).cloned())
                    .unwrap_or(CypherValue::Null);
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("value") {
                    nf[sl] = v.into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.static.set" => {
            let mut out = Vec::new();
            for frame in frames {
                let key = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let value = eval_expr(cx.arg(1)?, frame, &*graph, symtab)?;
                let previous = crate::apoc::static_store()
                    .write()
                    .ok()
                    .and_then(|mut m| {
                        if matches!(value, CypherValue::Null) {
                            m.remove(&key)
                        } else {
                            m.insert(key.clone(), value.clone())
                        }
                    })
                    .unwrap_or(CypherValue::Null);
                let mut nf = frame.clone();
                // Yields the PREVIOUS value (APOC semantics).
                if let Some(sl) = cx.slot_for("value") {
                    nf[sl] = previous.into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.static.list" => {
            let mut out = Vec::new();
            for frame in frames {
                let prefix = match cx.opt_arg(0, frame, &*graph) {
                    Some(CypherValue::String(s)) => s,
                    _ => String::new(),
                };
                let entries: Vec<(String, CypherValue)> = crate::apoc::static_store()
                    .read()
                    .map(|m| {
                        let mut v: Vec<(String, CypherValue)> = m
                            .iter()
                            .filter(|(k, _)| k.starts_with(&prefix))
                            .map(|(k, val)| (k.clone(), val.clone()))
                            .collect();
                        v.sort_by(|a, b| a.0.cmp(&b.0));
                        v
                    })
                    .unwrap_or_default();
                for (k, v) in entries {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("key") {
                        nf[sl] = CypherValue::String(k).into();
                    }
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = v.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // -- apoc.example.movies - the bundled Movies dataset --
        "apoc.example.movies" => {
            let t0 = web_time::Instant::now();
            const MOVIES: &str = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/apoc_movies.cypher"
            ));
            let before = crate::executor::get_query_stats();
            run_inner_cypher(graph, MOVIES, Default::default())?;
            let after = crate::executor::get_query_stats();
            let mut out = Vec::new();
            for frame in frames {
                let mut nf = frame.clone();
                for (col, val) in [
                    ("file", CypherValue::String("movies.cypher".to_string())),
                    (
                        "source",
                        CypherValue::String(
                            "example movie database from themoviedb.org".to_string(),
                        ),
                    ),
                    ("format", CypherValue::String("cypher".to_string())),
                    (
                        "nodes",
                        CypherValue::Integer(after.nodes_created - before.nodes_created),
                    ),
                    (
                        "relationships",
                        CypherValue::Integer(
                            after.relationships_created - before.relationships_created,
                        ),
                    ),
                    (
                        "properties",
                        CypherValue::Integer(after.properties_set - before.properties_set),
                    ),
                    (
                        "time",
                        CypherValue::Integer(t0.elapsed().as_millis() as i64),
                    ),
                    ("rows", CypherValue::Integer(0)),
                    ("batchSize", CypherValue::Integer(-1)),
                    ("batches", CypherValue::Integer(0)),
                    ("done", CypherValue::Bool(true)),
                    ("data", CypherValue::Null),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        // -- apoc.log.stream - server log lines ---------------
        "apoc.log.stream" => {
            let mut out = Vec::new();
            for frame in frames {
                let path = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let config = match cx.opt_arg(1, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let last = match config.get("last") {
                    Some(CypherValue::Integer(n)) => Some((*n).max(0) as usize),
                    _ => None,
                };
                // A real file at `path` wins (e.g. --log-dir's
                // traverse.log); otherwise the in-process ring buffer.
                let fs_path = path.strip_prefix("file://").unwrap_or(&path);
                let lines: Vec<String> = if std::path::Path::new(fs_path).is_file() {
                    std::fs::read_to_string(fs_path)
                        .map_err(|e| {
                            CypherError::runtime(format!(
                                "apoc.log.stream: cannot read '{fs_path}': {e}"
                            ))
                        })?
                        .lines()
                        .map(str::to_string)
                        .collect()
                } else {
                    crate::apoc_log::snapshot()
                };
                let skip = match last {
                    Some(n) if n < lines.len() => lines.len() - n,
                    _ => 0,
                };
                for (i, line) in lines.into_iter().enumerate().skip(skip) {
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("lineNo", CypherValue::Integer(i as i64)),
                        ("line", CypherValue::String(line)),
                        ("path", CypherValue::String(path.clone())),
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
        // -- apoc.warmup.run - meaningful no-op (in-memory engine) --
        "apoc.warmup.run" => {
            let mut out = Vec::new();
            for frame in frames {
                let mut nf = frame.clone();
                for (col, val) in [
                    ("pageSize", CypherValue::Integer(0)),
                    ("totalTime", CypherValue::Integer(0)),
                    (
                        "nodesTotal",
                        CypherValue::Integer(graph.node_count() as i64),
                    ),
                    ("relsTotal", CypherValue::Integer(graph.edge_count() as i64)),
                    ("transactionWasTerminated", CypherValue::Bool(false)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}

/// apoc.atomic.* property mutation. Single writer, so no retries; retryAttempts is accepted and
/// ignored.
fn atomic_apply(
    graph: &mut dyn GraphView,
    container: &CypherValue,
    prop: &str,
    op: &str,
    operand: &CypherValue,
) -> Result<(CypherValue, CypherValue), CypherError> {
    enum Target {
        Node(traverse_core::graph::types::NodeId),
        Edge(traverse_core::graph::types::EdgeId),
    }
    let target = match container {
        CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => Target::Node(*id),
        CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => Target::Edge(*id),
        _ => {
            return Err(CypherError::type_error(
                "apoc.atomic.* needs a NODE or RELATIONSHIP container".to_string(),
            ));
        }
    };
    let old_pv = match &target {
        Target::Node(id) => graph.get_node_property(*id, prop).ok().flatten(),
        Target::Edge(id) => graph.get_edge_property(*id, prop).ok().flatten(),
    };
    let old_v = old_pv
        .as_ref()
        .map(CypherValue::from_property)
        .unwrap_or(CypherValue::Null);
    let num = |v: &CypherValue| -> f64 {
        match v {
            CypherValue::Integer(i) => *i as f64,
            CypherValue::Float(f) => *f,
            _ => 0.0,
        }
    };
    let both_int = matches!(
        (&old_v, operand),
        (CypherValue::Integer(_), CypherValue::Integer(_))
    ) || matches!(
        (&old_v, operand),
        (CypherValue::Null, CypherValue::Integer(_))
    );
    let new_v: CypherValue = if op.ends_with("add") || op.ends_with("subtract") {
        let delta = if op.ends_with("add") {
            num(operand)
        } else {
            -num(operand)
        };
        let r = num(&old_v) + delta;
        if both_int {
            CypherValue::Integer(r as i64)
        } else {
            CypherValue::Float(r)
        }
    } else if op.ends_with("concat") {
        let a = match &old_v {
            CypherValue::String(s) => s.clone(),
            CypherValue::Null => String::new(),
            other => other.to_string(),
        };
        let b = match operand {
            CypherValue::String(s) => s.clone(),
            other => other.to_string(),
        };
        CypherValue::String(format!("{a}{b}"))
    } else if op.ends_with("insert") {
        // insert(container, prop, position, value): the caller packs the operand as LIST [position,
        // value]
        let (pos, value) = match operand {
            CypherValue::List(l) if l.len() == 2 => (
                match &l[0] {
                    CypherValue::Integer(i) => (*i).max(0) as usize,
                    _ => 0,
                },
                l[1].clone(),
            ),
            other => (0, other.clone()),
        };
        let mut items = match &old_v {
            CypherValue::List(l) => l.clone(),
            CypherValue::Null => Vec::new(),
            other => vec![other.clone()],
        };
        items.insert(pos.min(items.len()), value);
        CypherValue::List(items)
    } else if op.ends_with("remove") {
        let pos = match operand {
            CypherValue::Integer(i) => *i,
            _ => -1,
        };
        let mut items = match &old_v {
            CypherValue::List(l) => l.clone(),
            _ => Vec::new(),
        };
        if pos >= 0 && (pos as usize) < items.len() {
            items.remove(pos as usize);
        }
        CypherValue::List(items)
    } else {
        // update(container, prop, operation): the operation may reference `n.<prop>`; it's
        // rewritten to `$n`, bound to the current value, and run as a one-row inner query
        let expr = match operand {
            CypherValue::String(s) => s.clone(),
            _ => String::new(),
        };
        let mut params = std::collections::HashMap::new();
        params.insert("n".to_string(), old_v.clone());
        let rewritten = expr.replace(&format!("n.{prop}"), "$n");
        let result = run_inner_cypher(graph, &format!("RETURN {rewritten} AS v"), params)?;
        result
            .rows
            .first()
            .and_then(|r| r.first())
            .cloned()
            .unwrap_or(CypherValue::Null)
    };
    let Some(pv) = new_v.to_property() else {
        return Err(CypherError::type_error(
            "atomic result is not storable".to_string(),
        ));
    };
    let pid = graph.intern_property_key(prop);
    match target {
        Target::Node(id) => {
            graph.unindex_node(id);
            if let Ok(n) = graph.get_node_mut(id) {
                n.properties.insert(pid, pv);
            }
            graph.index_node(id);
        }
        Target::Edge(id) => {
            graph.unindex_edge(id);
            if let Ok(e) = graph.get_edge_mut(id) {
                e.properties.insert(pid, pv);
            }
            graph.index_edge(id);
        }
    }
    crate::executor::inc_stat(|s| s.properties_set += 1);
    Ok((old_v, new_v))
}
