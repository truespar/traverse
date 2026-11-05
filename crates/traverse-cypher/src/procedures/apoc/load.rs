//! `apoc.load.*`: JSON, XML and JDBC sources.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::ProcCall;
use crate::types::CypherValue;

/// `apoc.load.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall { frames, symtab, .. } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.load.json(pathOrUrl) :: (value): LOCAL FILES only, LOAD CSV's trust model.
        // Object -> one row, array -> one row per element, NDJSON as fallback
        "apoc.load.json" => {
            let mut out = Vec::new();
            for frame in frames {
                let path = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                if path.contains("://") && !path.starts_with("file://") {
                    return Err(CypherError::semantic(
                        "apoc.load.json supports local files only (same policy as LOAD CSV)"
                            .to_string(),
                    ));
                }
                let fpath = path.strip_prefix("file://").unwrap_or(&path);
                let text = std::fs::read_to_string(fpath).map_err(|e| {
                    CypherError::runtime(format!("apoc.load.json: cannot open '{fpath}': {e}"))
                })?;
                let docs: Vec<CypherValue> = match serde_json::from_str::<serde_json::Value>(&text)
                {
                    Ok(serde_json::Value::Array(a)) => {
                        a.iter().map(crate::apoc::json_to_cypher).collect()
                    }
                    Ok(v) => vec![crate::apoc::json_to_cypher(&v)],
                    Err(_) => {
                        // NDJSON: one document per line
                        let mut rows = Vec::new();
                        for line in text.lines() {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            let v =
                                serde_json::from_str::<serde_json::Value>(line).map_err(|e| {
                                    CypherError::runtime(format!(
                                        "apoc.load.json: invalid JSON: {e}"
                                    ))
                                })?;
                            rows.push(crate::apoc::json_to_cypher(&v));
                        }
                        rows
                    }
                };
                for doc in docs {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = doc.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        "apoc.load.jsonarray" => cx.emit_rows(&|frame| {
            let path = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                CypherValue::String(s) => s,
                _ => return Ok(Vec::new()),
            };
            if path.contains("://") && !path.starts_with("file://") {
                return Err(CypherError::semantic(
                    "apoc.load.jsonArray supports local files only".to_string(),
                ));
            }
            let fpath = path.strip_prefix("file://").unwrap_or(&path).to_string();
            let text = std::fs::read_to_string(&fpath)
                .map_err(|e| CypherError::runtime(format!("cannot open '{fpath}': {e}")))?;
            let j: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| CypherError::runtime(format!("invalid JSON: {e}")))?;
            let v = crate::apoc::json_to_cypher(&j);
            Ok(match v {
                CypherValue::List(l) => l.into_iter().map(|x| vec![("value", x)]).collect(),
                other => vec![vec![("value", other)]],
            })
        }),
        // -- apoc.load.jsonParams - HTTP with headers + payload --
        #[cfg(target_arch = "wasm32")]
        "apoc.load.jsonparams" => Err(CypherError::runtime(
            "apoc.load.jsonParams is not available in WASM builds",
        )),
        #[cfg(not(target_arch = "wasm32"))]
        "apoc.load.jsonparams" => {
            let mut out = Vec::new();
            for frame in frames {
                let url = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let headers = match cx.opt_arg(1, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let payload = match cx.opt_arg(2, frame, &*graph) {
                    Some(CypherValue::String(s)) if !s.is_empty() => Some(s),
                    _ => None,
                };
                let header_values: Vec<(String, String)> = headers
                    .iter()
                    .map(|(k, v)| {
                        let hv = match v {
                            CypherValue::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (k.to_string(), hv)
                    })
                    .collect();
                let resp = match payload {
                    Some(p) => {
                        let mut req = ureq::post(&url);
                        for (k, v) in &header_values {
                            req = req.header(k.as_str(), v.as_str());
                        }
                        req.send(p.as_str())
                    }
                    None => {
                        let mut req = ureq::get(&url);
                        for (k, v) in &header_values {
                            req = req.header(k.as_str(), v.as_str());
                        }
                        req.call()
                    }
                }
                .map_err(|e| CypherError::runtime(format!("apoc.load.jsonParams: {e}")))?;
                // Stream-parse the body; no size cap, as before.
                let body: serde_json::Value =
                    serde_json::from_reader(resp.into_body().into_reader())
                        .map_err(|e| CypherError::runtime(format!("apoc.load.jsonParams: {e}")))?;
                let values = match body {
                    serde_json::Value::Array(a) => a,
                    other => vec![other],
                };
                for v in values {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = crate::apoc::json_to_cypher(&v).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // -- apoc.load.jdbc - native drivers (sqlite / postgres) --
        "apoc.load.jdbc" => {
            let mut out = Vec::new();
            for frame in frames {
                let url = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let table_or_sql = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let sql = if table_or_sql
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("select")
                {
                    table_or_sql
                } else {
                    format!("SELECT * FROM {table_or_sql}")
                };
                for row in jdbc_query(&url, &sql)? {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("row") {
                        nf[sl] = row.into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // -- apoc.load.xml - local files, APOC map shape ------
        "apoc.load.xml" | "apoc.load.xmlsimple" => {
            let mut out = Vec::new();
            for frame in frames {
                let file = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                // (url, path = '/', config = {}, simple = false)
                if let Some(CypherValue::String(p)) = cx.opt_arg(1, frame, &*graph)
                    && !p.is_empty()
                    && p != "/"
                {
                    return Err(CypherError::semantic(
                        "apoc.load.xml: only the root path '/' is supported".to_string(),
                    ));
                }
                let simple = name_lower.ends_with("xmlsimple")
                    || matches!(cx.opt_arg(3, frame, &*graph), Some(CypherValue::Bool(true)));
                let text = crate::apoc_export::read_local(&file)?;
                let value = crate::apoc::xml_to_map(&text, simple)?;
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("value") {
                    nf[sl] = value.into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}

/// apoc.load.jdbc backend: maps the JDBC URL to a native driver. `jdbc:sqlite:<path>` (rusqlite)
/// and `jdbc:postgresql://...` (postgres); other dialects error with the supported list.
#[cfg(not(target_arch = "wasm32"))]
fn jdbc_query(url: &str, sql: &str) -> Result<Vec<CypherValue>, CypherError> {
    let mut rows_out = Vec::new();
    if let Some(path) = url
        .strip_prefix("jdbc:sqlite:")
        .or_else(|| url.strip_prefix("sqlite:"))
    {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| CypherError::runtime(format!("apoc.load.jdbc: {e}")))?;
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| CypherError::runtime(format!("apoc.load.jdbc: {e}")))?;
        let names: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut rows = stmt
            .query([])
            .map_err(|e| CypherError::runtime(format!("apoc.load.jdbc: {e}")))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| CypherError::runtime(format!("apoc.load.jdbc: {e}")))?
        {
            let mut m = std::collections::BTreeMap::new();
            for (i, name) in names.iter().enumerate() {
                let v = match row.get_ref(i) {
                    Ok(rusqlite::types::ValueRef::Null) => CypherValue::Null,
                    Ok(rusqlite::types::ValueRef::Integer(n)) => CypherValue::Integer(n),
                    Ok(rusqlite::types::ValueRef::Real(f)) => CypherValue::Float(f),
                    Ok(rusqlite::types::ValueRef::Text(t)) => {
                        CypherValue::String(String::from_utf8_lossy(t).to_string())
                    }
                    Ok(rusqlite::types::ValueRef::Blob(b)) => CypherValue::Bytes(b.to_vec()),
                    Err(_) => CypherValue::Null,
                };
                m.insert(name.clone(), v);
            }
            rows_out.push(CypherValue::Map(m));
        }
        return Ok(rows_out);
    }
    if let Some(rest) = url.strip_prefix("jdbc:postgresql://") {
        let conn_str = format!("postgresql://{rest}");
        let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
            .map_err(|e| CypherError::runtime(format!("apoc.load.jdbc: {e}")))?;
        let rows = client
            .query(sql, &[])
            .map_err(|e| CypherError::runtime(format!("apoc.load.jdbc: {e}")))?;
        for row in rows {
            let mut m = std::collections::BTreeMap::new();
            for (i, col) in row.columns().iter().enumerate() {
                use postgres::types::Type;
                let v = match *col.type_() {
                    Type::INT2 => row
                        .try_get::<_, Option<i16>>(i)
                        .ok()
                        .flatten()
                        .map(|n| CypherValue::Integer(n as i64)),
                    Type::INT4 => row
                        .try_get::<_, Option<i32>>(i)
                        .ok()
                        .flatten()
                        .map(|n| CypherValue::Integer(n as i64)),
                    Type::INT8 => row
                        .try_get::<_, Option<i64>>(i)
                        .ok()
                        .flatten()
                        .map(CypherValue::Integer),
                    Type::FLOAT4 => row
                        .try_get::<_, Option<f32>>(i)
                        .ok()
                        .flatten()
                        .map(|f| CypherValue::Float(f as f64)),
                    Type::FLOAT8 => row
                        .try_get::<_, Option<f64>>(i)
                        .ok()
                        .flatten()
                        .map(CypherValue::Float),
                    Type::BOOL => row
                        .try_get::<_, Option<bool>>(i)
                        .ok()
                        .flatten()
                        .map(CypherValue::Bool),
                    Type::BYTEA => row
                        .try_get::<_, Option<Vec<u8>>>(i)
                        .ok()
                        .flatten()
                        .map(CypherValue::Bytes),
                    _ => row
                        .try_get::<_, Option<String>>(i)
                        .ok()
                        .flatten()
                        .map(CypherValue::String),
                };
                m.insert(col.name().to_string(), v.unwrap_or(CypherValue::Null));
            }
            rows_out.push(CypherValue::Map(m));
        }
        return Ok(rows_out);
    }
    Err(CypherError::semantic(format!(
        "apoc.load.jdbc: unsupported JDBC url `{url}` - supported dialects: \
         jdbc:sqlite:<path>, jdbc:postgresql://<host>/<db>"
    )))
}

/// wasm32 has no sockets (postgres) and no C sqlite (rusqlite).
#[cfg(target_arch = "wasm32")]
fn jdbc_query(_url: &str, _sql: &str) -> Result<Vec<CypherValue>, CypherError> {
    Err(CypherError::runtime(
        "apoc.load.jdbc is not available in WASM builds",
    ))
}
