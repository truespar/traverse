//! WebAssembly bindings for the Traverse graph engine.
//!
//! A `TraverseDb` JS class embedding the same Cypher executor and GDS procedures
//! as the native server. Meant to run inside a Web Worker (see
//! `@truespar/traverse-wasm`'s `worker.js`), with `wasm-bindgen-rayon` for
//! multi-threading over `SharedArrayBuffer`.
//!
//! # Build
//!
//! ```sh
//! # Single-threaded build (works on stable):
//! wasm-pack build crates/traverse-wasm --target web --release
//!
//! # Multi-threaded build (requires nightly Rust):
//! wasm-pack build crates/traverse-wasm --target web --release \
//!   --features threads -- -Z build-std=panic_abort,std
//! ```
//!
//! The threaded variant needs the page served with
//! `Cross-Origin-Embedder-Policy: require-corp` and
//! `Cross-Origin-Opener-Policy: same-origin`; see
//! `docs/reference/wasm-deployment.md` for deployment.

// non-wasm targets compile to an empty crate (the workspace builds this crate on every target)

#[cfg(test)]
mod codec_test;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::sync::{Arc, Mutex};

    use serde::Serialize;
    use traverse_core::graph::Graph;
    use traverse_core::graph::property::PropertyValue;
    use traverse_core::storage::page_manager::PageManager;
    use traverse_core::tx::MvccStore;
    use traverse_cypher as cypher;
    use wasm_bindgen::prelude::*;

    /// Same cap as the server's `/api/query` on hydrated entities: past this many distinct nodes or
    /// edges, stop extracting them from the row sample and set `entities_truncated: true`. Studio
    /// shows the full table either way; the cap only bounds the graph view.
    const MAX_ENTITIES: usize = 1024;
    // browser WASM can't discover physical RAM, so install an explicit query budget instead of the
    // native "unknown = unlimited" default; it covers query intermediates, not the database or the
    // JS heap
    const QUERY_MEMORY_BYTES: usize = 128 * 1024 * 1024;
    const MAX_RESULT_ROWS: usize = 10_000;
    const RESULT_VALUE_BYTES: usize = 8 * 1024 * 1024;

    /// Re-export `wasm-bindgen-rayon`'s thread-pool initializer when
    /// the `threads` feature is enabled. JS callers do:
    /// ```js
    /// import init, { initThreadPool, TraverseDb } from './traverse_wasm.js'
    /// await init()
    /// await initThreadPool(navigator.hardwareConcurrency)
    /// const db = new TraverseDb()
    /// ```
    #[cfg(feature = "threads")]
    pub use wasm_bindgen_rayon::init_thread_pool;

    /// Install a panic hook on first load that prints panics to the
    /// browser console with a readable stack trace.
    fn install_panic_hook() {
        use std::sync::Once;
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            console_error_panic_hook::set_once();
        });
    }

    /// JS-visible result of `query()`. Mirrors the HTTP `QueryResponse`
    /// shape so the Studio stores treat WASM results identically to
    /// HTTP results - including the distinct-entity arrays the graph
    /// view consumes.
    #[derive(serde::Serialize)]
    struct QueryResponse {
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
        total_rows: usize,
        truncated: bool,
        /// Distinct nodes hydrated from the row sample. The graph view
        /// uses these to render; the Cypher table view uses `rows`.
        nodes: Vec<serde_json::Value>,
        /// Distinct edges hydrated from the row sample.
        edges: Vec<serde_json::Value>,
        /// Nodes BOUND by the query's patterns but not returned - the
        /// provenance behind a projection like `RETURN t.amount`, hydrated
        /// to the same shape as `nodes`. Lets the graph view highlight what
        /// the query touched without guessing from the rows.
        bound_nodes: Vec<serde_json::Value>,
        /// Edges bound but not returned; same contract as `bound_nodes`.
        bound_edges: Vec<serde_json::Value>,
        /// True if `nodes` or `edges` was capped at `MAX_ENTITIES`.
        entities_truncated: bool,
        query_type: String,
        /// Executor mutation counters, as in HTTP `/api/query` `stats`; Studio's result panel reads
        /// `nodes_created`, `relationships_created`, ... directly.
        stats: QueryStatsJson,
        /// Serialized logical plan for `EXPLAIN` / `PROFILE`, `null` otherwise; Studio's JSON tab
        /// shows the operator tree.
        #[serde(skip_serializing_if = "Option::is_none")]
        plan: Option<serde_json::Value>,
        time_ms: f64,
        /// Sub-timings for the result panel's `parse / plan / execute` chips; HTTP `/api/query`
        /// field names.
        parsing_time_ms: f64,
        planning_time_ms: f64,
        execution_time_ms: f64,
    }

    /// JS-visible algorithm metadata. Matches `GET /api/algorithms`.
    #[derive(serde::Serialize)]
    struct AlgorithmsResponse {
        algorithms: Vec<AlgorithmEntry>,
    }

    #[derive(serde::Serialize)]
    struct AlgorithmEntry {
        name: &'static str,
        #[serde(rename = "displayName")]
        display_name: &'static str,
        category: &'static str,
        #[serde(rename = "outputKind")]
        output_kind: &'static str,
        stability: &'static str,
        description: &'static str,
        writable: bool,
        modes: Vec<&'static str>,
        #[serde(rename = "configSchema")]
        config_schema: Vec<ConfigParamJson>,
        #[serde(rename = "outputColumns")]
        output_columns: Vec<&'static str>,
        #[serde(rename = "statsColumns")]
        stats_columns: Vec<&'static str>,
        #[serde(rename = "writeColumns")]
        write_columns: Vec<&'static str>,
    }

    #[derive(serde::Serialize)]
    struct ConfigParamJson {
        key: &'static str,
        #[serde(rename = "type")]
        ty: &'static str,
        default: serde_json::Value,
        description: &'static str,
        advanced: bool,
        required: bool,
    }

    /// In-process graph database: single graph, single tab. Methods are synchronous in Rust; the JS
    /// wrapper makes them `async`, so the page can move into a Web Worker without API changes.
    #[wasm_bindgen]
    pub struct TraverseDb {
        /// The database: each statement runs in a transaction of its own, so a failed one leaves
        /// nothing behind. Swapped whole by `loadTvdb` / `clear`.
        store: Mutex<Arc<MvccStore>>,
    }

    impl Default for TraverseDb {
        fn default() -> Self {
            Self::new()
        }
    }

    #[wasm_bindgen]
    impl TraverseDb {
        /// Empty graph; fill it with `CREATE` queries or `loadTvdb()`.
        #[wasm_bindgen(constructor)]
        pub fn new() -> Self {
            install_panic_hook();
            Self {
                store: Mutex::new(Arc::new(MvccStore::new(Graph::new()))),
            }
        }

        /// Run a Cypher query (read or write). `params_json` is an
        /// optional JSON-stringified parameter map; pass `null` if no
        /// parameters. `timeout_ms` (optional, > 0) limits the statement,
        /// on the pool's threads too; defaults to 10 seconds, capped at 30
        /// seconds. The statement is a transaction of its own: a write that
        /// fails or runs out of time leaves nothing behind.
        ///
        /// Returns a JS object matching the HTTP `QueryResponse` shape
        /// - columns, rows, hydrated nodes/edges, mutation stats,
        /// query type classification, elapsed time.
        ///
        /// Examples (from JS):
        /// ```js
        /// await db.query("MATCH (n) RETURN n", null, 30000)
        /// await db.query(
        ///   "CALL traverse.pageRank.stream({maxIterations: 20}) " +
        ///   "YIELD nodeId, score RETURN nodeId, score",
        ///   null,
        ///   null
        /// )
        /// ```
        pub fn query(
            &self,
            cypher_text: &str,
            params_json: JsValue,
            timeout_ms: Option<u32>,
            dialect: Option<String>,
        ) -> Result<JsValue, JsValue> {
            let started = now_ms();

            // Parse params if provided. Empty / null -> no params.
            let params = parse_params(params_json)?;

            // the statement's limits: its time limit (checked at iteration boundaries, on the
            // pool's threads too) and the query memory budget
            let query = cypher::query::QueryContext::new(cypher::query::QueryOptions {
                timeout: Some(std::time::Duration::from_millis(
                    timeout_ms
                        .filter(|ms| *ms > 0)
                        .unwrap_or(10_000)
                        .min(30_000) as u64,
                )),
                memory_limit: Some(QUERY_MEMORY_BYTES),
                ..Default::default()
            });
            let scope = query.enter();
            let store = self.store()?;
            // the statement is a transaction of its own: committed when it succeeds, rolled back
            // when it fails, so a failed write leaves nothing behind; CALL { ... } IN TRANSACTIONS
            // commits its batches in it
            let mut tx = store.begin_write();
            tx.allow_batch_commits();
            // timed entry points, so the result panel shows the parse / plan / execute breakdown
            // the HTTP server emits
            let executed = if dialect.as_deref() == Some("gql") {
                // ISO/IEC 39075: parse + lower via traverse-gql, executed by the same engine, as on
                // the server's `X-Query-Dialect: gql` route
                traverse_gql::execute_gql_timed(&mut tx, cypher_text, &params)
            } else {
                cypher::execute_with_params_timed(&mut tx, cypher_text, &params)
            };
            let result_outcome = match executed {
                Ok(done) => tx
                    .commit()
                    .map(|_| done)
                    .map_err(|e| JsValue::from_str(&format!("commit failed: {e}"))),
                Err(e) => {
                    tx.abort();
                    Err(JsValue::from_str(&format!("{e}")))
                }
            };
            drop(scope);
            let (result, parse_dur, plan_dur, exec_dur) = result_outcome?;

            // Budget BEFORE converting/copying into serde JSON and JS objects.
            // The result table reports its full row count and truncation; no
            // query text rewriting or LIMIT injection changes write semantics.
            let mut rows_json: Vec<Vec<serde_json::Value>> = Vec::new();
            let mut result_bytes = 0usize;
            for row in result.rows.iter().take(MAX_RESULT_ROWS) {
                let bytes = row.iter().fold(0usize, |sum, v| {
                    sum.saturating_add(cypher::memory::value_heap_bytes(v))
                        .saturating_add(std::mem::size_of::<cypher::types::CypherValue>())
                });
                // Reserve conversion/object overhead in addition to owned values.
                result_bytes = result_bytes.saturating_add(bytes.saturating_mul(4));
                if result_bytes > RESULT_VALUE_BYTES {
                    break;
                }
                rows_json.push(row.iter().map(cypher_value_to_json).collect());
            }
            let truncated = rows_json.len() < result.rows.len();
            let (nodes, edges, mut entities_truncated) =
                collect_distinct_entities(&result.rows[..rows_json.len()], MAX_ENTITIES);
            entities_truncated |= truncated;

            // Bound-but-not-returned entities: materialize the refs the
            // executor harvested from the leftover frame slots, with the
            // same machinery the rows went through.
            let (mut bound_nodes, mut bound_edges) = (Vec::new(), Vec::new());
            {
                let graph = store.unregistered_read();
                let mut bound_bytes = 0usize;
                for cv in result.bound.iter().take(MAX_ENTITIES) {
                    let m = cypher::executor::materialize_value(cv.clone(), &graph);
                    bound_bytes = bound_bytes.saturating_add(
                        cypher::memory::value_heap_bytes(&m)
                            .saturating_add(128)
                            .saturating_mul(4),
                    );
                    if bound_bytes > RESULT_VALUE_BYTES {
                        entities_truncated = true;
                        break;
                    }
                    let j = cypher_value_to_json(&m);
                    match j.get("_type").and_then(|t| t.as_str()) {
                        Some("node") => bound_nodes.push(j),
                        Some("edge") => bound_edges.push(j),
                        _ => {}
                    }
                }
                entities_truncated |= result.bound.len() > MAX_ENTITIES;
            }

            // EXPLAIN / PROFILE leave `rows` empty and put the plan in `result.plan` as a
            // CypherValue tree; JSON for the result panel's JSON tab
            let plan = result.plan.as_ref().map(cypher_value_to_json);

            let response = QueryResponse {
                columns: result.columns.clone(),
                total_rows: result.rows.len(),
                rows: rows_json,
                truncated,
                nodes,
                edges,
                entities_truncated,
                bound_nodes,
                bound_edges,
                query_type: query_type_label(result.query_type),
                stats: stats_to_json(&result.stats),
                plan,
                time_ms: now_ms() - started,
                parsing_time_ms: parse_dur.as_secs_f64() * 1000.0,
                planning_time_ms: plan_dur.as_secs_f64() * 1000.0,
                execution_time_ms: exec_dur.as_secs_f64() * 1000.0,
            };
            // `serialize_maps_as_objects(true)` is essential: by default serde_wasm_bindgen turns
            // Rust maps (including the `serde_json::Map` in Value::Object) into JS `Map`s, which
            // JSON.stringify renders as `{}` and Studio's row renderer can't index; with it every
            // map is a plain object, as in the HTTP response
            response
                .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
                .map_err(|e| JsValue::from_str(&format!("serialize: {e}")))
        }

        /// Algorithm catalog, same shape as HTTP `/api/algorithms`; Studio's GDS drawer builds its
        /// picker from it.
        pub fn algorithms(&self) -> Result<JsValue, JsValue> {
            install_panic_hook();
            cypher::algorithms::ensure_registered();
            let mut entries = cypher::algorithms::registry::list();
            entries.sort_by_key(|e| (e.metadata.category.as_str(), e.metadata.name));

            let items: Vec<AlgorithmEntry> = entries
                .iter()
                .map(|entry| {
                    let m = &entry.metadata;
                    let modes: Vec<&'static str> = if m.output_kind.writable() {
                        vec!["stream", "stats", "write"]
                    } else {
                        vec!["stream", "stats"]
                    };
                    let config_schema: Vec<ConfigParamJson> = m
                        .params
                        .iter()
                        .map(|p| ConfigParamJson {
                            required: p.required,
                            key: p.key,
                            ty: p.ty.as_str(),
                            default: cypher_value_to_json(&p.default),
                            description: p.description,
                            advanced: p.advanced,
                        })
                        .collect();
                    AlgorithmEntry {
                        name: m.name,
                        display_name: m.display_name,
                        category: m.category.as_str(),
                        output_kind: m.output_kind.as_str(),
                        stability: m.stability.as_str(),
                        description: m.description,
                        writable: m.output_kind.writable(),
                        modes,
                        config_schema,
                        output_columns: m.output_columns.to_vec(),
                        stats_columns: m.stats_columns.to_vec(),
                        write_columns: m.write_columns.to_vec(),
                    }
                })
                .collect();
            AlgorithmsResponse { algorithms: items }
                .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
                .map_err(|e| JsValue::from_str(&format!("serialize: {e}")))
        }

        /// Splits a script into statements, like the server's import endpoint and the
        /// CLI. `query` takes one statement, so pasted scripts are cut up first; this is
        /// the same splitter (a bare split on `;` would cut `{name: 'Smith; Jones'}` in
        /// half).
        ///
        /// Semicolons in string literals and line comments aren't separators; empty
        /// statements are dropped; a trailing statement without a semicolon is kept.
        #[wasm_bindgen(js_name = splitStatements)]
        pub fn split_statements(script: &str) -> Vec<String> {
            traverse_cypher::statements::split_statements(script)
                .into_iter()
                .map(str::to_owned)
                .collect()
        }

        /// Serializes the whole graph as a `.tvdb` byte buffer, **the native server's
        /// on-disk format**: same paged layout, record encoding and compression, so it
        /// loads in the native server via `Database::open` and vice versa. Typically
        /// persisted to OPFS by the Worker and read back with `loadTvdb`. Written from a
        /// checkpoint: queries and writes carry on meanwhile.
        #[wasm_bindgen(js_name = exportTvdb)]
        pub fn export_tvdb(&self) -> Result<Vec<u8>, JsValue> {
            let mut pm = PageManager::create_in_memory();
            let store = self.store()?;
            store
                .checkpoint()
                .and_then(|checkpoint| {
                    traverse_core::storage::codec::write_checkpoint(&mut pm, &checkpoint, &[])
                })
                .map_err(|e| JsValue::from_str(&format!("tvdb encode: {e}")))?;
            Ok(pm.to_bytes())
        }

        /// Replaces the current graph with one from a `.tvdb` buffer (from `exportTvdb` or the
        /// native server); the existing graph is dropped.
        #[wasm_bindgen(js_name = loadTvdb)]
        pub fn load_tvdb(&self, bytes: Vec<u8>) -> Result<(), JsValue> {
            let pm = PageManager::open_from_bytes(bytes)
                .map_err(|e| JsValue::from_str(&format!("tvdb open: {e}")))?;
            let (new_graph, _locations, _pm, _metadata) =
                traverse_core::storage::codec::load_graph(pm)
                    .map_err(|e| JsValue::from_str(&format!("tvdb decode: {e}")))?;
            self.replace(new_graph)
        }

        /// Resets to an empty graph, dropping indexes and properties too.
        pub fn clear(&self) -> Result<(), JsValue> {
            self.replace(Graph::new())
        }

        /// Current node count.
        #[wasm_bindgen(js_name = nodeCount)]
        pub fn node_count(&self) -> Result<u32, JsValue> {
            Ok(self.store()?.with_committed_graph(|g| g.node_count()) as u32)
        }

        /// Current edge count.
        #[wasm_bindgen(js_name = edgeCount)]
        pub fn edge_count(&self) -> Result<u32, JsValue> {
            Ok(self.store()?.with_committed_graph(|g| g.edge_count()) as u32)
        }

        /// Approximate in-memory size of the graph in bytes (node + edge property + index storage;
        /// not the JS heap or WASM linear memory). Same as the native server's per-database
        /// `memory_bytes`, so Studio's header shows real numbers in WASM mode.
        #[wasm_bindgen(js_name = estimatedMemory)]
        pub fn estimated_memory(&self) -> Result<u32, JsValue> {
            Ok(self.store()?.estimated_memory() as u32)
        }

        /// Library version (the workspace Cargo version).
        #[wasm_bindgen(js_name = version)]
        pub fn version() -> String {
            env!("CARGO_PKG_VERSION").to_string()
        }

        /// Schema summary: labels, relationship types, property keys, per-label / per-edge-type
        /// property maps, indexes and constraints, in the HTTP `/api/schema` shape so Studio's
        /// SchemaBrowser renders WASM databases identically.
        pub fn schema(&self) -> Result<JsValue, JsValue> {
            let store = self.store()?;
            store.with_committed_graph(|g| self.schema_of(g))
        }
    }

    impl TraverseDb {
        /// The current store.
        fn store(&self) -> Result<Arc<MvccStore>, JsValue> {
            self.store
                .lock()
                .map(|s| Arc::clone(&s))
                .map_err(|e| JsValue::from_str(&format!("store mutex poisoned: {e}")))
        }

        /// Swaps in a new database.
        fn replace(&self, graph: Graph) -> Result<(), JsValue> {
            let mut store = self
                .store
                .lock()
                .map_err(|e| JsValue::from_str(&format!("store mutex poisoned: {e}")))?;
            *store = Arc::new(MvccStore::new(graph));
            Ok(())
        }

        /// The schema summary of `g`; see [`TraverseDb::schema`].
        fn schema_of(&self, g: &Graph) -> Result<JsValue, JsValue> {
            use traverse_core::graph::constraint::ConstraintType;
            use traverse_core::tx::view::GraphView;

            let labels = g.schema_labels();
            let rel_types = g.schema_edge_types();
            let property_keys: Vec<SchemaProp> = g
                .schema_property_types()
                .into_iter()
                .map(|(name, ty)| SchemaProp { name, ty })
                .collect();
            let (labels_owner, edges_owner) = g.schema_properties_by_owner();
            let labels_detail: Vec<SchemaOwner> = labels_owner
                .into_iter()
                .map(|(name, props)| SchemaOwner {
                    name,
                    properties: props
                        .into_iter()
                        .map(|(n, t)| SchemaProp { name: n, ty: t })
                        .collect(),
                })
                .collect();
            let relationship_types_detail: Vec<SchemaOwner> = edges_owner
                .into_iter()
                .map(|(name, props)| SchemaOwner {
                    name,
                    properties: props
                        .into_iter()
                        .map(|(n, t)| SchemaProp { name: n, ty: t })
                        .collect(),
                })
                .collect();

            // walk the index catalog + constraint registry straight off the graph (the server goes
            // through GraphView, which Graph implements, so no MvccStore round trip here)
            let strings = <Graph as GraphView>::strings(g);
            let indexes: Vec<SchemaIndex> = <Graph as GraphView>::index_catalog(g)
                .descriptors()
                .iter()
                .filter_map(|d| {
                    let label = strings.try_resolve(d.label_id.0)?;
                    let property = strings.try_resolve(d.property_key.0)?;
                    Some(SchemaIndex {
                        name: d.name.clone(),
                        label: label.to_string(),
                        property: property.to_string(),
                        entries: g.index_size(&d.name).unwrap_or(0) as u64,
                    })
                })
                .collect();
            let constraints: Vec<SchemaConstraint> = <Graph as GraphView>::constraint_registry(g)
                .descriptors()
                .iter()
                .filter_map(|d| {
                    let label = strings.try_resolve(d.label_id.0)?;
                    let property = strings.try_resolve(d.property_key.0)?;
                    // rendered like the server's HTTP schema route; Studio reads both
                    // interchangeably
                    let ty = match &d.constraint_type {
                        ConstraintType::Unique => "unique".to_string(),
                        ConstraintType::Exists => "exists".to_string(),
                        ConstraintType::Type(k) => format!("type:{}", k.surface()),
                        ConstraintType::NodeKey => "node_key".to_string(),
                    };
                    Some(SchemaConstraint {
                        name: d.name.clone(),
                        ty,
                        label: label.to_string(),
                        property: property.to_string(),
                    })
                })
                .collect();

            let response = SchemaResponse {
                labels,
                relationship_types: rel_types,
                property_keys,
                indexes,
                constraints,
                labels_detail,
                relationship_types_detail,
            };
            response
                .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
                .map_err(|e| JsValue::from_str(&format!("serialize schema: {e}")))
        }
    }

    /// HTTP `/api/query` `stats`, same counter names, so Studio's result panel reads them directly.
    /// i64 fields, as natively: node/edge counts can exceed u32.
    #[derive(serde::Serialize, Default)]
    struct QueryStatsJson {
        nodes_created: i64,
        nodes_deleted: i64,
        relationships_created: i64,
        relationships_deleted: i64,
        properties_set: i64,
        labels_added: i64,
        labels_removed: i64,
        indexes_added: i64,
        indexes_removed: i64,
        constraints_added: i64,
        constraints_removed: i64,
    }

    fn stats_to_json(s: &cypher::executor::QueryStats) -> QueryStatsJson {
        QueryStatsJson {
            nodes_created: s.nodes_created,
            nodes_deleted: s.nodes_deleted,
            relationships_created: s.relationships_created,
            relationships_deleted: s.relationships_deleted,
            properties_set: s.properties_set,
            labels_added: s.labels_added,
            labels_removed: s.labels_removed,
            indexes_added: s.indexes_added,
            indexes_removed: s.indexes_removed,
            constraints_added: s.constraints_added,
            constraints_removed: s.constraints_removed,
        }
    }

    /// The HTTP server's classification (`Read` / `Write` / `ReadWrite` / `Schema`), shown as the
    /// result panel chip.
    fn query_type_label(qt: cypher::executor::QueryType) -> String {
        match qt {
            cypher::executor::QueryType::Read => "Read",
            cypher::executor::QueryType::Write => "Write",
            cypher::executor::QueryType::ReadWrite => "ReadWrite",
            cypher::executor::QueryType::Schema => "Schema",
        }
        .to_string()
    }

    /// Schema property (name + observed type), the HTTP `PropertyKeyInfo` JSON shape.
    #[derive(serde::Serialize)]
    struct SchemaProp {
        name: String,
        #[serde(rename = "type")]
        ty: &'static str,
    }

    /// The HTTP `IndexInfo` JSON for Studio's SchemaBrowser.
    #[derive(serde::Serialize)]
    struct SchemaIndex {
        name: String,
        label: String,
        property: String,
        entries: u64,
    }

    /// The HTTP `ConstraintInfo` JSON for Studio's SchemaBrowser.
    #[derive(serde::Serialize)]
    struct SchemaConstraint {
        name: String,
        #[serde(rename = "type")]
        ty: String,
        label: String,
        property: String,
    }

    /// Properties per label / edge type.
    #[derive(serde::Serialize)]
    struct SchemaOwner {
        name: String,
        properties: Vec<SchemaProp>,
    }

    /// JS-visible schema response, the HTTP `SchemaResponse` shape, so Studio reuses its types.
    #[derive(serde::Serialize)]
    struct SchemaResponse {
        labels: Vec<String>,
        relationship_types: Vec<String>,
        property_keys: Vec<SchemaProp>,
        indexes: Vec<SchemaIndex>,
        constraints: Vec<SchemaConstraint>,
        labels_detail: Vec<SchemaOwner>,
        relationship_types_detail: Vec<SchemaOwner>,
    }

    // -- helpers -------------------------------------------------------

    /// Optional `params_json` -> Cypher params map. Accepts `null`, `undefined`, a JS object or a
    /// JSON-stringified object; anything else is an error.
    fn parse_params(
        v: JsValue,
    ) -> Result<std::collections::HashMap<String, cypher::types::CypherValue>, JsValue> {
        if v.is_null() || v.is_undefined() {
            return Ok(Default::default());
        }
        // structured JS object first
        if let Ok(json) = serde_wasm_bindgen::from_value::<serde_json::Value>(v.clone()) {
            return Ok(json_to_params(&json));
        }
        // else a JSON-encoded string
        if let Some(s) = v.as_string() {
            let parsed: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| JsValue::from_str(&format!("invalid params JSON: {e}")))?;
            return Ok(json_to_params(&parsed));
        }
        Err(JsValue::from_str("params must be an object or JSON string"))
    }

    fn json_to_params(
        v: &serde_json::Value,
    ) -> std::collections::HashMap<String, cypher::types::CypherValue> {
        let mut out = std::collections::HashMap::new();
        if let serde_json::Value::Object(map) = v {
            for (k, val) in map {
                out.insert(k.clone(), json_value_to_cypher(val));
            }
        }
        out
    }

    fn json_value_to_cypher(v: &serde_json::Value) -> cypher::types::CypherValue {
        use cypher::types::CypherValue;
        match v {
            serde_json::Value::Null => CypherValue::Null,
            serde_json::Value::Bool(b) => CypherValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    CypherValue::Integer(i)
                } else if let Some(f) = n.as_f64() {
                    CypherValue::Float(f)
                } else {
                    CypherValue::Null
                }
            }
            serde_json::Value::String(s) => CypherValue::String(s.clone()),
            serde_json::Value::Array(arr) => {
                CypherValue::List(arr.iter().map(json_value_to_cypher).collect())
            }
            serde_json::Value::Object(map) => {
                let mut m = std::collections::BTreeMap::new();
                for (k, v) in map {
                    m.insert(k.clone(), json_value_to_cypher(v));
                }
                CypherValue::Map(m)
            }
        }
    }

    /// Cypher -> JSON in the HTTP server's shape: Node/Edge/Path cells become tagged objects
    /// (`_type: "node"` etc.) the Studio renderer recognizes, so the graph view has structured data
    /// to draw.
    fn cypher_value_to_json(v: &cypher::types::CypherValue) -> serde_json::Value {
        use cypher::types::CypherValue;
        match v {
            CypherValue::Null => serde_json::Value::Null,
            CypherValue::Bool(b) => serde_json::Value::Bool(*b),
            CypherValue::Integer(i) => serde_json::json!(*i),
            CypherValue::Float(f) => serde_json::json!(*f),
            CypherValue::String(s) => serde_json::Value::String(s.clone()),
            CypherValue::List(items) => {
                serde_json::Value::Array(items.iter().map(cypher_value_to_json).collect())
            }
            CypherValue::Map(m) => {
                let mut obj = serde_json::Map::new();
                for (k, val) in m {
                    obj.insert(k.clone(), cypher_value_to_json(val));
                }
                serde_json::Value::Object(obj)
            }
            CypherValue::Node {
                id,
                labels,
                properties,
            } => serde_json::json!({
                "_type": "node",
                "id": id.0,
                "labels": labels,
                "properties": property_map_to_json(properties),
            }),
            CypherValue::Edge {
                id,
                edge_type,
                source,
                target,
                properties,
            } => serde_json::json!({
                "_type": "edge",
                "id": id.0,
                "type": edge_type,
                "source": source.0,
                "target": target.0,
                "properties": property_map_to_json(properties),
            }),
            CypherValue::NodeRef(nid) => serde_json::json!({ "_type": "node_ref", "id": nid.0 }),
            CypherValue::EdgeRef { id, .. } => {
                serde_json::json!({ "_type": "edge_ref", "id": id.0 })
            }
            CypherValue::Path { nodes, edges } => serde_json::json!({
                "_type": "path",
                "nodes": nodes.iter().map(cypher_value_to_json).collect::<Vec<_>>(),
                "edges": edges.iter().map(cypher_value_to_json).collect::<Vec<_>>(),
            }),
            // temporal & other types: debug string
            other => serde_json::Value::String(format!("{other:?}")),
        }
    }

    /// Property map -> JSON object, like the server's helper.
    fn property_map_to_json(
        props: &std::collections::BTreeMap<String, PropertyValue>,
    ) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        for (k, v) in props {
            obj.insert(k.clone(), property_value_to_json(v));
        }
        serde_json::Value::Object(obj)
    }

    /// Property value -> JSON, like the server's `property_value_to_json`.
    fn property_value_to_json(pv: &PropertyValue) -> serde_json::Value {
        match pv {
            PropertyValue::Null => serde_json::Value::Null,
            PropertyValue::Bool(b) => serde_json::Value::Bool(*b),
            PropertyValue::Int(i) => serde_json::json!(*i),
            PropertyValue::Float(f) => serde_json::json!(*f),
            PropertyValue::String(s) => serde_json::Value::String(s.clone()),
            PropertyValue::Bytes(b) => serde_json::json!(b),
            PropertyValue::List(items) => {
                serde_json::Value::Array(items.iter().map(property_value_to_json).collect())
            }
            PropertyValue::Map(m) => {
                let mut obj = serde_json::Map::new();
                for (k, v) in m {
                    obj.insert(k.clone(), property_value_to_json(v));
                }
                serde_json::Value::Object(obj)
            }
            PropertyValue::DateTime(ms) => serde_json::json!({ "datetime_ms": ms }),
            PropertyValue::Duration(ms) => serde_json::json!({ "duration_ms": ms }),
            PropertyValue::Point2D { x, y, srid } => {
                serde_json::json!({ "x": x, "y": y, "srid": srid })
            }
            PropertyValue::Point3D { x, y, z, srid } => {
                serde_json::json!({ "x": x, "y": y, "z": z, "srid": srid })
            }
            PropertyValue::Vector(v) => serde_json::json!(v),
        }
    }

    /// Collects distinct nodes and edges (by id) from every cell in `rows` for the graph view, like
    /// the server's collector in `http::routes::query`: Path cells flattened, List/Map cells recursed into,
    /// NodeRef/EdgeRef skipped (nothing to render). Stops after `max_entities` of either kind.
    fn collect_distinct_entities(
        rows: &[Vec<cypher::types::CypherValue>],
        max_entities: usize,
    ) -> (Vec<serde_json::Value>, Vec<serde_json::Value>, bool) {
        use cypher::types::CypherValue;
        use std::collections::HashSet;
        let mut node_ids: HashSet<u64> = HashSet::new();
        let mut edge_ids: HashSet<u64> = HashSet::new();
        let mut nodes: Vec<serde_json::Value> = Vec::new();
        let mut edges: Vec<serde_json::Value> = Vec::new();
        let mut truncated = false;

        fn visit(
            cv: &CypherValue,
            node_ids: &mut HashSet<u64>,
            edge_ids: &mut HashSet<u64>,
            nodes: &mut Vec<serde_json::Value>,
            edges: &mut Vec<serde_json::Value>,
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

    /// Wall-clock ms from `performance.now()` (high-resolution monotonic), for query timing.
    fn now_ms() -> f64 {
        if let Some(window) = web_sys::window()
            && let Some(perf) = window.performance()
        {
            return perf.now();
        }
        // worker scope fallback
        if let Ok(scope) = js_sys::global().dyn_into::<web_sys::WorkerGlobalScope>()
            && let Some(perf) = scope.performance()
        {
            return perf.now();
        }
        // last resort: Date.now() in ms
        js_sys::Date::now()
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::*;
