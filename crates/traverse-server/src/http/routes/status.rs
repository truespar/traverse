//! Status and introspection: health, readiness, schema, metrics, the algorithm list and the API
//! docs.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use traverse_core::graph::constraint::ConstraintType;
use traverse_core::tx::view::GraphView;

use super::AppState;
use super::json::cypher_value_to_json;

// -- GET /api/health -------------------------------------------------

pub(super) async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_secs = state.start_time.elapsed().as_secs();

    // Linux: RSS from /proc/self/statm; on Windows the working set is unreliable (aggressively
    // trimmed), so sum estimated_memory per database
    let mut memory_bytes = if cfg!(target_os = "linux") {
        traverse_core::memory::allocated_bytes()
    } else {
        0
    };
    if memory_bytes == 0 {
        for (name, _, _, _) in state.registry.list_databases() {
            if let Some(entry) = state.registry.get(&name) {
                memory_bytes += entry.store.estimated_memory();
            }
        }
    }

    let databases: Vec<Value> = state
        .registry
        .list_databases()
        .into_iter()
        .map(|(name, is_default, _read_only, _aliases)| {
            let entry = state.registry.get(&name);
            let (nodes, edges, db_memory) = entry
                .as_ref()
                .map(|e| {
                    let mem = e.store.estimated_memory();
                    let read = e.store.begin_read();
                    match read {
                        Ok(tx) => (
                            <dyn GraphView>::node_count(&tx),
                            <dyn GraphView>::edge_count(&tx),
                            mem,
                        ),
                        Err(_) => (0, 0, mem),
                    }
                })
                .unwrap_or((0, 0, 0));
            json!({
                "name": name,
                "default": is_default,
                "nodes": nodes,
                "edges": edges,
                "memory_bytes": db_memory,
            })
        })
        .collect();

    // compile-time features, so Studio can hide UI the server can't back (e.g. the GQL dialect
    // dropdown without --features gql), and whether the Graph Assistant has a model (ai_provider)
    let features = json!({
        "ai": state.ai.is_some(),
        "gql": cfg!(feature = "gql"),
        "grpc": cfg!(feature = "grpc"),
        "tls": cfg!(feature = "tls"),
        "mcp": cfg!(feature = "mcp"),
        "ml": cfg!(feature = "ml"),
    });

    axum::Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime_secs,
        "memory_bytes": memory_bytes,
        "databases": databases,
        "allow_drop_database": state.allow_drop_database,
        "auth_required": state.api_key.is_some(),
        "features": features,
    }))
}

// -- GET /api/ready --------------------------------------------------

pub(super) async fn ready_handler() -> impl IntoResponse {
    axum::Json(json!({ "status": "ready" }))
}

// -- GET /api/schema -------------------------------------------------

pub(super) async fn schema_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let db_name = params
        .get("database")
        .map(|s| s.as_str())
        .unwrap_or_else(|| state.registry.default_name());
    let entry = match state.registry.get(db_name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({ "error": format!("database '{}' not found", db_name) })),
            )
                .into_response();
        }
    };

    // fast path: cached schema metadata, O(labels+types+keys) not O(nodes+edges)
    let (labels, rel_types, prop_keys_typed) = entry.store.schema_summary();
    let (labels_by_owner, edges_by_owner) = entry.store.schema_properties_by_owner();

    // indexes still need a read lock for the registry
    let read = match entry.store.begin_read() {
        Ok(tx) => tx,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let strings = <dyn GraphView>::strings(&read);
    let indexes: Vec<Value> = <dyn GraphView>::index_catalog(&read)
        .descriptors()
        .iter()
        .filter_map(|d| {
            let lbl = strings.try_resolve(d.label_id.0)?;
            let prop = strings.try_resolve(d.property_key.0)?;
            Some(json!({
                "name": d.name,
                "label": lbl,
                "property": prop,
                "entries": <dyn GraphView>::index_size(&read, &d.name).unwrap_or(0),
            }))
        })
        .collect();

    let constraints: Vec<Value> = <dyn GraphView>::constraint_registry(&read)
        .descriptors()
        .iter()
        .filter_map(|d| {
            let lbl = strings.try_resolve(d.label_id.0)?;
            let prop = strings.try_resolve(d.property_key.0)?;
            let ctype = match &d.constraint_type {
                ConstraintType::Unique => "unique".to_string(),
                ConstraintType::Exists => "exists".to_string(),
                ConstraintType::Type(k) => format!("type:{}", k.surface()),
                ConstraintType::NodeKey => "node_key".to_string(),
            };
            Some(json!({
                "name": d.name,
                "type": ctype,
                "label": lbl,
                "property": prop,
            }))
        })
        .collect();

    let property_keys: Vec<Value> = prop_keys_typed
        .iter()
        .map(|(name, typ)| json!({ "name": name, "type": typ }))
        .collect();

    let labels_detail: Vec<Value> = labels_by_owner
        .iter()
        .map(|(name, props)| {
            let ps: Vec<Value> = props
                .iter()
                .map(|(pname, ptype)| json!({ "name": pname, "type": ptype }))
                .collect();
            json!({ "name": name, "properties": ps })
        })
        .collect();

    let relationship_types_detail: Vec<Value> = edges_by_owner
        .iter()
        .map(|(name, props)| {
            let ps: Vec<Value> = props
                .iter()
                .map(|(pname, ptype)| json!({ "name": pname, "type": ptype }))
                .collect();
            json!({ "name": name, "properties": ps })
        })
        .collect();

    axum::Json(json!({
        "labels": labels,
        "relationship_types": rel_types,
        "property_keys": property_keys,
        "indexes": indexes,
        "constraints": constraints,
        "labels_detail": labels_detail,
        "relationship_types_detail": relationship_types_detail,
    }))
    .into_response()
}

// -- GET /metrics ----------------------------------------------------

pub(super) async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut out = String::new();

    // memory
    let mem = traverse_core::memory::allocated_bytes();
    out.push_str(&format!(
        "# HELP traverse_memory_bytes Current allocated memory in bytes.\n\
         # TYPE traverse_memory_bytes gauge\n\
         traverse_memory_bytes {mem}\n"
    ));

    // uptime
    let uptime = state.start_time.elapsed().as_secs();
    out.push_str(&format!(
        "# HELP traverse_uptime_seconds Server uptime in seconds.\n\
         # TYPE traverse_uptime_seconds gauge\n\
         traverse_uptime_seconds {uptime}\n"
    ));

    // per-database node/edge counts
    out.push_str(
        "# HELP traverse_nodes Total number of nodes.\n\
         # TYPE traverse_nodes gauge\n",
    );
    for (name, _, _, _) in state.registry.list_databases() {
        if let Some(entry) = state.registry.get(&name)
            && let Ok(tx) = entry.store.begin_read()
        {
            let n = <dyn GraphView>::node_count(&tx);
            out.push_str(&format!("traverse_nodes{{db=\"{name}\"}} {n}\n"));
        }
    }

    out.push_str(
        "# HELP traverse_edges Total number of edges.\n\
         # TYPE traverse_edges gauge\n",
    );
    for (name, _, _, _) in state.registry.list_databases() {
        if let Some(entry) = state.registry.get(&name)
            && let Ok(tx) = entry.store.begin_read()
        {
            let e = <dyn GraphView>::edge_count(&tx);
            out.push_str(&format!("traverse_edges{{db=\"{name}\"}} {e}\n"));
        }
    }

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
}

// -- GET /api/algorithms ---------------------------------------------
// Static catalog of the registered graph algorithms; Studio's GDS drawer fetches it once to build
// the picker and config form. Derived entirely from each algorithm's `metadata()`, so new
// algorithms need no change here.

pub(super) async fn algorithms_list_handler() -> impl IntoResponse {
    // the algorithm registry is thread-local (like the TCK procedure registry); a Tokio worker that
    // never ran a `traverse.*` CALL hasn't populated it, so seed it here (first touch per thread, a
    // single Cell load afterwards)
    traverse_cypher::algorithms::ensure_registered();

    let mut entries = traverse_cypher::algorithms::registry::list();
    entries.sort_by_key(|e| (e.metadata.category.as_str(), e.metadata.name));

    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let m = &entry.metadata;
            let modes: Vec<&'static str> = if m.output_kind.writable() {
                vec!["stream", "stats", "write"]
            } else {
                vec!["stream", "stats"]
            };
            let config_schema: Vec<Value> = m
                .params
                .iter()
                .map(|p| {
                    json!({
                        "key": p.key,
                        "type": p.ty.as_str(),
                        "default": cypher_value_to_json(&p.default),
                        "description": p.description,
                        "advanced": p.advanced,
                        "required": p.required,
                    })
                })
                .collect();
            json!({
                "name": m.name,
                "displayName": m.display_name,
                "category": m.category.as_str(),
                "outputKind": m.output_kind.as_str(),
                "stability": m.stability.as_str(),
                "description": m.description,
                "writable": m.output_kind.writable(),
                "modes": modes,
                "configSchema": config_schema,
                "outputColumns": m.output_columns,
                "statsColumns": m.stats_columns,
                "writeColumns": m.write_columns,
            })
        })
        .collect();

    axum::Json(json!({
        "algorithms": items,
    }))
    .into_response()
}

// -- GET /api/docs --------------------------------------------------

pub(super) async fn scalar_docs_handler() -> impl IntoResponse {
    let html = r#"<!doctype html>
<html>
<head>
  <title>Traverse API Reference</title>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
</head>
<body>
  <script id="api-reference" data-url="/api/openapi.json"></script>
  <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#;

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
}

// -- GET /api/openapi.json ------------------------------------------

pub(super) async fn openapi_spec_handler() -> impl IntoResponse {
    // version injected at compile time; the rest is a static spec
    static SPEC_TEMPLATE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/http/openapi.json"
    ));

    let spec = SPEC_TEMPLATE.replace("${VERSION}", env!("CARGO_PKG_VERSION"));

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        spec,
    )
}
