//! `POST /api/query`: Cypher and GQL, with GQL status codes on errors.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use traverse_cypher::types::CypherValue;

use super::AppState;
use super::json::{cypher_value_to_json, json_to_cypher_params};
use crate::statement::ClientAddress;

/// Why a query request didn't produce a result.
enum Failure {
    /// The statement failed (including stopped by its time limit or the client going away).
    Query(traverse_cypher::error::CypherError),
    /// The server couldn't run it (no reader slot, the worker panicked).
    Server(String),
}

// -- POST /api/query -------------------------------------------------

/// Runs a query in the requested dialect. openCypher goes straight to `traverse_cypher`; GQL
/// (`use_gql`, needs the `gql` feature) parses with `traverse_gql`, lowers to the same Cypher
/// representation and runs on the shared engine.
fn dispatch_query(
    tx: &mut dyn traverse_core::tx::view::GraphView,
    query: &str,
    params: &HashMap<String, traverse_cypher::types::CypherValue>,
    use_gql: bool,
) -> Result<
    (
        traverse_cypher::executor::CypherResult,
        std::time::Duration,
        std::time::Duration,
        std::time::Duration,
    ),
    traverse_cypher::error::CypherError,
> {
    if use_gql {
        #[cfg(feature = "gql")]
        {
            return run_gql(tx, query, params);
        }
        // query_handler rejects `use_gql=true` without the feature before this; the explicit panic
        // keeps a refactor from silently running a GQL query as Cypher
        #[cfg(not(feature = "gql"))]
        unreachable!("dispatch_query reached with use_gql=true without `gql` feature");
    }
    traverse_cypher::execute_with_params_timed(tx, query, params)
}

#[cfg(feature = "gql")]
fn run_gql(
    tx: &mut dyn traverse_core::tx::view::GraphView,
    query: &str,
    params: &HashMap<String, traverse_cypher::types::CypherValue>,
) -> Result<
    (
        traverse_cypher::executor::CypherResult,
        std::time::Duration,
        std::time::Duration,
        std::time::Duration,
    ),
    traverse_cypher::error::CypherError,
> {
    // parse -> lower -> path-mode -> execute lives in `traverse_gql::exec` so embedded bindings run
    // the same code; the first duration is GQL parse+lower (the engine's own parse is zero for
    // pre-lowered statements)
    traverse_gql::execute_gql_timed(tx, query, params)
}

/// Handles a parsed GQL catalog statement against the live registry:
/// `Some(Ok(()))` on success, `Some(Err(msg))` for a classified catalog
/// failure, `None` when it isn't a catalog op handled here (the caller falls
/// through to parse-lower-execute, which reports it as unsupported).
///
/// Handled:
/// * `CREATE GRAPH name [IF NOT EXISTS]` (no body, or `ANY`) ->
///   `registry.create_database(name)`
/// * `DROP GRAPH name [IF EXISTS]` -> `registry.drop_database(name)`
///
/// `USE name` isn't a catalog change: it routes the request (see `query_handler`).
///
/// Not handled: multi-segment paths (`/foo/mygraph`), bodies other than `ANY`
/// (inline type spec, `LIKE`, `AS COPY OF`), `CREATE/DROP SCHEMA` and
/// `CREATE/DROP GRAPH TYPE`.
#[cfg(feature = "gql")]
fn try_handle_gql_catalog(
    registry: &crate::registry::DatabaseRegistry,
    query: &str,
    current_db: &str,
) -> Option<Result<(), String>> {
    use traverse_gql::{GraphTypeBody, Statement};
    let program = traverse_gql::parse(query).ok()?;
    // single-statement catalog programs only; scripts like `CREATE GRAPH a NEXT CREATE GRAPH b`
    // would need transactional semantics
    let [stmt] = program.statements.as_slice() else {
        return None;
    };
    match stmt {
        Statement::CreateGraph {
            path,
            body,
            if_not_exists,
            ..
        } => {
            // single-segment relative names only
            if path.absolute || path.segments.len() != 1 {
                return None;
            }
            // bare form or `ANY` body only; LIKE, AS COPY OF and inline bodies need engine support
            if !matches!(body, None | Some(GraphTypeBody::Open { .. })) {
                return None;
            }
            let name = &path.segments[0];
            match registry.create_database(name) {
                Ok(_) => Some(Ok(())),
                Err(msg) => {
                    if *if_not_exists && msg.contains("already exists") {
                        // IF NOT EXISTS: conflict is a silent no-op
                        Some(Ok(()))
                    } else {
                        Some(Err(msg))
                    }
                }
            }
        }
        Statement::DropGraph {
            path, if_exists, ..
        } => {
            if path.absolute || path.segments.len() != 1 {
                return None;
            }
            let name = &path.segments[0];
            match registry.drop_database(name, current_db) {
                Ok(()) => Some(Ok(())),
                Err(msg) => {
                    if *if_exists && msg.contains("not found") {
                        Some(Ok(()))
                    } else {
                        Some(Err(msg))
                    }
                }
            }
        }
        _ => None,
    }
}

pub(super) async fn query_handler(
    State(state): State<AppState>,
    ClientAddress(client_address): ClientAddress,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let query = match body.get("query").and_then(|v| v.as_str()) {
        Some(q) => q.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "missing 'query' field" })),
            )
                .into_response();
        }
    };

    // dialect: the header wins over the body field, both default to openCypher; GQL needs the `gql`
    // feature
    let dialect = headers
        .get("x-query-dialect")
        .and_then(|h| h.to_str().ok())
        .or_else(|| body.get("dialect").and_then(|v| v.as_str()))
        .map(|s| s.to_ascii_lowercase());
    let use_gql = matches!(dialect.as_deref(), Some("gql"));
    if use_gql && !cfg!(feature = "gql") {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "GQL dialect requested but this server was built without the \
                          `gql` feature. Rebuild with `--features gql` or omit the \
                          `X-Query-Dialect` header to use openCypher.",
            })),
        )
            .into_response();
    }

    // GQL catalog dispatch: CREATE/DROP GRAPH and USE mutate the registry, not a graph, so they're
    // handled before the transaction flow; `None` falls through to parse-lower-execute
    #[cfg(feature = "gql")]
    if use_gql {
        let catalog_start = Instant::now();
        let current_db = body
            .get("database")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| state.registry.default_name())
            .to_string();
        if let Some(result) = try_handle_gql_catalog(&state.registry, &query, &current_db) {
            return match result {
                Ok(()) => axum::Json(json!({
                    "columns": Vec::<String>::new(),
                    "rows": Vec::<Vec<Value>>::new(),
                    "total_rows": 0,
                    "truncated": false,
                    "nodes": Vec::<Value>::new(),
                    "edges": Vec::<Value>::new(),
                    "entities_truncated": false,
                    "query_type": "Schema",
                    "stats": {
                        "nodes_created": 0,
                        "nodes_deleted": 0,
                        "relationships_created": 0,
                        "relationships_deleted": 0,
                        "properties_set": 0,
                        "labels_added": 0,
                        "labels_removed": 0,
                    },
                    "time_ms": catalog_start.elapsed().as_secs_f64() * 1000.0,
                    "parsing_time_ms": 0.0,
                    "planning_time_ms": 0.0,
                    "execution_time_ms": 0.0,
                }))
                .into_response(),
                Err(msg) => (
                    StatusCode::BAD_REQUEST,
                    axum::Json(query_error_envelope(
                        use_gql,
                        &traverse_cypher::error::CypherError::semantic(msg),
                        &query,
                    )),
                )
                    .into_response(),
            };
        }
    }

    // user-supplied parameters, shared with SHOW TRANSACTIONS
    let params = Arc::new(match body.get("parameters") {
        Some(p) => json_to_cypher_params(p),
        None => HashMap::new(),
    });

    // a GQL `USE <graph>` picks the database, over the request's `database` field
    #[cfg(feature = "gql")]
    let used = if use_gql {
        match traverse_gql::use_target(&query, &params) {
            Ok(target) => target.map(|name| state.registry.resolve_alias(&name)),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(query_error_envelope(use_gql, &e, &query)),
                )
                    .into_response();
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "gql"))]
    let used: Option<String> = None;
    let db_name = used.unwrap_or_else(|| {
        body.get("database")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| state.registry.default_name())
            .to_string()
    });
    let db_name = db_name.as_str();

    let entry = match state.registry.get(db_name) {
        Some(e) => e,
        None => {
            let error = traverse_cypher::error::CypherError::semantic(format!(
                "database '{db_name}' not found"
            ))
            .with_status(traverse_cypher::status::GqlStatus::GraphNotFound);
            return (
                StatusCode::NOT_FOUND,
                axum::Json(query_error_envelope(use_gql, &error, &query)),
            )
                .into_response();
        }
    };

    let start = Instant::now();

    // read-only databases
    if entry.read_only && crate::is_write_query(&query) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "error": format!("database '{}' is read-only", db_name) })),
        )
            .into_response();
    }

    // session user for ISO/IEC 39075 section 20.3 `SESSION_USER`: with a single configured API key the
    // caller is `api-user` (a stable placeholder) when auth is on and `anonymous` without it
    let (session_user, user) = if state.api_key.is_some() {
        ("api-user", Some("api-user".to_string()))
    } else {
        ("anonymous", None)
    };

    let is_write = crate::is_write_query(&query);

    // the statement runs on a blocking thread as a registered auto-commit transaction; the client
    // going away (this future dropped) stops it at its next check
    let database = db_name.to_string();
    let text = query.clone();
    let result = crate::statement::run_request("http", client_address, user, move |request| {
        let statement = request.statement(&database, &text, Some(Arc::clone(&params)));
        let _query = statement.ctx.enter();
        let _session_guard = traverse_cypher::session::set_session_user(session_user);
        if is_write {
            let mut tx = crate::statement::begin_write(&entry.store, &statement.ctx)
                .map_err(Failure::Query)?;
            match dispatch_query(&mut tx, &text, &params, use_gql) {
                Ok(r) => match tx.commit() {
                    Ok(_) => {
                        entry.mark_dirty();
                        Ok(r)
                    }
                    // a commit failure (redo-log/disk I/O) is a query error, not a process abort
                    Err(e) => Err(Failure::Query(
                        traverse_cypher::error::CypherError::runtime(format!("commit failed: {e}")),
                    )),
                },
                Err(e) => {
                    tx.abort();
                    Err(Failure::Query(e))
                }
            }
        } else {
            match entry.store.begin_read() {
                Ok(mut tx) => {
                    dispatch_query(&mut tx, &text, &params, use_gql).map_err(Failure::Query)
                }
                Err(e) => Err(Failure::Server(e.to_string())),
            }
        }
    })
    .await
    .unwrap_or_else(|e| Err(Failure::Server(e)));

    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;

    // slow query logging
    if let Some(threshold) = crate::config::slow_query_threshold()
        && elapsed > threshold
    {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis() as u64,
            "slow query ({}ms): {}",
            elapsed.as_millis(),
            crate::truncate_str(&query, 200),
        );
    }

    match result {
        Ok((cypher_result, parse_dur, plan_dur, exec_dur)) => {
            // row cap: serializing every row of a huge cartesian product would blow up wire size
            // and Studio's JS heap; all rows are still walked for distinct nodes/edges, so the
            // graph view stays complete when rows are truncated
            const MAX_HTTP_ROWS: usize = 10_000;
            // cap on distinct entities, against queries returning millions of unique nodes (`MATCH
            // (n)`)
            const MAX_ENTITIES: usize = 100_000;

            let total_rows = cypher_result.rows.len();
            let truncated = total_rows > MAX_HTTP_ROWS;

            // pass 1: dedup nodes and edges across the FULL result, capped on distinct entities
            // rather than rows, so small graphs hidden in large joins come through whole
            let (nodes, edges, entities_truncated) =
                collect_distinct_entities(&cypher_result.rows, MAX_ENTITIES);

            // pass 2: serialize the row sample for the table/JSON views
            let rows: Vec<Vec<Value>> = cypher_result
                .rows
                .iter()
                .take(MAX_HTTP_ROWS)
                .map(|row| row.iter().map(cypher_value_to_json).collect())
                .collect();

            axum::Json(json!({
                "columns": cypher_result.columns,
                "rows": rows,
                "total_rows": total_rows,
                "truncated": truncated,
                "nodes": nodes,
                "edges": edges,
                "entities_truncated": entities_truncated,
                "query_type": format!("{:?}", cypher_result.query_type),
                "stats": {
                    "nodes_created": cypher_result.stats.nodes_created,
                    "nodes_deleted": cypher_result.stats.nodes_deleted,
                    "relationships_created": cypher_result.stats.relationships_created,
                    "relationships_deleted": cypher_result.stats.relationships_deleted,
                    "properties_set": cypher_result.stats.properties_set,
                    "labels_added": cypher_result.stats.labels_added,
                    "labels_removed": cypher_result.stats.labels_removed,
                },
                "time_ms": elapsed_ms,
                "parsing_time_ms": parse_dur.as_secs_f64() * 1000.0,
                "planning_time_ms": plan_dur.as_secs_f64() * 1000.0,
                "execution_time_ms": exec_dur.as_secs_f64() * 1000.0,
            }))
            .into_response()
        }
        Err(Failure::Query(e)) => (
            StatusCode::BAD_REQUEST,
            axum::Json(query_error_envelope(use_gql, &e, &query)),
        )
            .into_response(),
        Err(Failure::Server(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// HTTP error JSON for a failed parse / lower / execute. The flat `error: <message>` and the
/// error's GQLSTATUS (`gql_status`) are always there; GQL-dialect requests also get a
/// `gql_status_object` (ISO/IEC 39075 section 4.9.2 structured status).
fn query_error_envelope(
    use_gql: bool,
    err: &traverse_cypher::error::CypherError,
    statement_text: &str,
) -> Value {
    let message = err.to_string();
    let gql = err.status();
    if !use_gql {
        return json!({ "error": message, "gql_status": gql.code() });
    }
    let mut status = json!({
        "gql_status": gql.code(),
        "condition": gql.description(),
        "statement_type": "query",
        "message": message,
        "statement_text": statement_text,
        "nested_status_objects": Vec::<Value>::new(),
    });
    if let traverse_cypher::error::CypherError::SyntaxError { position, .. } = err {
        let (line, column) = line_column_for(statement_text, *position);
        status.as_object_mut().unwrap().insert(
            "position".to_string(),
            json!({ "offset": position, "line": line, "column": column }),
        );
    }
    json!({
        "error": message,
        "gql_status": gql.code(),
        "gql_status_object": status,
    })
}

/// 1-based line/column of a byte offset in `s`, for source positions in GQL-status objects.
fn line_column_for(s: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in s.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

// -- helpers ---------------------------------------------------------

/// Collects distinct nodes and edges (by id) from every cell in `rows` for the graph view, stopping
/// once `max_entities` of either kind are seen; the returned `truncated` flag says the result was
/// wider. `Path` cells are unpacked into nodes/edges; `NodeRef` / `EdgeRef` cells are skipped (no
/// labels/properties, and the renderer needs the hydrated form).
fn collect_distinct_entities(
    rows: &[Vec<CypherValue>],
    max_entities: usize,
) -> (Vec<Value>, Vec<Value>, bool) {
    use std::collections::HashSet;
    let mut node_ids: HashSet<u64> = HashSet::new();
    let mut edge_ids: HashSet<u64> = HashSet::new();
    let mut nodes: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut truncated = false;

    fn visit(
        cv: &CypherValue,
        node_ids: &mut HashSet<u64>,
        edge_ids: &mut HashSet<u64>,
        nodes: &mut Vec<Value>,
        edges: &mut Vec<Value>,
        max_entities: usize,
        truncated: &mut bool,
    ) {
        if *truncated {
            return;
        }
        match cv {
            CypherValue::Node { id, .. } => {
                if node_ids.insert(id.0) {
                    if nodes.len() >= max_entities {
                        *truncated = true;
                        node_ids.remove(&id.0);
                        return;
                    }
                    nodes.push(cypher_value_to_json(cv));
                }
            }
            CypherValue::Edge { id, .. } => {
                if edge_ids.insert(id.0) {
                    if edges.len() >= max_entities {
                        *truncated = true;
                        edge_ids.remove(&id.0);
                        return;
                    }
                    edges.push(cypher_value_to_json(cv));
                }
            }
            CypherValue::Path {
                nodes: pnodes,
                edges: pedges,
            } => {
                for n in pnodes {
                    visit(n, node_ids, edge_ids, nodes, edges, max_entities, truncated);
                    if *truncated {
                        return;
                    }
                }
                for e in pedges {
                    visit(e, node_ids, edge_ids, nodes, edges, max_entities, truncated);
                    if *truncated {
                        return;
                    }
                }
            }
            CypherValue::List(items) => {
                for it in items {
                    visit(
                        it,
                        node_ids,
                        edge_ids,
                        nodes,
                        edges,
                        max_entities,
                        truncated,
                    );
                    if *truncated {
                        return;
                    }
                }
            }
            CypherValue::Map(m) => {
                for v in m.values() {
                    visit(v, node_ids, edge_ids, nodes, edges, max_entities, truncated);
                    if *truncated {
                        return;
                    }
                }
            }
            _ => {}
        }
    }

    for row in rows {
        for cell in row {
            visit(
                cell,
                &mut node_ids,
                &mut edge_ids,
                &mut nodes,
                &mut edges,
                max_entities,
                &mut truncated,
            );
            if truncated {
                return (nodes, edges, true);
            }
        }
    }
    (nodes, edges, false)
}
