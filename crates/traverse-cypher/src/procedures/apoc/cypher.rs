//! `apoc.cypher.*` and the conditionals (`apoc.when`, `apoc.case`, `apoc.do.*`): dynamic Cypher.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::{ProcCall, run_inner_cypher, run_inner_cypher_timeboxed};
use crate::types::CypherValue;

/// `apoc.cypher.*`, `apoc.when`, `apoc.case` and `apoc.do.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.cypher.run(stmt, params) :: (value), dynamic execution
        "apoc.cypher.run" | "apoc.cypher.doit" => {
            let mut out = Vec::new();
            for frame in frames {
                let stmt = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    o => {
                        return Err(CypherError::type_error(format!(
                            "statement must be a STRING, got {}",
                            o.type_name()
                        )));
                    }
                };
                let params = match args.and_then(|a| a.get(1)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::Map(m) => m.into_iter().collect(),
                        _ => std::collections::HashMap::new(),
                    },
                    None => std::collections::HashMap::new(),
                };
                let result = run_inner_cypher(graph, &stmt, params)?;
                for row in &result.rows {
                    let mut m = std::collections::BTreeMap::new();
                    for (i, col) in result.columns.iter().enumerate() {
                        m.insert(
                            col.clone(),
                            row.get(i).cloned().unwrap_or(CypherValue::Null),
                        );
                    }
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = CypherValue::Map(m).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // apoc.when(cond, ifQuery, elseQuery, params) :: (value)
        "apoc.when" | "apoc.do.when" => {
            let mut out = Vec::new();
            for frame in frames {
                let cond = matches!(
                    eval_expr(cx.arg(0)?, frame, &*graph, symtab)?,
                    CypherValue::Bool(true)
                );
                let q = if cond {
                    eval_expr(cx.arg(1)?, frame, &*graph, symtab)?
                } else {
                    match args.and_then(|a| a.get(2)) {
                        Some(e) => eval_expr(e, frame, &*graph, symtab)?,
                        None => CypherValue::String(String::new()),
                    }
                };
                let CypherValue::String(q) = q else {
                    return Err(CypherError::type_error(
                        "apoc.when branch queries must be STRINGs".to_string(),
                    ));
                };
                if q.trim().is_empty() {
                    continue; // empty branch: no rows for this input row
                }
                let params = match args.and_then(|a| a.get(3)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::Map(m) => m.into_iter().collect(),
                        _ => std::collections::HashMap::new(),
                    },
                    None => std::collections::HashMap::new(),
                };
                let result = run_inner_cypher(graph, &q, params)?;
                for row in &result.rows {
                    let mut m = std::collections::BTreeMap::new();
                    for (i, col) in result.columns.iter().enumerate() {
                        m.insert(
                            col.clone(),
                            row.get(i).cloned().unwrap_or(CypherValue::Null),
                        );
                    }
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = CypherValue::Map(m).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // apoc.case([cond, query, ...], elseQuery, params) :: (value)
        "apoc.case" | "apoc.do.case" => {
            let mut out = Vec::new();
            for frame in frames {
                let conds = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::List(l) => l,
                    _ => Vec::new(),
                };
                let mut chosen: Option<String> = None;
                for pair in conds.chunks(2) {
                    if matches!(pair.first(), Some(CypherValue::Bool(true))) {
                        if let Some(CypherValue::String(q)) = pair.get(1) {
                            chosen = Some(q.clone());
                        }
                        break;
                    }
                }
                let q = match chosen {
                    Some(q) => q,
                    None => match args.and_then(|a| a.get(1)) {
                        Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => String::new(),
                        },
                        None => String::new(),
                    },
                };
                if q.trim().is_empty() {
                    continue;
                }
                let params = match args.and_then(|a| a.get(2)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::Map(m) => m.into_iter().collect(),
                        _ => Default::default(),
                    },
                    None => Default::default(),
                };
                let result = run_inner_cypher(graph, &q, params)?;
                for row in &result.rows {
                    let mut m = std::collections::BTreeMap::new();
                    for (i, col) in result.columns.iter().enumerate() {
                        m.insert(
                            col.clone(),
                            row.get(i).cloned().unwrap_or(CypherValue::Null),
                        );
                    }
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = CypherValue::Map(m).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.cypher.runwrite" | "apoc.cypher.runschema" | "apoc.cypher.runtimeboxed" => {
            // same engine paths as run/doIt; runTimeboxed's limit is a child of the running query,
            // so the outer limits keep applying
            let mut out = Vec::new();
            for frame in frames {
                let stmt = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let params = match args.and_then(|a| a.get(1)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::Map(m) => m.into_iter().collect(),
                        _ => Default::default(),
                    },
                    None => Default::default(),
                };
                let timebox = if name_lower.ends_with("runtimeboxed") {
                    match args
                        .and_then(|a| a.get(2))
                        .map(|e| eval_expr(e, frame, &*graph, symtab))
                        .transpose()?
                    {
                        Some(CypherValue::Integer(ms)) => {
                            Some(std::time::Duration::from_millis(ms.max(1) as u64))
                        }
                        None | Some(CypherValue::Null) => None,
                        Some(other) => {
                            return Err(CypherError::type_error(format!(
                                "apoc.cypher.runTimeboxed: timeout must be an INTEGER (ms), got {}",
                                other.type_name()
                            )));
                        }
                    }
                } else {
                    None
                };
                let result = match timebox {
                    Some(limit) => run_inner_cypher_timeboxed(graph, &stmt, params, limit)?,
                    None => run_inner_cypher(graph, &stmt, params)?,
                };
                for row in &result.rows {
                    let mut m = std::collections::BTreeMap::new();
                    for (i, col) in result.columns.iter().enumerate() {
                        m.insert(
                            col.clone(),
                            row.get(i).cloned().unwrap_or(CypherValue::Null),
                        );
                    }
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = CypherValue::Map(m).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.cypher.runmany" | "apoc.cypher.runmanyreadonly" => {
            let mut out = Vec::new();
            for frame in frames {
                let stmts = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let params: std::collections::HashMap<String, CypherValue> =
                    match args.and_then(|a| a.get(1)) {
                        Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                            CypherValue::Map(m) => m.into_iter().collect(),
                            _ => Default::default(),
                        },
                        None => Default::default(),
                    };
                let mut row_no = 0i64;
                for stmt in stmts.split(';') {
                    let stmt = stmt.trim();
                    if stmt.is_empty() {
                        continue;
                    }
                    let result = run_inner_cypher(graph, stmt, params.clone())?;
                    for row in &result.rows {
                        let mut m = std::collections::BTreeMap::new();
                        for (i, col) in result.columns.iter().enumerate() {
                            m.insert(
                                col.clone(),
                                row.get(i).cloned().unwrap_or(CypherValue::Null),
                            );
                        }
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("row") {
                            nf[sl] = CypherValue::Integer(row_no).into();
                        }
                        if let Some(sl) = cx.slot_for("result") {
                            nf[sl] = CypherValue::Map(m).into();
                        }
                        out.push(nf);
                        row_no += 1;
                    }
                }
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}
