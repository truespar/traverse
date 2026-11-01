//! The `TraverseService` RPCs: queries, transactions, CRUD, schema, database info and the
//! query channel.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use traverse_core::graph::types::{EdgeId, NodeId};
use traverse_core::tx::GraphView;

use crate::notification::{self, Severity};
use crate::{is_admin_query, is_cache_bypass, is_write_query};

use super::super::convert;
use super::super::proto;
use super::super::proto::traverse_service_server::TraverseService;
use super::procedures::{execute_auto_commit, handle_procedure};
use super::stream::{
    send_channel_result, send_channel_result_streaming, send_result, send_result_streaming,
};
use super::{
    FgServiceImpl, client_limit, cypher_err_to_status, graph_err_to_status, require_admin,
    require_write, resolve_database, validate_bookmarks, watch_disconnect,
};

#[tonic::async_trait]
impl TraverseService for FgServiceImpl {
    type ExecuteQueryStream = ReceiverStream<Result<proto::ExecuteQueryResponse, Status>>;

    async fn execute_query(
        &self,
        request: Request<proto::ExecuteQueryRequest>,
    ) -> Result<Response<Self::ExecuteQueryStream>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        let client_address = request
            .remote_addr()
            .map_or_else(String::new, |a| a.to_string());
        let req = request.into_inner();

        // Log impersonation if requested (parsed + logged, not enforced)
        if !req.impersonate_user.is_empty() {
            tracing::debug!("impersonating user: {}", req.impersonate_user);
        }

        // Resolve database: for explicit transactions, use the session's database;
        // for auto-commit, resolve from the request field.
        let (db_entry, db_name) = if !req.transaction_id.is_empty() {
            let session_db = self.sessions.database_name(&req.transaction_id).await?;
            if !req.database.is_empty() && req.database != session_db {
                return Err(Status::invalid_argument(format!(
                    "database '{}' does not match transaction database '{}'",
                    req.database, session_db
                )));
            }
            (resolve_database(&session_db, &self.registry)?.0, session_db)
        } else {
            resolve_database(&req.database, &self.registry)?
        };

        // Validate bookmarks
        validate_bookmarks(&req.bookmarks, &db_entry.store)?;

        let params = convert::proto_params_to_cypher(&req.parameters)?;
        let read_only = req.access_mode.eq_ignore_ascii_case("r");
        let timeout_ms = req.timeout_ms;

        // RBAC pre-checks (before spawn_blocking so errors are returned as
        // gRPC status, not as stream messages).
        if auth.role.is_some() {
            if is_admin_query(&req.query) {
                require_admin(&auth)?;
            }
            if is_write_query(&req.query) {
                require_write(&auth)?;
            }
        }

        let registry = Arc::clone(&self.registry);
        let sessions = Arc::clone(&self.sessions);
        let auth_catalog = self.auth_catalog.clone();
        let (tx, rx) = mpsc::channel(512);

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let t_start = Instant::now();
            let store = &db_entry.store;

            // Check for built-in procedures first
            let query_trimmed = req.query.trim();
            if let Some(proc_result) = handle_procedure(
                query_trimmed,
                store,
                &registry,
                &db_name,
                &auth_catalog,
                &auth,
            ) {
                match proc_result {
                    Ok(result) => {
                        send_result(&rt, &tx, result, None, db_name, t_start, vec![]);
                    }
                    Err(status) => {
                        let _ = rt.block_on(tx.send(Err(status)));
                    }
                }
                return;
            }

            // RBAC: reject writes for Reader role (non-procedure queries)
            if auth.role.is_some()
                && is_write_query(&req.query)
                && let Err(e) = require_write(&auth)
            {
                let _ = rt.block_on(tx.send(Err(e)));
                return;
            }

            // Check read-only enforcement for write queries (auto-commit and explicit tx)
            if read_only && is_write_query(&req.query) {
                let _ = rt.block_on(tx.send(Err(Status::permission_denied(
                    "writing in read access mode not allowed",
                ))));
                return;
            }

            // Enforce read-only databases
            if db_entry.read_only && is_write_query(&req.query) {
                let _ =
                    rt.block_on(tx.send(Err(Status::permission_denied("database is read-only"))));
                return;
            }

            let principal = crate::statement::principal(auth.username.as_deref(), auth.role);
            let shown_params = Arc::new(params.clone());
            // an auto-commit statement is a transaction of its own, listed while it runs and
            // stopped by its time limit, TERMINATE TRANSACTIONS or the client going away
            let auto_commit = || {
                let transaction = crate::statement::register(
                    &db_name,
                    auth.username.as_deref(),
                    "grpc",
                    &crate::statement::next_connection_id("grpc"),
                    &client_address,
                    "",
                    BTreeMap::new(),
                );
                let running = crate::statement::start(
                    &transaction,
                    crate::statement::time_limit(client_limit(timeout_ms)).0,
                    principal.clone(),
                    &req.query,
                    Some(Arc::clone(&shown_params)),
                );
                (transaction, running)
            };

            // Parse notification settings from request (default: Warning, no disabled categories)
            let notif_min_severity = if req.notification_minimum_severity.is_empty() {
                Some(Severity::Warning)
            } else {
                Severity::parse(&req.notification_minimum_severity)
            };
            let notif_disabled: Vec<String> = req
                .notification_disabled_categories
                .iter()
                .map(|s| s.to_uppercase())
                .collect();

            // Analyze query for notifications (performance hints, unknown labels)
            let notifications = notification::analyze_query_notifications(
                &req.query,
                store,
                notif_min_severity,
                &notif_disabled,
            );

            // Streaming path: auto-commit reads (no transaction_id, not a write)
            if req.transaction_id.is_empty() && (read_only || !is_write_query(&req.query)) {
                let (_transaction, running) = auto_commit();
                let watcher = watch_disconnect(&rt, &tx, &running.ctx);
                let _query = running.ctx.enter();
                let mut read_tx = match store.begin_read() {
                    Ok(tx) => tx,
                    Err(e) => {
                        let _ =
                            rt.block_on(tx.send(Err(Status::resource_exhausted(e.to_string()))));
                        return;
                    }
                };

                let bypass_cache = is_cache_bypass(query_trimmed);

                // Strip literals for cache key normalization
                let (stripped, stripped_params) = traverse_cypher::strip_literals(&req.query)
                    .unwrap_or_else(|_| (req.query.clone(), vec![]));

                // Merge stripped params into user params
                let mut full_params = params.clone();
                for (i, val) in stripped_params.iter().enumerate() {
                    full_params.insert(format!("__sp{i}"), val.clone());
                }

                let index_gen = traverse_core::tx::GraphView::index_catalog(&read_tx).generation();

                let prepared: std::result::Result<
                    std::sync::Arc<traverse_cypher::PreparedQuery>,
                    traverse_cypher::error::CypherError,
                > = if !bypass_cache {
                    if let Some(cached) = db_entry.plan_cache.get(&stripped, index_gen) {
                        Ok(cached)
                    } else {
                        traverse_cypher::prepare_query(&read_tx, &stripped, &full_params)
                            .map(std::sync::Arc::new)
                    }
                } else {
                    traverse_cypher::prepare_query(&read_tx, &stripped, &full_params)
                        .map(std::sync::Arc::new)
                };

                let prepared = match prepared {
                    Ok(p) => {
                        // cache the plan on a miss
                        if !bypass_cache && db_entry.plan_cache.get(&stripped, index_gen).is_none()
                        {
                            db_entry.plan_cache.insert(
                                stripped,
                                std::sync::Arc::clone(&p),
                                index_gen,
                            );
                        }
                        p
                    }
                    Err(e) => {
                        watcher.abort();
                        let _ = rt.block_on(tx.send(Err(cypher_err_to_status(e))));
                        return;
                    }
                };

                match traverse_cypher::execute_prepared_streaming(
                    &mut read_tx,
                    &prepared,
                    &full_params,
                ) {
                    Ok(traverse_cypher::executor::QueryExecution::Streaming(stream)) => {
                        send_result_streaming(&rt, &tx, stream, db_name, t_start, notifications);
                    }
                    Ok(traverse_cypher::executor::QueryExecution::Materialized(result)) => {
                        send_result(&rt, &tx, result, None, db_name, t_start, notifications);
                    }
                    Err(e) => {
                        let _ = rt.block_on(tx.send(Err(cypher_err_to_status(e))));
                    }
                }
                watcher.abort();
            } else if req.transaction_id.is_empty() {
                // Auto-commit write - materialize as before
                let (_transaction, running) = auto_commit();
                let watcher = watch_disconnect(&rt, &tx, &running.ctx);
                let outcome = {
                    let _query = running.ctx.enter();
                    execute_auto_commit(&db_entry, &req.query, &params, read_only, &running.ctx)
                };
                watcher.abort();
                match outcome {
                    Ok((result, bookmark)) => {
                        send_result(&rt, &tx, result, bookmark, db_name, t_start, notifications);
                    }
                    Err(status) => {
                        let _ = rt.block_on(tx.send(Err(status)));
                    }
                }
            } else {
                // explicit transaction, materialized; writes are refused if it began read-only
                let run = |otx: &mut traverse_core::tx::OwnedOptimisticTx,
                           registration: &traverse_cypher::transactions::Transaction,
                           remaining: Option<std::time::Duration>| {
                    let limit = match (remaining, client_limit(timeout_ms)) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    let running = crate::statement::start(
                        registration,
                        crate::statement::time_limit(limit).0,
                        principal.clone(),
                        &req.query,
                        Some(Arc::clone(&shown_params)),
                    );
                    let watcher = watch_disconnect(&rt, &tx, &running.ctx);
                    let result = {
                        let _query = running.ctx.enter();
                        traverse_cypher::execute_with_params(otx, &req.query, &params)
                            .map_err(cypher_err_to_status)
                    };
                    watcher.abort();
                    result
                };
                let res = rt.block_on(sessions.with_session_query(
                    &req.transaction_id,
                    is_write_query(&req.query),
                    run,
                ));
                match res {
                    Ok(result) => {
                        send_result(&rt, &tx, result, None, db_name, t_start, notifications);
                    }
                    Err(status) => {
                        let _ = rt.block_on(tx.send(Err(status)));
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // -- Node CRUD ----------------------------------------------------

    async fn create_node(
        &self,
        request: Request<proto::CreateNodeRequest>,
    ) -> Result<Response<proto::CreateNodeResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();
        let props = convert::proto_map_to_property_map(&req.properties)?;
        let label_refs: Vec<&str> = req.labels.iter().map(String::as_str).collect();

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            let node_id = write_tx
                .create_node(&label_refs, props)
                .map_err(|e| Status::internal(e.to_string()))?;
            let node = convert::read_node_to_proto(&write_tx, node_id)?;
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::CreateNodeResponse {
                node: Some(node),
            }))
        } else {
            let node = self
                .sessions
                .with_session_write(&req.transaction_id, |tx| {
                    let node_id = GraphView::create_node(tx, &label_refs, props)
                        .map_err(graph_err_to_status)?;
                    convert::read_node_to_proto(tx, node_id)
                })
                .await?;
            Ok(Response::new(proto::CreateNodeResponse {
                node: Some(node),
            }))
        }
    }

    async fn get_node(
        &self,
        request: Request<proto::GetNodeRequest>,
    ) -> Result<Response<proto::GetNodeResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let req = request.into_inner();
        let node_id = NodeId(req.id);

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let read_tx = db_entry
                .store
                .begin_read()
                .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
            let node = convert::read_node_to_proto(&read_tx, node_id)?;
            Ok(Response::new(proto::GetNodeResponse { node: Some(node) }))
        } else {
            let node = self
                .sessions
                .with_session(&req.transaction_id, |tx| {
                    convert::read_node_to_proto(tx, node_id)
                })
                .await?;
            Ok(Response::new(proto::GetNodeResponse { node: Some(node) }))
        }
    }

    async fn delete_node(
        &self,
        request: Request<proto::DeleteNodeRequest>,
    ) -> Result<Response<proto::DeleteNodeResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();
        let node_id = NodeId(req.id);

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            if req.detach {
                write_tx
                    .delete_node_cascading(node_id)
                    .map_err(graph_err_to_status)?;
            } else {
                write_tx.delete_node(node_id).map_err(graph_err_to_status)?;
            }
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::DeleteNodeResponse {}))
        } else {
            self.sessions
                .with_session_write(&req.transaction_id, |tx| {
                    if req.detach {
                        GraphView::delete_node_cascading(tx, node_id).map_err(graph_err_to_status)
                    } else {
                        GraphView::delete_node(tx, node_id).map_err(graph_err_to_status)
                    }
                })
                .await?;
            Ok(Response::new(proto::DeleteNodeResponse {}))
        }
    }

    // -- Edge CRUD ----------------------------------------------------

    async fn create_edge(
        &self,
        request: Request<proto::CreateEdgeRequest>,
    ) -> Result<Response<proto::CreateEdgeResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();
        let props = convert::proto_map_to_property_map(&req.properties)?;
        let source = NodeId(req.source);
        let target = NodeId(req.target);

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            let edge_id = write_tx
                .create_edge(source, target, &req.edge_type, props)
                .map_err(graph_err_to_status)?;
            let edge = convert::read_edge_to_proto(&write_tx, edge_id)?;
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::CreateEdgeResponse {
                edge: Some(edge),
            }))
        } else {
            let edge = self
                .sessions
                .with_session_write(&req.transaction_id, |tx| {
                    let edge_id = GraphView::create_edge(tx, source, target, &req.edge_type, props)
                        .map_err(graph_err_to_status)?;
                    convert::read_edge_to_proto(tx, edge_id)
                })
                .await?;
            Ok(Response::new(proto::CreateEdgeResponse {
                edge: Some(edge),
            }))
        }
    }

    async fn get_edge(
        &self,
        request: Request<proto::GetEdgeRequest>,
    ) -> Result<Response<proto::GetEdgeResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let req = request.into_inner();
        let edge_id = EdgeId(req.id);

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let read_tx = db_entry
                .store
                .begin_read()
                .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
            let edge = convert::read_edge_to_proto(&read_tx, edge_id)?;
            Ok(Response::new(proto::GetEdgeResponse { edge: Some(edge) }))
        } else {
            let edge = self
                .sessions
                .with_session(&req.transaction_id, |tx| {
                    convert::read_edge_to_proto(tx, edge_id)
                })
                .await?;
            Ok(Response::new(proto::GetEdgeResponse { edge: Some(edge) }))
        }
    }

    async fn delete_edge(
        &self,
        request: Request<proto::DeleteEdgeRequest>,
    ) -> Result<Response<proto::DeleteEdgeResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();
        let edge_id = EdgeId(req.id);

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            write_tx.delete_edge(edge_id).map_err(graph_err_to_status)?;
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::DeleteEdgeResponse {}))
        } else {
            self.sessions
                .with_session_write(&req.transaction_id, |tx| {
                    GraphView::delete_edge(tx, edge_id).map_err(graph_err_to_status)
                })
                .await?;
            Ok(Response::new(proto::DeleteEdgeResponse {}))
        }
    }

    // -- Property updates ---------------------------------------------

    async fn update_properties(
        &self,
        request: Request<proto::UpdatePropertiesRequest>,
    ) -> Result<Response<proto::UpdatePropertiesResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();
        let props = convert::proto_map_to_property_map(&req.properties)?;
        let target = req
            .target
            .ok_or_else(|| Status::invalid_argument("target (node_id or edge_id) is required"))?;

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            match target {
                proto::update_properties_request::Target::NodeId(id) => {
                    write_tx
                        .update_node_properties(NodeId(id), props)
                        .map_err(graph_err_to_status)?;
                }
                proto::update_properties_request::Target::EdgeId(id) => {
                    write_tx
                        .update_edge_properties(EdgeId(id), props)
                        .map_err(graph_err_to_status)?;
                }
            }
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::UpdatePropertiesResponse {}))
        } else {
            self.sessions
                .with_session_write(&req.transaction_id, |tx| match target {
                    proto::update_properties_request::Target::NodeId(id) => {
                        GraphView::update_node_properties(tx, NodeId(id), props)
                            .map_err(graph_err_to_status)
                    }
                    proto::update_properties_request::Target::EdgeId(id) => {
                        GraphView::update_edge_properties(tx, EdgeId(id), props)
                            .map_err(graph_err_to_status)
                    }
                })
                .await?;
            Ok(Response::new(proto::UpdatePropertiesResponse {}))
        }
    }

    // -- Transactions -------------------------------------------------

    async fn begin_transaction(
        &self,
        request: Request<proto::BeginTransactionRequest>,
    ) -> Result<Response<proto::BeginTransactionResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        let client_address = request
            .remote_addr()
            .map_or_else(String::new, |a| a.to_string());
        let req = request.into_inner();

        // Log impersonation if requested (parsed + logged, not enforced)
        if !req.impersonate_user.is_empty() {
            tracing::debug!("impersonating user: {}", req.impersonate_user);
        }

        // Resolve database
        let (db_entry, db_name) = resolve_database(&req.database, &self.registry)?;

        // Validate bookmarks
        validate_bookmarks(&req.bookmarks, &db_entry.store)?;

        // Force read-only for read-only databases
        let read_only = req.access_mode.eq_ignore_ascii_case("r") || db_entry.read_only;
        let timeout_ms = if req.timeout_ms > 0 {
            Some(req.timeout_ms)
        } else {
            None
        };

        let registration = crate::statement::register(
            &db_name,
            auth.username.as_deref(),
            "grpc",
            &crate::statement::next_connection_id("grpc"),
            &client_address,
            "",
            BTreeMap::new(),
        );
        let tx_id = self
            .sessions
            .begin(
                &db_entry.store,
                db_name,
                read_only,
                timeout_ms,
                registration,
            )
            .await?;
        Ok(Response::new(proto::BeginTransactionResponse {
            transaction_id: tx_id,
        }))
    }

    async fn commit_transaction(
        &self,
        request: Request<proto::CommitTransactionRequest>,
    ) -> Result<Response<proto::CommitTransactionResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let req = request.into_inner();
        let (bookmark, db_name) = self.sessions.commit(&req.transaction_id).await?;
        if let Some(entry) = self.registry.get(&db_name) {
            entry.mark_dirty();
        }
        Ok(Response::new(proto::CommitTransactionResponse { bookmark }))
    }

    async fn rollback_transaction(
        &self,
        request: Request<proto::RollbackTransactionRequest>,
    ) -> Result<Response<proto::RollbackTransactionResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let req = request.into_inner();
        self.sessions.rollback(&req.transaction_id).await?;
        Ok(Response::new(proto::RollbackTransactionResponse {}))
    }

    // -- Schema -------------------------------------------------------

    async fn create_index(
        &self,
        request: Request<proto::CreateIndexRequest>,
    ) -> Result<Response<proto::CreateIndexResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            write_tx
                .create_index(&req.name, &req.label, &req.property)
                .map_err(graph_err_to_status)?;
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::CreateIndexResponse {}))
        } else {
            self.sessions
                .with_session_write(&req.transaction_id, |tx| {
                    GraphView::create_index(tx, &req.name, &req.label, &req.property)
                        .map_err(graph_err_to_status)
                })
                .await?;
            Ok(Response::new(proto::CreateIndexResponse {}))
        }
    }

    async fn drop_index(
        &self,
        request: Request<proto::DropIndexRequest>,
    ) -> Result<Response<proto::DropIndexResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            write_tx
                .drop_index(&req.name)
                .map_err(graph_err_to_status)?;
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::DropIndexResponse {}))
        } else {
            self.sessions
                .with_session_write(&req.transaction_id, |tx| {
                    GraphView::drop_index(tx, &req.name).map_err(graph_err_to_status)
                })
                .await?;
            Ok(Response::new(proto::DropIndexResponse {}))
        }
    }

    // -- Database info ------------------------------------------------

    async fn get_labels(
        &self,
        request: Request<proto::GetLabelsRequest>,
    ) -> Result<Response<proto::GetLabelsResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let db_entry = self.registry.default_entry();
        let read_tx = db_entry
            .store
            .begin_read()
            .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
        let strings = GraphView::strings(&read_tx);
        let mut labels = std::collections::BTreeSet::new();
        for nid in GraphView::nodes_vec(&read_tx) {
            if let Ok(node) = GraphView::get_node(&read_tx, nid) {
                for lid in &node.labels {
                    labels.insert(strings.resolve(lid.0).to_string());
                }
            }
        }
        Ok(Response::new(proto::GetLabelsResponse {
            labels: labels.into_iter().collect(),
        }))
    }

    async fn get_relationship_types(
        &self,
        request: Request<proto::GetRelationshipTypesRequest>,
    ) -> Result<Response<proto::GetRelationshipTypesResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let db_entry = self.registry.default_entry();
        let read_tx = db_entry
            .store
            .begin_read()
            .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
        let strings = GraphView::strings(&read_tx);
        let mut types = std::collections::BTreeSet::new();
        for eid in GraphView::all_edges_vec(&read_tx) {
            if let Ok(edge) = GraphView::get_edge(&read_tx, eid) {
                types.insert(strings.resolve(edge.edge_type.0).to_string());
            }
        }
        Ok(Response::new(proto::GetRelationshipTypesResponse {
            types: types.into_iter().collect(),
        }))
    }

    async fn get_property_keys(
        &self,
        request: Request<proto::GetPropertyKeysRequest>,
    ) -> Result<Response<proto::GetPropertyKeysResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let db_entry = self.registry.default_entry();
        let read_tx = db_entry
            .store
            .begin_read()
            .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
        let strings = GraphView::strings(&read_tx);
        let mut keys = std::collections::BTreeSet::new();
        for nid in GraphView::nodes_vec(&read_tx) {
            if let Ok(node) = GraphView::get_node(&read_tx, nid) {
                for (k, _) in node.properties.iter() {
                    keys.insert(strings.resolve(k.0).to_string());
                }
            }
        }
        for eid in GraphView::all_edges_vec(&read_tx) {
            if let Ok(edge) = GraphView::get_edge(&read_tx, eid) {
                for (k, _) in edge.properties.iter() {
                    keys.insert(strings.resolve(k.0).to_string());
                }
            }
        }
        Ok(Response::new(proto::GetPropertyKeysResponse {
            keys: keys.into_iter().collect(),
        }))
    }

    async fn get_node_count(
        &self,
        request: Request<proto::GetNodeCountRequest>,
    ) -> Result<Response<proto::GetNodeCountResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let db_entry = self.registry.default_entry();
        let read_tx = db_entry
            .store
            .begin_read()
            .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
        let count = GraphView::node_count(&read_tx) as u64;
        Ok(Response::new(proto::GetNodeCountResponse { count }))
    }

    async fn get_edge_count(
        &self,
        request: Request<proto::GetEdgeCountRequest>,
    ) -> Result<Response<proto::GetEdgeCountResponse>, Status> {
        self.extract_auth(request.metadata())?;
        let db_entry = self.registry.default_entry();
        let read_tx = db_entry
            .store
            .begin_read()
            .map_err(|e| tonic::Status::resource_exhausted(e.to_string()))?;
        let count = GraphView::edge_count(&read_tx) as u64;
        Ok(Response::new(proto::GetEdgeCountResponse { count }))
    }

    // -- Batch CRUD ----------------------------------------------------

    async fn batch_create_nodes(
        &self,
        request: Request<proto::BatchCreateNodesRequest>,
    ) -> Result<Response<proto::BatchCreateNodesResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            let mut ids = Vec::with_capacity(req.nodes.len());
            for node_req in &req.nodes {
                let props = convert::proto_map_to_property_map(&node_req.properties)?;
                let label_refs: Vec<&str> = node_req.labels.iter().map(String::as_str).collect();
                let nid = write_tx
                    .create_node(&label_refs, props)
                    .map_err(|e| Status::internal(e.to_string()))?;
                ids.push(nid.0);
            }
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::BatchCreateNodesResponse {
                node_ids: ids,
            }))
        } else {
            let ids = self
                .sessions
                .with_session_write(&req.transaction_id, |tx| {
                    let mut ids = Vec::with_capacity(req.nodes.len());
                    for node_req in &req.nodes {
                        let props = convert::proto_map_to_property_map(&node_req.properties)?;
                        let label_refs: Vec<&str> =
                            node_req.labels.iter().map(String::as_str).collect();
                        let nid = GraphView::create_node(tx, &label_refs, props)
                            .map_err(graph_err_to_status)?;
                        ids.push(nid.0);
                    }
                    Ok(ids)
                })
                .await?;
            Ok(Response::new(proto::BatchCreateNodesResponse {
                node_ids: ids,
            }))
        }
    }

    async fn batch_create_edges(
        &self,
        request: Request<proto::BatchCreateEdgesRequest>,
    ) -> Result<Response<proto::BatchCreateEdgesResponse>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        require_write(&auth)?;
        let req = request.into_inner();

        if req.transaction_id.is_empty() {
            let db_entry = self.registry.default_entry();
            let mut write_tx = db_entry.store.begin_write();
            let mut ids = Vec::with_capacity(req.edges.len());
            for edge_req in &req.edges {
                let props = convert::proto_map_to_property_map(&edge_req.properties)?;
                let source = NodeId(edge_req.source);
                let target = NodeId(edge_req.target);
                let eid = write_tx
                    .create_edge(source, target, &edge_req.edge_type, props)
                    .map_err(graph_err_to_status)?;
                ids.push(eid.0);
            }
            write_tx
                .commit()
                .map_err(|e| Status::internal(e.to_string()))?;
            db_entry.mark_dirty();
            Ok(Response::new(proto::BatchCreateEdgesResponse {
                edge_ids: ids,
            }))
        } else {
            let ids = self
                .sessions
                .with_session_write(&req.transaction_id, |tx| {
                    let mut ids = Vec::with_capacity(req.edges.len());
                    for edge_req in &req.edges {
                        let props = convert::proto_map_to_property_map(&edge_req.properties)?;
                        let source = NodeId(edge_req.source);
                        let target = NodeId(edge_req.target);
                        let eid =
                            GraphView::create_edge(tx, source, target, &edge_req.edge_type, props)
                                .map_err(graph_err_to_status)?;
                        ids.push(eid.0);
                    }
                    Ok(ids)
                })
                .await?;
            Ok(Response::new(proto::BatchCreateEdgesResponse {
                edge_ids: ids,
            }))
        }
    }

    // -- QueryChannel (bidirectional streaming) ------------------------

    type QueryChannelStream = ReceiverStream<Result<proto::QueryChannelResponse, Status>>;

    async fn query_channel(
        &self,
        request: Request<Streaming<proto::QueryChannelRequest>>,
    ) -> Result<Response<Self::QueryChannelStream>, Status> {
        let auth = self.extract_auth(request.metadata())?;
        let client_address = request
            .remote_addr()
            .map_or_else(String::new, |a| a.to_string());
        let connection_id = crate::statement::next_connection_id("grpc");
        let mut request_stream = request.into_inner();
        let registry = Arc::clone(&self.registry);
        let auth_catalog = self.auth_catalog.clone();

        let (tx, rx) = mpsc::channel(512);

        tokio::spawn(async move {
            while let Ok(Some(req)) = request_stream.message().await {
                let request_id = req.request_id;
                let registry = Arc::clone(&registry);
                let auth_catalog = auth_catalog.clone();
                let auth = auth.clone();
                let tx = tx.clone();
                let client_address = client_address.clone();
                let connection_id = connection_id.clone();

                // Resolve database
                let (db_entry, db_name) = match resolve_database(&req.database, &registry) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx
                            .send(Ok(proto::QueryChannelResponse {
                                request_id,
                                payload: Some(proto::query_channel_response::Payload::Error(
                                    proto::QueryError {
                                        code: 5, // NOT_FOUND
                                        message: e.message().to_string(),
                                    },
                                )),
                            }))
                            .await;
                        continue;
                    }
                };

                let params = match convert::proto_params_to_cypher(&req.parameters) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx
                            .send(Ok(proto::QueryChannelResponse {
                                request_id,
                                payload: Some(proto::query_channel_response::Payload::Error(
                                    proto::QueryError {
                                        code: 3, // INVALID_ARGUMENT
                                        message: e.message().to_string(),
                                    },
                                )),
                            }))
                            .await;
                        continue;
                    }
                };

                let read_only = req.access_mode.eq_ignore_ascii_case("r");
                let timeout_ms = req.timeout_ms;

                // RBAC pre-checks
                if auth.role.is_some() {
                    if is_admin_query(&req.query)
                        && let Err(e) = require_admin(&auth)
                    {
                        let _ = tx
                            .send(Ok(proto::QueryChannelResponse {
                                request_id,
                                payload: Some(proto::query_channel_response::Payload::Error(
                                    proto::QueryError {
                                        code: 7, // PERMISSION_DENIED
                                        message: e.message().to_string(),
                                    },
                                )),
                            }))
                            .await;
                        continue;
                    }
                    if is_write_query(&req.query)
                        && let Err(e) = require_write(&auth)
                    {
                        let _ = tx
                            .send(Ok(proto::QueryChannelResponse {
                                request_id,
                                payload: Some(proto::query_channel_response::Payload::Error(
                                    proto::QueryError {
                                        code: 7, // PERMISSION_DENIED
                                        message: e.message().to_string(),
                                    },
                                )),
                            }))
                            .await;
                        continue;
                    }
                }

                // Execute query in blocking task (serial - we await before next iteration)
                let _ = tokio::task::spawn_blocking(move || {
                    let rt = tokio::runtime::Handle::current();
                    let t_start = Instant::now();
                    let store = &db_entry.store;
                    let query_trimmed = req.query.trim();

                    // Check for built-in procedures first
                    if let Some(proc_result) = handle_procedure(
                        query_trimmed,
                        store,
                        &registry,
                        &db_name,
                        &auth_catalog,
                        &auth,
                    ) {
                        match proc_result {
                            Ok(result) => {
                                send_channel_result(&rt, &tx, request_id, result, db_name, t_start);
                            }
                            Err(status) => {
                                let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                                    request_id,
                                    payload: Some(proto::query_channel_response::Payload::Error(
                                        proto::QueryError {
                                            code: 13, // INTERNAL
                                            message: status.message().to_string(),
                                        },
                                    )),
                                })));
                            }
                        }
                        return;
                    }

                    // Read-only enforcement
                    if read_only && is_write_query(&req.query) {
                        let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                            request_id,
                            payload: Some(proto::query_channel_response::Payload::Error(
                                proto::QueryError {
                                    code: 7,
                                    message: "writing in read access mode not allowed".into(),
                                },
                            )),
                        })));
                        return;
                    }

                    // an auto-commit transaction of its own, stopped by its time limit,
                    // TERMINATE TRANSACTIONS or the client closing the channel
                    let transaction = crate::statement::register(
                        &db_name,
                        auth.username.as_deref(),
                        "grpc",
                        &connection_id,
                        &client_address,
                        "",
                        BTreeMap::new(),
                    );
                    let running = crate::statement::start(
                        &transaction,
                        crate::statement::time_limit(client_limit(timeout_ms)).0,
                        crate::statement::principal(auth.username.as_deref(), auth.role),
                        &req.query,
                        Some(Arc::new(params.clone())),
                    );
                    let watcher = watch_disconnect(&rt, &tx, &running.ctx);
                    let _query = running.ctx.enter();

                    // Streaming path: auto-commit reads
                    if read_only || !is_write_query(&req.query) {
                        let mut read_tx = match store.begin_read() {
                            Ok(tx) => tx,
                            Err(e) => {
                                let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                                    request_id,
                                    payload: Some(proto::query_channel_response::Payload::Error(
                                        proto::QueryError {
                                            code: 8, // RESOURCE_EXHAUSTED
                                            message: e.to_string(),
                                        },
                                    )),
                                })));
                                return;
                            }
                        };

                        let bypass_cache = is_cache_bypass(query_trimmed);
                        let (stripped, stripped_params) =
                            traverse_cypher::strip_literals(&req.query)
                                .unwrap_or_else(|_| (req.query.clone(), vec![]));

                        let mut full_params = params;
                        for (i, val) in stripped_params.iter().enumerate() {
                            full_params.insert(format!("__sp{i}"), val.clone());
                        }

                        let index_gen =
                            traverse_core::tx::GraphView::index_catalog(&read_tx).generation();

                        let prepared: std::result::Result<
                            std::sync::Arc<traverse_cypher::PreparedQuery>,
                            traverse_cypher::error::CypherError,
                        > = if !bypass_cache {
                            if let Some(cached) = db_entry.plan_cache.get(&stripped, index_gen) {
                                Ok(cached)
                            } else {
                                traverse_cypher::prepare_query(&read_tx, &stripped, &full_params)
                                    .map(std::sync::Arc::new)
                            }
                        } else {
                            traverse_cypher::prepare_query(&read_tx, &stripped, &full_params)
                                .map(std::sync::Arc::new)
                        };

                        let prepared = match prepared {
                            Ok(p) => {
                                if !bypass_cache
                                    && db_entry.plan_cache.get(&stripped, index_gen).is_none()
                                {
                                    db_entry.plan_cache.insert(
                                        stripped,
                                        std::sync::Arc::clone(&p),
                                        index_gen,
                                    );
                                }
                                p
                            }
                            Err(e) => {
                                let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                                    request_id,
                                    payload: Some(proto::query_channel_response::Payload::Error(
                                        proto::QueryError {
                                            code: 3,
                                            message: format!("{e}"),
                                        },
                                    )),
                                })));
                                watcher.abort();
                                return;
                            }
                        };

                        match traverse_cypher::execute_prepared_streaming(
                            &mut read_tx,
                            &prepared,
                            &full_params,
                        ) {
                            Ok(traverse_cypher::executor::QueryExecution::Streaming(stream)) => {
                                send_channel_result_streaming(
                                    &rt, &tx, request_id, stream, db_name, t_start,
                                );
                            }
                            Ok(traverse_cypher::executor::QueryExecution::Materialized(result)) => {
                                send_channel_result(&rt, &tx, request_id, result, db_name, t_start);
                            }
                            Err(e) => {
                                let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                                    request_id,
                                    payload: Some(proto::query_channel_response::Payload::Error(
                                        proto::QueryError {
                                            code: 3,
                                            message: format!("{e}"),
                                        },
                                    )),
                                })));
                            }
                        }
                    } else {
                        // Auto-commit write
                        match execute_auto_commit(
                            &db_entry,
                            &req.query,
                            &params,
                            read_only,
                            &running.ctx,
                        ) {
                            Ok((result, _bookmark)) => {
                                send_channel_result(&rt, &tx, request_id, result, db_name, t_start);
                            }
                            Err(status) => {
                                let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                                    request_id,
                                    payload: Some(proto::query_channel_response::Payload::Error(
                                        proto::QueryError {
                                            code: 13,
                                            message: status.message().to_string(),
                                        },
                                    )),
                                })));
                            }
                        }
                    }

                    watcher.abort();
                })
                .await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
