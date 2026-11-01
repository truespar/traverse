//! MCP (Model Context Protocol) endpoint: the Streamable HTTP transport
//! (JSON-RPC 2.0 over POST), exposing graph tools to AI models:
//!
//! - query: run a Cypher query
//! - schema: labels, types, properties, indexes
//! - databases: list databases
//! - search_nodes: nodes by label + property filter
//! - shortest_path: shortest path between two nodes
//! - neighbors: a node's neighbors

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use traverse_core::tx::view::GraphView;

use super::routes::AppState;

// -- JSON-RPC 2.0 types ----------------------------------------------

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// JSON-RPC error codes
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const _INTERNAL_ERROR: i64 = -32603;

// -- MCP protocol constants ------------------------------------------

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const SERVER_NAME: &str = "traverse";

// -- Tool definitions ------------------------------------------------

fn tool_definitions() -> Value {
    json!([
        {
            "name": "query",
            "description": "Run a Cypher query against the graph database. Returns columns and rows. Supports parameterized queries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cypher": {
                        "type": "string",
                        "description": "The Cypher query to execute"
                    },
                    "database": {
                        "type": "string",
                        "description": "Database name (optional, uses default if omitted)"
                    },
                    "parameters": {
                        "type": "object",
                        "description": "Query parameters (optional). Keys are parameter names, values are the parameter values."
                    }
                },
                "required": ["cypher"]
            }
        },
        {
            "name": "schema",
            "description": "Get the database schema including node labels, relationship types, property keys (with types), indexes, and constraints.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "database": {
                        "type": "string",
                        "description": "Database name (optional, uses default if omitted)"
                    }
                }
            }
        },
        {
            "name": "databases",
            "description": "List all available databases with their names, node/edge counts, and status.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "search_nodes",
            "description": "Find nodes by label and optional property filter. Returns matching nodes with their properties. Limited to 100 results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Node label to search for"
                    },
                    "property": {
                        "type": "string",
                        "description": "Property name to filter on (optional)"
                    },
                    "value": {
                        "description": "Property value to match (optional, requires property)"
                    },
                    "database": {
                        "type": "string",
                        "description": "Database name (optional, uses default if omitted)"
                    }
                },
                "required": ["label"]
            }
        },
        {
            "name": "shortest_path",
            "description": "Find the shortest path between two nodes by their internal IDs. Returns the path as a sequence of nodes and edges.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_id": {
                        "type": "integer",
                        "description": "Internal ID of the start node"
                    },
                    "to_id": {
                        "type": "integer",
                        "description": "Internal ID of the end node"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Maximum path length (default: 10)"
                    },
                    "database": {
                        "type": "string",
                        "description": "Database name (optional, uses default if omitted)"
                    }
                },
                "required": ["from_id", "to_id"]
            }
        },
        {
            "name": "neighbors",
            "description": "Get the neighbors of a node by its internal ID. Returns connected nodes and the edges connecting them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "integer",
                        "description": "Internal ID of the node"
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["outgoing", "incoming", "both"],
                        "description": "Edge direction filter (default: both)"
                    },
                    "edge_type": {
                        "type": "string",
                        "description": "Filter by edge type (optional)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of neighbors to return (default: 50)"
                    },
                    "database": {
                        "type": "string",
                        "description": "Database name (optional, uses default if omitted)"
                    }
                },
                "required": ["node_id"]
            }
        }
    ])
}

// -- POST /api/mcp handler -------------------------------------------

pub async fn mcp_post_handler(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let request: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = JsonRpcResponse::error(None, PARSE_ERROR, format!("Parse error: {e}"));
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_string(&resp).unwrap(),
            )
                .into_response();
        }
    };

    if request.jsonrpc != "2.0" {
        let resp = JsonRpcResponse::error(request.id, INVALID_REQUEST, "Expected jsonrpc: \"2.0\"");
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&resp).unwrap(),
        )
            .into_response();
    }

    // notifications (method "notifications/...") and responses (no method) get 202 Accepted, per the
    // MCP Streamable HTTP spec
    let is_notification = request.method.starts_with("notifications/");
    if is_notification {
        return StatusCode::ACCEPTED.into_response();
    }

    let resp = match request.method.as_str() {
        "initialize" => handle_initialize(request.id, &request.params),
        "tools/list" => handle_tools_list(request.id),
        "tools/call" => handle_tools_call(request.id, &request.params, &state).await,
        "ping" => JsonRpcResponse::success(request.id, json!({})),
        _ => JsonRpcResponse::error(
            request.id,
            METHOD_NOT_FOUND,
            format!("Method not found: {}", request.method),
        ),
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&resp).unwrap(),
    )
        .into_response()
}

// -- GET /api/mcp (SSE stream): not supported, 405 ------------------

pub async fn mcp_get_handler() -> impl IntoResponse {
    StatusCode::METHOD_NOT_ALLOWED
}

// -- DELETE /api/mcp (session termination): not supported -----------

pub async fn mcp_delete_handler() -> impl IntoResponse {
    StatusCode::METHOD_NOT_ALLOWED
}

// -- MCP method handlers ---------------------------------------------

fn handle_initialize(id: Option<Value>, _params: &Value) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

fn handle_tools_list(id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        json!({
            "tools": tool_definitions()
        }),
    )
}

async fn handle_tools_call(id: Option<Value>, params: &Value, state: &AppState) -> JsonRpcResponse {
    let tool_name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return JsonRpcResponse::error(
                id,
                INVALID_PARAMS,
                "Missing 'name' in tools/call params",
            );
        }
    };

    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "query" => tool_query(&arguments, state).await,
        "schema" => tool_schema(&arguments, state),
        "databases" => tool_databases(state),
        "search_nodes" => tool_search_nodes(&arguments, state).await,
        "shortest_path" => tool_shortest_path(&arguments, state).await,
        "neighbors" => tool_neighbors(&arguments, state).await,
        _ => Err(format!("Unknown tool: {tool_name}")),
    };

    match result {
        Ok(content) => JsonRpcResponse::success(
            id,
            json!({
                "content": [{ "type": "text", "text": content }]
            }),
        ),
        Err(msg) => JsonRpcResponse::success(
            id,
            json!({
                "content": [{ "type": "text", "text": msg }],
                "isError": true
            }),
        ),
    }
}

// -- Tool implementations --------------------------------------------

fn resolve_db(
    args: &Value,
    state: &AppState,
) -> Result<(String, std::sync::Arc<crate::registry::DatabaseEntry>), String> {
    let db_name = args
        .get("database")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| state.registry.default_name());
    let entry = state
        .registry
        .get(db_name)
        .ok_or_else(|| format!("Database '{}' not found", db_name))?;
    Ok((db_name.to_string(), entry))
}

async fn tool_query(args: &Value, state: &AppState) -> Result<String, String> {
    let cypher = args
        .get("cypher")
        .and_then(|v| v.as_str())
        .ok_or("Missing required 'cypher' argument")?;

    let (db_name, entry) = resolve_db(args, state)?;

    let params = match args.get("parameters") {
        Some(p) => super::routes::json_to_cypher_params(p),
        None => HashMap::new(),
    };

    // read-only databases
    if entry.read_only && crate::is_write_query(cypher) {
        return Err(format!("Database '{}' is read-only", db_name));
    }

    let is_write = crate::is_write_query(cypher);

    // a registered auto-commit transaction on a blocking thread, stopped by the server's time
    // limit or the client going away
    let user = state.api_key.is_some().then(|| "api-user".to_string());
    let text = cypher.to_string();
    let result = crate::statement::run_request("mcp", String::new(), user, move |request| {
        let statement =
            request.statement(&db_name, &text, Some(std::sync::Arc::new(params.clone())));
        let _query = statement.ctx.enter();
        if is_write {
            let mut tx = crate::statement::begin_write(&entry.store, &statement.ctx)
                .map_err(|e| e.to_string())?;
            match traverse_cypher::execute_with_params_timed(&mut tx, &text, &params) {
                Ok(res) => {
                    tx.commit().map_err(|e| format!("commit failed: {e}"))?;
                    entry.mark_dirty();
                    Ok(res)
                }
                Err(e) => {
                    tx.abort();
                    Err(e.to_string())
                }
            }
        } else {
            let mut tx = entry.store.begin_read().map_err(|e| e.to_string())?;
            traverse_cypher::execute_with_params_timed(&mut tx, &text, &params)
                .map_err(|e| e.to_string())
        }
    })
    .await
    .and_then(|r| r);

    match result {
        Ok((cypher_result, _parse, _plan, exec_dur)) => {
            const MAX_ROWS: usize = 500;
            let total = cypher_result.rows.len();
            let truncated = total > MAX_ROWS;

            let rows: Vec<Vec<Value>> = cypher_result
                .rows
                .iter()
                .take(MAX_ROWS)
                .map(|row| {
                    row.iter()
                        .map(super::routes::cypher_value_to_json)
                        .collect()
                })
                .collect();

            let resp = json!({
                "columns": cypher_result.columns,
                "rows": rows,
                "total_rows": total,
                "truncated": truncated,
                "execution_time_ms": exec_dur.as_secs_f64() * 1000.0,
            });
            Ok(serde_json::to_string_pretty(&resp).unwrap())
        }
        Err(e) => Err(format!("Query error: {e}")),
    }
}

fn tool_schema(args: &Value, state: &AppState) -> Result<String, String> {
    let (_db_name, entry) = resolve_db(args, state)?;

    let (labels, rel_types, prop_keys_typed) = entry.store.schema_summary();
    let (labels_by_owner, edges_by_owner) = entry.store.schema_properties_by_owner();

    let read = entry.store.begin_read().map_err(|e| e.to_string())?;
    let strings = <dyn GraphView>::strings(&read);

    let indexes: Vec<Value> = <dyn GraphView>::index_catalog(&read)
        .descriptors()
        .iter()
        .filter_map(|d| {
            let lbl = strings.try_resolve(d.label_id.0)?;
            let prop = strings.try_resolve(d.property_key.0)?;
            let entries = <dyn GraphView>::index_size(&read, &d.name).unwrap_or(0);
            Some(json!({ "label": lbl, "property": prop, "entries": entries }))
        })
        .collect();

    let constraints: Vec<Value> = <dyn GraphView>::constraint_registry(&read)
        .descriptors()
        .iter()
        .filter_map(|d| {
            let lbl = strings.try_resolve(d.label_id.0)?;
            let prop = strings.try_resolve(d.property_key.0)?;
            let ctype = match &d.constraint_type {
                traverse_core::graph::constraint::ConstraintType::Unique => "unique".to_string(),
                traverse_core::graph::constraint::ConstraintType::Exists => "exists".to_string(),
                traverse_core::graph::constraint::ConstraintType::Type(k) => {
                    format!("type:{}", k.surface())
                }
                traverse_core::graph::constraint::ConstraintType::NodeKey => "node_key".to_string(),
            };
            Some(json!({ "type": ctype, "label": lbl, "property": prop }))
        })
        .collect();

    let labels_detail: Vec<Value> = labels_by_owner
        .iter()
        .map(|(name, props)| {
            let ps: Vec<String> = props.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            json!({ "label": name, "properties": ps })
        })
        .collect();

    let edges_detail: Vec<Value> = edges_by_owner
        .iter()
        .map(|(name, props)| {
            let ps: Vec<String> = props.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            json!({ "type": name, "properties": ps })
        })
        .collect();

    let resp = json!({
        "labels": labels,
        "relationship_types": rel_types,
        "property_keys": prop_keys_typed.iter().map(|(n, t)| format!("{n}: {t}")).collect::<Vec<_>>(),
        "indexes": indexes,
        "constraints": constraints,
        "labels_detail": labels_detail,
        "relationship_types_detail": edges_detail,
    });
    Ok(serde_json::to_string_pretty(&resp).unwrap())
}

fn tool_databases(state: &AppState) -> Result<String, String> {
    let dbs: Vec<Value> = state
        .registry
        .list_databases()
        .iter()
        .map(|(name, is_default, read_only, aliases)| {
            let entry = state.registry.get(name);
            let (nodes, edges) = entry
                .as_ref()
                .map(|e| {
                    let read = e.store.begin_read().ok();
                    read.map(|r| (r.node_count(), r.edge_count()))
                        .unwrap_or((0, 0))
                })
                .unwrap_or((0, 0));
            json!({
                "name": name,
                "default": is_default,
                "read_only": read_only,
                "aliases": aliases,
                "node_count": nodes,
                "edge_count": edges,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&json!({ "databases": dbs })).unwrap())
}

async fn tool_search_nodes(args: &Value, state: &AppState) -> Result<String, String> {
    let label = args
        .get("label")
        .and_then(|v| v.as_str())
        .ok_or("Missing required 'label' argument")?;

    let property = args.get("property").and_then(|v| v.as_str());
    let value = args.get("value");

    let (_db_name, entry) = resolve_db(args, state)?;

    // Cypher for the search
    let cypher = match (property, value) {
        (Some(prop), Some(val)) => {
            let val_str = match val {
                Value::String(s) => format!("'{}'", s.replace('\'', "\\'")),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return Err("Unsupported value type".into()),
            };
            format!("MATCH (n:{label}) WHERE n.{prop} = {val_str} RETURN n LIMIT 100")
        }
        (Some(prop), None) => {
            format!("MATCH (n:{label}) WHERE n.{prop} IS NOT NULL RETURN n LIMIT 100")
        }
        _ => format!("MATCH (n:{label}) RETURN n LIMIT 100"),
    };

    let mut tx = entry.store.begin_read().map_err(|e| e.to_string())?;
    let result = traverse_cypher::execute_with_params_timed(&mut tx, &cypher, &HashMap::new());

    match result {
        Ok((cypher_result, _, _, _)) => {
            let nodes: Vec<Value> = cypher_result
                .rows
                .iter()
                .flat_map(|row| {
                    row.iter()
                        .map(super::routes::cypher_value_to_json)
                        .collect::<Vec<_>>()
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "count": nodes.len(),
                "nodes": nodes,
            }))
            .unwrap())
        }
        Err(e) => Err(format!("Search error: {e}")),
    }
}

async fn tool_shortest_path(args: &Value, state: &AppState) -> Result<String, String> {
    let from_id = args
        .get("from_id")
        .and_then(|v| v.as_i64())
        .ok_or("Missing required 'from_id' argument")?;
    let to_id = args
        .get("to_id")
        .and_then(|v| v.as_i64())
        .ok_or("Missing required 'to_id' argument")?;
    let max_depth = args.get("max_depth").and_then(|v| v.as_i64()).unwrap_or(10);

    let (_db_name, entry) = resolve_db(args, state)?;

    let cypher = format!(
        "MATCH p = shortestPath((a)-[*..{max_depth}]-(b)) WHERE id(a) = {from_id} AND id(b) = {to_id} RETURN p"
    );

    let mut tx = entry.store.begin_read().map_err(|e| e.to_string())?;
    let result = traverse_cypher::execute_with_params_timed(&mut tx, &cypher, &HashMap::new());

    match result {
        Ok((cypher_result, _, _, _)) => {
            if cypher_result.rows.is_empty() {
                Ok(json!({"path": null, "message": "No path found"}).to_string())
            } else {
                let path: Vec<Value> = cypher_result.rows[0]
                    .iter()
                    .map(super::routes::cypher_value_to_json)
                    .collect();
                Ok(serde_json::to_string_pretty(&json!({"path": path[0]})).unwrap())
            }
        }
        Err(e) => Err(format!("Path error: {e}")),
    }
}

async fn tool_neighbors(args: &Value, state: &AppState) -> Result<String, String> {
    let node_id = args
        .get("node_id")
        .and_then(|v| v.as_i64())
        .ok_or("Missing required 'node_id' argument")?;
    let direction = args
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("both");
    let edge_type = args.get("edge_type").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);

    let (_db_name, entry) = resolve_db(args, state)?;

    let rel_pattern = match edge_type {
        Some(t) => format!(":{t}"),
        None => String::new(),
    };

    let cypher = match direction {
        "outgoing" => format!(
            "MATCH (n)-[r{rel_pattern}]->(m) WHERE id(n) = {node_id} RETURN r, m LIMIT {limit}"
        ),
        "incoming" => format!(
            "MATCH (n)<-[r{rel_pattern}]-(m) WHERE id(n) = {node_id} RETURN r, m LIMIT {limit}"
        ),
        _ => format!(
            "MATCH (n)-[r{rel_pattern}]-(m) WHERE id(n) = {node_id} RETURN r, m LIMIT {limit}"
        ),
    };

    let mut tx = entry.store.begin_read().map_err(|e| e.to_string())?;
    let result = traverse_cypher::execute_with_params_timed(&mut tx, &cypher, &HashMap::new());

    match result {
        Ok((cypher_result, _, _, _)) => {
            let neighbors: Vec<Value> = cypher_result
                .rows
                .iter()
                .map(|row| {
                    json!({
                        "edge": super::routes::cypher_value_to_json(&row[0]),
                        "node": super::routes::cypher_value_to_json(&row[1]),
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "count": neighbors.len(),
                "neighbors": neighbors,
            }))
            .unwrap())
        }
        Err(e) => Err(format!("Neighbors error: {e}")),
    }
}
