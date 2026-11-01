//! `/ws`: queries over a WebSocket.
//!
//! Client messages: `{"type": "query", "id", "query", "database"?, "parameters"?}` and
//! `{"type": "cancel", "id"}`, which stops that query while it runs. Replies: one `result` per row,
//! then `complete`, or an `error`. Each query is an auto-commit transaction; closing the socket
//! stops the running one.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use traverse_cypher::types::CypherValue;

use super::AppState;
use super::json::{cypher_value_to_json, json_to_cypher_params};
use crate::statement::{ClientAddress, HttpRequest};

// -- WS /ws ----------------------------------------------------------

pub(super) async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ClientAddress(client_address): ClientAddress,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_websocket(socket, state, client_address))
}

/// Runs a query as a statement of `request`, on the calling (blocking) thread.
fn execute_query_sync(
    request: &HttpRequest,
    state: &AppState,
    query: &str,
    db_name: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(traverse_cypher::executor::CypherResult, f64), String> {
    let entry = state
        .registry
        .get(db_name)
        .ok_or_else(|| format!("database '{}' not found", db_name))?;

    // read-only databases
    if entry.read_only && crate::is_write_query(query) {
        return Err(format!("database '{}' is read-only", db_name));
    }

    let start = Instant::now();
    let statement = request.statement(db_name, query, Some(std::sync::Arc::new(params.clone())));
    let _query = statement.ctx.enter();

    let result = if crate::is_write_query(query) {
        let mut tx = crate::statement::begin_write(&entry.store, &statement.ctx)
            .map_err(|e| e.to_string())?;
        match traverse_cypher::execute_with_params(&mut tx, query, params) {
            Ok(r) => match tx.commit() {
                Ok(_) => {
                    entry.mark_dirty();
                    Ok(r)
                }
                Err(e) => Err(traverse_cypher::error::CypherError::runtime(format!(
                    "commit failed: {e}"
                ))),
            },
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    } else {
        let mut tx = entry.store.begin_read().map_err(|e| e.to_string())?;
        traverse_cypher::execute_with_params(&mut tx, query, params)
    };

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    result.map(|r| (r, elapsed_ms)).map_err(|e| e.to_string())
}

/// A reply on the socket; `false` when the client is gone.
async fn send(socket: &mut WebSocket, msg: Value) -> bool {
    socket
        .send(Message::Text(msg.to_string().into()))
        .await
        .is_ok()
}

async fn handle_websocket(mut socket: WebSocket, state: AppState, client_address: String) {
    // messages that arrived while a query ran, handled in order afterwards
    let mut queued: VecDeque<String> = VecDeque::new();
    loop {
        let text = match queued.pop_front() {
            Some(text) => text,
            None => match socket.recv().await {
                Some(Ok(Message::Text(t))) => t.to_string(),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
        };

        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                if !send(
                    &mut socket,
                    json!({"type": "error", "message": format!("invalid JSON: {e}")}),
                )
                .await
                {
                    return;
                }
                continue;
            }
        };

        let msg_type = req.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let id = req
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match msg_type {
            "query" => {
                let query = match req.get("query").and_then(|v| v.as_str()) {
                    Some(q) => q.to_string(),
                    None => {
                        if !send(
                            &mut socket,
                            json!({"type": "error", "id": id, "message": "missing 'query'"}),
                        )
                        .await
                        {
                            return;
                        }
                        continue;
                    }
                };

                let db_name = req
                    .get("database")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| state.registry.default_name())
                    .to_string();

                let params = match req.get("parameters") {
                    Some(p) => json_to_cypher_params(p),
                    None => HashMap::new(),
                };

                // the query runs on a blocking thread while this loop keeps reading: a `cancel`
                // for it, or the socket closing, drops `work`, which stops the query
                let user = state.api_key.is_some().then(|| "api-user".to_string());
                let worker_state = state.clone();
                let work = crate::statement::run_request(
                    "ws",
                    client_address.clone(),
                    user,
                    move |request| {
                        execute_query_sync(request, &worker_state, &query, &db_name, &params)
                    },
                );
                tokio::pin!(work);
                let result = loop {
                    tokio::select! {
                        done = &mut work => break done.and_then(|r| r),
                        incoming = socket.recv() => match incoming {
                            Some(Ok(Message::Text(t))) => {
                                let cancels_this = serde_json::from_str::<Value>(&t)
                                    .ok()
                                    .is_some_and(|m| {
                                        m.get("type").and_then(Value::as_str) == Some("cancel")
                                            && m.get("id").and_then(Value::as_str)
                                                == Some(id.as_str())
                                    });
                                if cancels_this {
                                    break Err("the query was cancelled".to_string());
                                }
                                queued.push_back(t.to_string());
                            }
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                            Some(Ok(_)) => {}
                        },
                    }
                };

                match result {
                    Ok((cypher_result, elapsed_ms)) => {
                        let columns: Vec<Value> = cypher_result
                            .columns
                            .iter()
                            .map(|c| Value::String(c.clone()))
                            .collect();

                        // stream each row
                        let row_count = cypher_result.rows.len();
                        for row in &cypher_result.rows {
                            let row_json: Vec<Value> =
                                row.iter().map(cypher_value_to_json).collect();
                            let msg = json!({
                                "type": "result",
                                "id": id,
                                "columns": columns,
                                "row": row_json,
                            });
                            if !send(&mut socket, msg).await {
                                return; // client disconnected
                            }
                        }

                        // completion message
                        if !send(
                            &mut socket,
                            json!({
                                "type": "complete",
                                "id": id,
                                "stats": {
                                    "rows": row_count,
                                    "time_ms": elapsed_ms,
                                },
                            }),
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) => {
                        if !send(
                            &mut socket,
                            json!({"type": "error", "id": id, "message": e}),
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            // nothing of that id is running any more
            "cancel" => {}
            _ => {
                if !send(
                    &mut socket,
                    json!({"type": "error", "id": id, "message": format!("unknown message type: '{msg_type}'")}),
                )
                .await
                {
                    return;
                }
            }
        }
    }
}
