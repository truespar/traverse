//! RUN: planning (through the plan cache) and executing a query, auto-commit or in an explicit
//! transaction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::debug;
use traverse_core::tx::timestamp::HybridTimestamp;
use traverse_cypher::PreparedQuery;
use traverse_cypher::executor::{CypherResult, QueryStats};
use traverse_cypher::types::CypherValue;

use crate::bolt_type::{cypher_value_to_packstream, packstream_params_to_cypher};
use crate::error::ServerError;
use crate::message::ServerMessage;
use crate::notification::{self, Severity};
use crate::packstream::Value;
use crate::registry::DatabaseEntry;
use crate::{is_cache_bypass, is_write_query};

use super::tx::{metadata_map, parse_bookmark};
use super::{
    Connection, DeferredStatement, PendingResult, PendingResultKind, State, perf_trace_enabled,
};

impl Connection {
    pub(super) fn handle_run(
        &mut self,
        query: String,
        parameters: Vec<(String, Value)>,
        extra: Vec<(String, Value)>,
    ) -> Vec<ServerMessage> {
        let trace = perf_trace_enabled();
        let t_entry = if trace { Some(Instant::now()) } else { None };
        let mut t_phases: Vec<(&'static str, Duration)> = if trace {
            Vec::with_capacity(16)
        } else {
            Vec::new()
        };
        let phase = |label: &'static str,
                     mark: &mut Instant,
                     phases: &mut Vec<(&'static str, Duration)>| {
            let now = Instant::now();
            phases.push((label, now - *mark));
            *mark = now;
        };
        let mut t_mark = t_entry.unwrap_or_else(Instant::now);

        // RUN allowed in Ready, TxReady, Streaming and TxStreaming (pipelining)
        if self.state != State::Ready
            && self.state != State::TxReady
            && self.state != State::Streaming
            && self.state != State::TxStreaming
        {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected RUN")];
        }

        // save connection-level notification settings (per-query overrides are temporary)
        let saved_min_sev = self.notification_min_severity;
        let saved_disabled = self.notification_disabled_categories.clone();

        // extras: db, tx_timeout, imp_user, bookmarks, notifications
        if let Some(err) = self.parse_run_extras(&extra) {
            self.notification_min_severity = saved_min_sev;
            self.notification_disabled_categories = saved_disabled;
            return err;
        }
        if trace {
            phase("extras", &mut t_mark, &mut t_phases);
        }

        // built-in procedure calls
        let query_trimmed = query.trim();

        if let Some(proc_result) = self.handle_procedure(query_trimmed) {
            // restore notification settings (no per-query analysis for procedures)
            self.notification_min_severity = saved_min_sev;
            self.notification_disabled_categories = saved_disabled;
            return proc_result;
        }
        if trace {
            phase("procedure_check", &mut t_mark, &mut t_phases);
        }

        // classify write once
        let is_write = is_write_query(&query);
        if trace {
            phase("is_write", &mut t_mark, &mut t_phases);
        }

        // mode:"r": no writes in read-only transactions
        if self.read_only_tx && is_write {
            self.notification_min_severity = saved_min_sev;
            self.notification_disabled_categories = saved_disabled;
            self.enter_failed();
            return vec![self.failure(
                "Neo.ClientError.Statement.AccessMode",
                "writing in read access mode not allowed",
            )];
        }

        // read-only databases reject writes
        if self.current_entry.read_only && is_write {
            self.notification_min_severity = saved_min_sev;
            self.notification_disabled_categories = saved_disabled;
            self.enter_failed();
            return vec![self.failure(
                "Neo.ClientError.Statement.AccessMode",
                &format!("database '{}' is read-only", self.current_db_name),
            )];
        }

        // shared: the engine reads them, SHOW TRANSACTIONS shows them
        let params = match packstream_params_to_cypher(&parameters) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                self.enter_failed();
                return vec![self.failure("Neo.ClientError.Statement.TypeError", &e.to_string())];
            }
        };
        if trace {
            phase("params_to_cypher", &mut t_mark, &mut t_phases);
        }

        // query notifications (performance hints, unknown labels, ...)
        let notifications = notification::analyze_query_notifications(
            &query,
            &self.current_entry.store,
            self.notification_min_severity,
            &self.notification_disabled_categories,
        );
        self.pending_notifications = notifications;

        // restore connection-level notification settings after per-query analysis
        self.notification_min_severity = saved_min_sev;
        self.notification_disabled_categories = saved_disabled;
        if trace {
            phase("notifications", &mut t_mark, &mut t_phases);
        }

        let run_start = Instant::now();
        let is_in_tx = self.state == State::TxReady || self.state == State::TxStreaming;

        // auto-commit reads (no explicit tx, no write) defer execution to PULL and stream
        let can_defer = !is_in_tx && !is_write;

        if can_defer {
            // plan cache first (bypassed for DDL/CALL)
            let bypass_cache = is_cache_bypass(&query);
            let store = Arc::clone(&self.current_entry.store);
            let index_gen = {
                let view = store.unregistered_read();
                traverse_core::tx::GraphView::index_catalog(&view).generation()
            };
            if trace {
                phase("index_gen", &mut t_mark, &mut t_phases);
            }

            // strip literals for the cache key
            let stripped_arc = traverse_cypher::strip_literals_cached(&query)
                .unwrap_or_else(|_| std::sync::Arc::new((query.clone(), vec![])));
            let (stripped, stripped_params) = (&stripped_arc.0, &stripped_arc.1);
            if trace {
                phase("strip_literals", &mut t_mark, &mut t_phases);
            }

            // merge the stripped params into the user params
            let mut full_params = (*params).clone();
            for (i, val) in stripped_params.iter().enumerate() {
                full_params.insert(format!("__sp{i}"), val.clone());
            }
            if trace {
                phase("merge_params", &mut t_mark, &mut t_phases);
            }

            let mut was_cache_miss = false;
            let (prepared, parsing_time, planning_time): (
                std::result::Result<Arc<PreparedQuery>, traverse_cypher::error::CypherError>,
                Duration,
                Duration,
            ) = if !bypass_cache {
                if let Some(cached) = self.current_entry.plan_cache.get(stripped, index_gen) {
                    if trace {
                        phase("cache_hit", &mut t_mark, &mut t_phases);
                    }
                    (Ok(cached), Duration::ZERO, Duration::ZERO)
                } else {
                    was_cache_miss = true;
                    let view = store.unregistered_read();
                    let r =
                        match traverse_cypher::prepare_query_timed(&view, stripped, &full_params) {
                            Ok((pq, pt, plt)) => (Ok(Arc::new(pq)), pt, plt),
                            Err(e) => (Err(e), Duration::ZERO, Duration::ZERO),
                        };
                    if trace {
                        phase("cache_miss_prepare", &mut t_mark, &mut t_phases);
                    }
                    r
                }
            } else {
                let view = store.unregistered_read();
                let r = match traverse_cypher::prepare_query_timed(&view, stripped, &full_params) {
                    Ok((pq, pt, plt)) => (Ok(Arc::new(pq)), pt, plt),
                    Err(e) => (Err(e), Duration::ZERO, Duration::ZERO),
                };
                if trace {
                    phase("bypass_prepare", &mut t_mark, &mut t_phases);
                }
                r
            };

            match prepared {
                Ok(prepared) => {
                    // cache on a miss (not DDL/CALL); a hit already bumped recency under the read
                    // lock in get(), so the write lock is only for insert
                    if !bypass_cache && was_cache_miss {
                        self.current_entry.plan_cache.insert(
                            stripped.clone(),
                            Arc::clone(&prepared),
                            index_gen,
                        );
                    }
                    if trace {
                        phase("cache_update", &mut t_mark, &mut t_phases);
                    }

                    let qid = self.next_qid;
                    self.next_qid += 1;
                    self.last_qid = qid;

                    let columns_value = Value::List(
                        prepared
                            .columns
                            .iter()
                            .map(|c| Value::String(c.clone()))
                            .collect(),
                    );
                    let query_type = prepared.query_type;

                    // EXPLAIN: no deferral, return the plan now
                    if prepared.mode == traverse_cypher::ast::QueryMode::Explain {
                        let plan_cv =
                            traverse_cypher::executor::plan_to_cypher_value(&prepared.plan);
                        self.pending_results.insert(
                            qid,
                            PendingResult {
                                kind: PendingResultKind::Materialized {
                                    rows: vec![],
                                    cursor: 0,
                                },
                                stats: QueryStats::default(),
                                query_type,
                                plan: Some(plan_cv),
                                run_start,
                                parsing_time,
                                planning_time,
                                execution_time: Duration::ZERO,
                            },
                        );
                    } else {
                        // store the prepared query for deferred execution on PULL; the
                        // auto-commit transaction is listed from now until its rows are out
                        let plan_cv = if prepared.mode == traverse_cypher::ast::QueryMode::Profile {
                            Some(traverse_cypher::executor::plan_to_cypher_value(
                                &prepared.plan,
                            ))
                        } else {
                            None
                        };
                        let statement = DeferredStatement {
                            transaction: self.register_transaction(),
                            time_limit: crate::statement::time_limit(self.tx_timeout),
                            text: query.clone(),
                            params: Arc::clone(&params),
                        };
                        self.tx_timeout = None;
                        self.pending_results.insert(
                            qid,
                            PendingResult {
                                kind: PendingResultKind::Deferred {
                                    prepared: Arc::clone(&prepared),
                                    params: full_params.clone(),
                                    statement,
                                },
                                stats: QueryStats::default(),
                                query_type,
                                plan: plan_cv,
                                run_start,
                                parsing_time,
                                planning_time,
                                execution_time: Duration::ZERO,
                            },
                        );
                    }

                    self.state = if is_in_tx {
                        State::TxStreaming
                    } else {
                        State::Streaming
                    };

                    let t_first = run_start.elapsed().as_millis() as i64;
                    debug!(plan_us = run_start.elapsed().as_micros() as u64, "run");
                    let mut metadata = vec![
                        ("fields".into(), columns_value),
                        ("t_first".into(), Value::Integer(t_first)),
                        (
                            "type".into(),
                            Value::String(query_type.as_bolt_str().into()),
                        ),
                        ("qid".into(), Value::Integer(qid)),
                    ];
                    metadata.push(("db".into(), Value::String(self.current_db_name.clone())));

                    // plan in the metadata for EXPLAIN
                    if prepared.mode == traverse_cypher::ast::QueryMode::Explain
                        && let Some(ref plan) =
                            self.pending_results.get(&qid).and_then(|p| p.plan.clone())
                    {
                        metadata.push((
                            "plan".into(),
                            cypher_value_to_packstream(plan, self.version.major),
                        ));
                    }
                    if trace {
                        phase("metadata", &mut t_mark, &mut t_phases);
                    }

                    if let Some(t0) = t_entry {
                        let total = t0.elapsed();
                        let phases_str = t_phases
                            .iter()
                            .map(|(k, d)| format!("{}={}ns", k, d.as_nanos()))
                            .collect::<Vec<_>>()
                            .join(" ");
                        eprintln!(
                            "[perf] RUN total={}ns parsing={}ns planning={}ns | {}",
                            total.as_nanos(),
                            parsing_time.as_nanos(),
                            planning_time.as_nanos(),
                            phases_str,
                        );
                    }

                    vec![ServerMessage::Success { metadata }]
                }
                Err(e) => {
                    self.enter_failed();
                    vec![self.error_message(&ServerError::from(e))]
                }
            }
        } else {
            // execute now (writes, transactions, ...)
            let result = self.execute_query(&query, &params, is_write);
            let elapsed = run_start.elapsed();
            let t_first = elapsed.as_millis() as i64;

            // slow query logging
            if let Some(threshold) = crate::config::slow_query_threshold()
                && elapsed > threshold
            {
                let q = crate::truncate_str(&query, 200);
                let suffix = if query.len() > 200 { "..." } else { "" };
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    "slow query ({}ms): {q}{suffix}",
                    elapsed.as_millis(),
                );
            }

            match result {
                Ok(cypher_result) => {
                    // DDL (CREATE/DROP INDEX) invalidates the plan cache
                    if cypher_result.stats.indexes_added != 0
                        || cypher_result.stats.indexes_removed != 0
                        || cypher_result.stats.constraints_added != 0
                        || cypher_result.stats.constraints_removed != 0
                    {
                        self.current_entry.plan_cache.clear();
                    }

                    let columns_value = Value::List(
                        cypher_result
                            .columns
                            .iter()
                            .map(|c| Value::String(c.clone()))
                            .collect(),
                    );

                    let query_type = cypher_result.query_type;
                    let stats = cypher_result.stats;
                    let plan_cv = cypher_result.plan;

                    let qid = self.next_qid;
                    self.next_qid += 1;
                    self.last_qid = qid;

                    // auto-commit writes set the bookmark in execute_query
                    self.pending_results.insert(
                        qid,
                        PendingResult {
                            kind: PendingResultKind::Materialized {
                                rows: cypher_result.rows,
                                cursor: 0,
                            },
                            stats,
                            query_type,
                            plan: plan_cv,
                            run_start,
                            parsing_time: Duration::ZERO,
                            planning_time: Duration::ZERO,
                            execution_time: elapsed,
                        },
                    );

                    self.state = if is_in_tx {
                        State::TxStreaming
                    } else {
                        State::Streaming
                    };

                    let mut metadata = vec![
                        ("fields".into(), columns_value),
                        ("t_first".into(), Value::Integer(t_first)),
                        (
                            "type".into(),
                            Value::String(query_type.as_bolt_str().into()),
                        ),
                        ("qid".into(), Value::Integer(qid)),
                    ];
                    metadata.push(("db".into(), Value::String(self.current_db_name.clone())));

                    // plan in the metadata for EXPLAIN/PROFILE
                    if let Some(ref plan) =
                        self.pending_results.get(&qid).and_then(|p| p.plan.clone())
                    {
                        metadata.push((
                            "plan".into(),
                            cypher_value_to_packstream(plan, self.version.major),
                        ));
                    }

                    vec![ServerMessage::Success { metadata }]
                }
                // a RESET stopped it: the client asked for that, answer IGNORED
                Err(ServerError::Interrupted {
                    reason: traverse_cypher::query::Interrupt::Cancelled,
                    ..
                }) if self.interrupts.reset_pending() => vec![ServerMessage::Ignored],
                Err(e) => {
                    self.enter_failed();
                    vec![self.error_message(&e)]
                }
            }
        }
    }

    /// Parses RUN extras: db, tx_timeout (auto-commit only), imp_user, tx_metadata, notification
    /// settings. `Some(failure_response)` for an invalid db name.
    fn parse_run_extras(&mut self, extra: &[(String, Value)]) -> Option<Vec<ServerMessage>> {
        for (k, v) in extra {
            match k.as_str() {
                "db" => {
                    if let Some(err) = self.resolve_database(v) {
                        return Some(err);
                    }
                }
                "tx_timeout" => {
                    // auto-commit only (not inside an explicit tx)
                    if self.state == State::Ready
                        && let Some(ms) = v.as_i64()
                        && ms > 0
                    {
                        self.tx_timeout = Some(Duration::from_millis(ms as u64));
                    }
                }
                "imp_user" => {
                    if let Some(user) = v.as_str() {
                        debug!("impersonating user: {user}");
                    }
                }
                "bookmarks" => {
                    if let Value::List(bms) = v {
                        let bookmarks: Vec<&str> = bms.iter().filter_map(Value::as_str).collect();
                        for bk in &bookmarks {
                            if !bk.starts_with("FB:") {
                                debug!("ignoring non-traverse bookmark: {bk}");
                                continue;
                            }
                            if let Some(bk_ts) = parse_bookmark(bk) {
                                let store_ts = self.current_entry.store.visible_ts();
                                if bk_ts > store_ts && store_ts != HybridTimestamp::ZERO {
                                    debug!(
                                        "bookmark {bk} (ts={bk_ts:?}) is ahead of store (ts={store_ts:?})"
                                    );
                                }
                                debug!("bookmark validated: {bk}");
                            } else {
                                debug!("malformed bookmark ignored: {bk}");
                            }
                        }
                    }
                }
                "mode" => {
                    // auto-commit queries (state == Ready)
                    if self.state == State::Ready
                        && let Some(mode) = v.as_str()
                    {
                        debug!("auto-commit access mode: {mode}");
                        self.read_only_tx = mode == "r";
                    }
                }
                "tx_metadata" => {
                    // auto-commit only; an explicit transaction took its metadata at BEGIN
                    if self.state == State::Ready {
                        self.tx_metadata = metadata_map(v);
                    }
                }
                "notifications_minimum_severity" => {
                    if let Some(sev) = v.as_str() {
                        self.notification_min_severity = Severity::parse(sev);
                        debug!("per-query notification min severity: {sev}");
                    }
                }
                "notifications_disabled_categories" | "notifications_disabled_classifications" => {
                    if let Value::List(cats) = v {
                        self.notification_disabled_categories = cats
                            .iter()
                            .filter_map(|c| c.as_str().map(|s| s.to_uppercase()))
                            .collect();
                        debug!(
                            "per-query disabled notification categories: {:?}",
                            self.notification_disabled_categories
                        );
                    }
                }
                other => {
                    debug!("ignoring unknown RUN extra: {other}");
                }
            }
        }
        None
    }

    /// Runs a statement now (writes, statements in an explicit transaction) under its own context:
    /// listed in its transaction, stopped by the time limit, a RESET, a closed connection or
    /// `TERMINATE TRANSACTIONS`.
    fn execute_query(
        &mut self,
        query: &str,
        params: &Arc<HashMap<String, CypherValue>>,
        is_write: bool,
    ) -> std::result::Result<CypherResult, ServerError> {
        // an auto-commit statement is a transaction of its own
        let auto = self
            .explicit_tx
            .is_none()
            .then(|| self.register_transaction());
        let (limit, client_limit) = match &self.explicit_tx {
            Some(tx) => {
                if tx.timed_out() {
                    self.client_time_limit = true;
                    return Err(ServerError::Interrupted {
                        reason: traverse_cypher::query::Interrupt::TimedOut,
                        message: "the transaction timed out".to_string(),
                    });
                }
                crate::statement::time_limit(tx.remaining())
            }
            None => crate::statement::time_limit(self.tx_timeout),
        };
        self.client_time_limit = client_limit;
        let registration = match (&auto, &self.explicit_tx) {
            (Some(t), _) => t,
            (None, Some(tx)) => &tx.registration,
            (None, None) => unreachable!("an auto-commit statement registers itself"),
        };
        let running = crate::statement::start(
            registration,
            limit,
            self.principal(),
            query,
            Some(Arc::clone(params)),
        );
        self.interrupts.set_running(Some(Arc::clone(&running.ctx)));
        let result = {
            let _query = running.ctx.enter();
            self.execute_query_inner(query, params, is_write, &running.ctx)
        };
        self.interrupts.set_running(None);
        if auto.is_some() {
            // clear the auto-commit tx_timeout
            self.tx_timeout = None;
        }
        result
    }

    fn execute_query_inner(
        &mut self,
        query: &str,
        params: &HashMap<String, CypherValue>,
        is_write: bool,
        ctx: &traverse_cypher::query::QueryContext,
    ) -> std::result::Result<CypherResult, ServerError> {
        // per-query `USE <db>` routing (Cypher multi-database syntax): the parser tolerates the
        // prefix, but databases live in the SERVER, so routing happens here, for this query only,
        // without switching the session's database
        let routed: Option<Arc<DatabaseEntry>> = match use_prefix_target(query) {
            Some(name) => {
                let resolved = self.registry.resolve_alias(&name);
                if resolved == self.current_db_name {
                    None
                } else if self.explicit_tx.is_some() {
                    return Err(ServerError::Graph(
                        "USE cannot switch databases inside an open transaction".to_string(),
                    ));
                } else {
                    match self.registry.get(&resolved) {
                        Some(entry) => Some(entry),
                        None => {
                            return Err(ServerError::Graph(format!("database '{name}' not found")));
                        }
                    }
                }
            }
            None => None,
        };
        let entry = routed.as_ref().unwrap_or(&self.current_entry).clone();

        if let Some(ref mut tx) = self.explicit_tx {
            traverse_cypher::execute_with_params(&mut tx.tx, query, params)
                .map_err(ServerError::from)
        } else {
            // auto-commit: read-only or write?
            if is_write {
                // the write path is plan-cached too (lexing + parsing per execution adds up, and
                // the parser grows with every language feature); same key discipline as reads:
                // stripped text + index generation, DDL and non-allowlisted CALLs bypass
                let mut write_tx = crate::statement::begin_write(&entry.store, ctx)?;
                let result = if !crate::is_cache_bypass(query) {
                    let stripped_arc = traverse_cypher::strip_literals_cached(query)
                        .unwrap_or_else(|_| std::sync::Arc::new((query.to_string(), vec![])));
                    let (stripped, stripped_params) = (&stripped_arc.0, &stripped_arc.1);
                    let mut full_params = params.clone();
                    for (i, val) in stripped_params.iter().enumerate() {
                        full_params.insert(format!("__sp{i}"), val.clone());
                    }
                    let index_gen =
                        traverse_core::tx::GraphView::index_catalog(&write_tx).generation();
                    let cached = entry.plan_cache.get(stripped, index_gen);
                    let prepared = match cached {
                        Some(p) => Ok(p),
                        None => {
                            match traverse_cypher::prepare_query_timed(
                                &write_tx,
                                stripped,
                                &full_params,
                            ) {
                                Ok((pq, _, _)) => {
                                    let arc = Arc::new(pq);
                                    entry.plan_cache.insert(
                                        stripped.clone(),
                                        Arc::clone(&arc),
                                        index_gen,
                                    );
                                    Ok(arc)
                                }
                                Err(e) => Err(e),
                            }
                        }
                    };
                    match prepared {
                        Ok(p) => traverse_cypher::execute_prepared(&mut write_tx, &p, &full_params)
                            .map_err(ServerError::from)?,
                        Err(e) => return Err(ServerError::from(e)),
                    }
                } else {
                    traverse_cypher::execute_with_params(&mut write_tx, query, params)
                        .map_err(ServerError::from)?
                };

                let ts = write_tx.commit().map_err(ServerError::from)?;
                entry.mark_dirty();
                self.last_bookmark = Some(format!("FB:{}:{}", ts.physical, ts.logical));
                // periodic GC every 1000 auto-commit writes: reclaim old deltas and last_modified
                // entries, so sustained bulk imports without concurrent readers don't grow
                // unbounded
                self.write_count += 1;
                if self.write_count.is_multiple_of(1000) {
                    entry.store.gc();
                    // reclaim GC'd delta chains on this thread (allocated and freed on the same
                    // Bolt thread)
                    // SAFETY: mi_collect has no preconditions; it only walks this thread's heap
                    #[cfg(feature = "native-alloc")]
                    unsafe {
                        libmimalloc_sys::mi_collect(true);
                    }
                }
                Ok(result)
            } else {
                let mut read_tx = entry.store.begin_read().map_err(ServerError::from)?;
                traverse_cypher::execute_with_params(&mut read_tx, query, params)
                    .map_err(ServerError::from)
            }
        }
    }
}

/// Database name from a leading `USE <name>` prefix (after any `CYPHER ...` / EXPLAIN / PROFILE
/// preludes), if any. Dotted names are joined verbatim, backticked segments unwrapped.
fn use_prefix_target(query: &str) -> Option<String> {
    let mut rest = query.trim_start();
    // skip CYPHER prelude tokens and mode keywords, conservatively
    loop {
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with("cypher") {
            // skip "CYPHER" and its version/option tokens up to the next keyword-looking token that
            // isn't key=value or a number
            let mut it = rest.splitn(2, char::is_whitespace);
            let _ = it.next();
            rest = it.next().unwrap_or("").trim_start();
            while let Some(tok) = rest.split_whitespace().next() {
                if tok.contains('=') || tok.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    rest = rest[tok.len()..].trim_start();
                } else {
                    break;
                }
            }
            continue;
        }
        if lower.starts_with("explain") || lower.starts_with("profile") {
            let mut it = rest.splitn(2, char::is_whitespace);
            let _ = it.next();
            rest = it.next().unwrap_or("").trim_start();
            continue;
        }
        break;
    }
    let lower = rest.to_ascii_lowercase();
    if !lower.starts_with("use") {
        return None;
    }
    let after = &rest[3..];
    if !after.starts_with(char::is_whitespace) {
        return None; // e.g. a variable named `used...`
    }
    let name_part = after.split_whitespace().next()?;
    let name = name_part.trim_matches('`');
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}
