//! PULL and DISCARD: streaming result records and the completion metadata.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tracing::debug;

use crate::bolt_type::cypher_value_to_packstream;
use crate::error::{Result, ServerError};
use crate::framing;
use crate::message::{self, ServerMessage};
use crate::packstream::{self, Value};

use super::{
    Connection, PendingResult, PendingResultKind, State, notification_to_packstream,
    perf_trace_enabled,
};

impl Connection {
    /// Streams PULL results straight to the wire, no bulk `Vec<ServerMessage>`. Lazy results
    /// (morsel pipeline output) go Frame -> CypherValue -> PackStream one row at a time, each written
    /// immediately, so only one row's `CypherValue` / `PackStream::Value` exists at once.
    /// Materialized results are written record by record too.
    pub(super) async fn handle_pull_streaming<W: AsyncWrite + Unpin>(
        &mut self,
        extra: Vec<(String, Value)>,
        writer: &mut BufWriter<W>,
    ) -> Result<()> {
        let trace = perf_trace_enabled();
        let t_entry = if trace { Some(Instant::now()) } else { None };
        let mut t_phases: Vec<(&'static str, Duration)> = if trace {
            Vec::with_capacity(8)
        } else {
            Vec::new()
        };

        if self.state != State::Streaming && self.state != State::TxStreaming {
            // Bolt: after a FAILURE, pipelined messages are IGNORED until RESET. Drivers pipeline
            // RUN+PULL, so answering the PULL with a second FAILURE ("unexpected PULL") would hide
            // the real error from RUN (e.g. a syntax error). handle_message() does the same on the
            // non-streaming path.
            if self.state == State::Failed {
                self.write_msg(writer, ServerMessage::Ignored).await?;
                return Ok(());
            }
            self.write_msg(
                writer,
                self.failure("Neo.ClientError.Request.Invalid", "unexpected PULL"),
            )
            .await?;
            return Ok(());
        }

        let n = extra
            .iter()
            .find(|(k, _)| k == "n")
            .and_then(|(_, v)| v.as_i64())
            .unwrap_or(-1);

        let qid = extra
            .iter()
            .find(|(k, _)| k == "qid")
            .and_then(|(_, v)| v.as_i64())
            .unwrap_or(-1);
        let effective_qid = if qid == -1 { self.last_qid } else { qid };

        // -- resolve deferred results -------------------------------------
        let mut exec_us: u64 = 0;
        if let Some(pending) = self.pending_results.get(&effective_qid)
            && matches!(pending.kind, PendingResultKind::Deferred { .. })
            && let Some(mut pending) = self.pending_results.remove(&effective_qid)
        {
            // take the kind out so `pending` isn't partially moved (needed later for
            // metadata)
            let taken_kind = std::mem::replace(
                &mut pending.kind,
                PendingResultKind::Materialized {
                    rows: Vec::new(),
                    cursor: 0,
                },
            );
            if let PendingResultKind::Deferred {
                prepared,
                params,
                statement,
            } = taken_kind
            {
                let exec_start = Instant::now();
                let t_phase_start = if trace { Some(Instant::now()) } else { None };
                let store = Arc::clone(&self.current_entry.store);
                let mut read_tx = match store.begin_read() {
                    Ok(tx) => tx,
                    Err(e) => {
                        self.enter_failed();
                        self.write_msg(
                            writer,
                            self.failure(
                                "Neo.TransientError.General.DatabaseError",
                                &e.to_string(),
                            ),
                        )
                        .await?;
                        return Ok(());
                    }
                };
                if let Some(t) = t_phase_start {
                    t_phases.push(("begin_read", t.elapsed()));
                }

                // the statement's context carries its limits and parameters to whichever thread
                // pulls rows; the reader can stop it while it streams
                let running = crate::statement::start(
                    &statement.transaction,
                    statement.time_limit.0,
                    self.principal(),
                    &statement.text,
                    Some(Arc::clone(&statement.params)),
                );
                self.client_time_limit = statement.time_limit.1;
                self.interrupts.set_running(Some(Arc::clone(&running.ctx)));
                let t_phase_start = if trace { Some(Instant::now()) } else { None };
                let deferred_result = crate::statement::blocking(|| {
                    let _query = running.ctx.enter();
                    traverse_cypher::execute_prepared_lazy(&mut read_tx, &prepared, &params)
                });
                if let Some(t) = t_phase_start {
                    t_phases.push(("execute_prepared_lazy", t.elapsed()));
                }

                match deferred_result {
                    Ok(lazy_result) => {
                        let exec_elapsed = exec_start.elapsed();
                        exec_us = exec_elapsed.as_micros() as u64;

                        match lazy_result {
                            traverse_cypher::executor::QueryResultLazy::Lazy(mut lazy) => {
                                let total = lazy.total();
                                let pull_all = n < 0 || (total > 0 && n as usize >= total);

                                if pull_all {
                                    // -- PULL all from the lazy source: zero-alloc batched
                                    // streaming --
                                    pending.stats = lazy.stats.clone();
                                    pending.query_type = lazy.query_type;
                                    pending.execution_time = exec_elapsed;

                                    let bolt_major = self.version.major;
                                    let ser_start = Instant::now();
                                    let mut sent = 0usize;
                                    let slot_indices = lazy.column_slot_indices();
                                    let mut batch_buf = std::mem::take(&mut self.batch_buf);
                                    batch_buf.clear();
                                    batch_buf.reserve(65536);
                                    let t_phase_start =
                                        if trace { Some(Instant::now()) } else { None };
                                    let mut t_frame_iter = Duration::ZERO;
                                    let mut t_encode = Duration::ZERO;
                                    let mut t_chunk = Duration::ZERO;
                                    let mut t_write = Duration::ZERO;

                                    // rows go out in ~60 KB batches: each is produced with the
                                    // statement current on this thread, then written; the task
                                    // may resume on another thread for the next one
                                    loop {
                                        let mut failure = None;
                                        let done = crate::statement::blocking(|| {
                                            let _query = lazy.enter();
                                            loop {
                                                let t =
                                                    if trace { Some(Instant::now()) } else { None };
                                                let next = lazy.next_frame(&read_tx);
                                                if let Some(t) = t {
                                                    t_frame_iter += t.elapsed();
                                                }
                                                let frame = match next {
                                                    Ok(Some(frame)) => frame,
                                                    Ok(None) => break true,
                                                    Err(e) => {
                                                        failure = Some(e);
                                                        break true;
                                                    }
                                                };
                                                // encode the record into a temp buffer, then
                                                // chunk it into the batch buffer; one record can
                                                // exceed the 65535-byte Bolt chunk limit (e.g.
                                                // 500 paths with full node props)
                                                let t_enc =
                                                    if trace { Some(Instant::now()) } else { None };
                                                let record_start = batch_buf.len();
                                                packstream::encode::encode_record_from_slots(
                                                    &mut batch_buf,
                                                    &frame,
                                                    &slot_indices,
                                                    &read_tx,
                                                    bolt_major,
                                                );
                                                let record_bytes =
                                                    batch_buf.split_off(record_start);
                                                if let Some(t) = t_enc {
                                                    t_encode += t.elapsed();
                                                }

                                                // record as one Bolt message (multi-chunk above
                                                // 65535 bytes, then the 0x0000 terminator)
                                                let t_ch =
                                                    if trace { Some(Instant::now()) } else { None };
                                                let mut offset = 0;
                                                while offset < record_bytes.len() {
                                                    let remaining = record_bytes.len() - offset;
                                                    let chunk_size = remaining.min(0xFFFF);
                                                    batch_buf.extend_from_slice(
                                                        &(chunk_size as u16).to_be_bytes(),
                                                    );
                                                    batch_buf.extend_from_slice(
                                                        &record_bytes[offset..offset + chunk_size],
                                                    );
                                                    offset += chunk_size;
                                                }
                                                // chunk terminator
                                                batch_buf.extend_from_slice(&[0u8; 2]);
                                                if let Some(t) = t_ch {
                                                    t_chunk += t.elapsed();
                                                }

                                                sent += 1;
                                                if batch_buf.len() >= 60_000 {
                                                    break false;
                                                }
                                            }
                                        });
                                        if let Some(e) = failure {
                                            // a mid-stream error (timeout, cancel, memory
                                            // ceiling, lazy projection type error) may come
                                            // after some RECORDs are on the wire; send a proper
                                            // FAILURE (the driver discards the partial records
                                            // and reports the error) instead of letting `?` tear
                                            // down the TCP connection, which the client would see
                                            // as a broken pipe with the real error lost
                                            batch_buf.clear();
                                            self.batch_buf = std::mem::take(&mut batch_buf);
                                            self.interrupts.set_running(None);
                                            self.fail_statement(writer, ServerError::from(e))
                                                .await?;
                                            return Ok(());
                                        }
                                        if !batch_buf.is_empty() {
                                            let t_w =
                                                if trace { Some(Instant::now()) } else { None };
                                            writer.write_all(&batch_buf).await?;
                                            batch_buf.clear();
                                            if let Some(t) = t_w {
                                                t_write += t.elapsed();
                                            }
                                        }
                                        if done {
                                            break;
                                        }
                                    }
                                    self.batch_buf = batch_buf;
                                    self.interrupts.set_running(None);
                                    drop(lazy); // free remaining frame memory
                                    drop(running);
                                    drop(statement);
                                    if let Some(t) = t_phase_start {
                                        t_phases.push(("frame_loop_total", t.elapsed()));
                                        t_phases.push(("frame_iter", t_frame_iter));
                                        t_phases.push(("encode", t_encode));
                                        t_phases.push(("chunk", t_chunk));
                                        t_phases.push(("write", t_write));
                                    }

                                    let ser_us = ser_start.elapsed().as_micros() as u64;
                                    let total_us = pending.run_start.elapsed().as_micros() as u64;
                                    debug!(rows = sent, exec_us, ser_us, total_us, "pull");
                                    if trace && let Some(t0) = t_entry {
                                        let total = t0.elapsed();
                                        let phases_str = t_phases
                                            .iter()
                                            .map(|(k, d)| format!("{}={}ns", k, d.as_nanos()))
                                            .collect::<Vec<_>>()
                                            .join(" ");
                                        eprintln!(
                                            "[perf] PULL(lazy) total={}ns rows={} exec={}ns ser={}ns | {}",
                                            total.as_nanos(),
                                            sent,
                                            exec_elapsed.as_nanos(),
                                            ser_start.elapsed().as_nanos(),
                                            phases_str,
                                        );
                                    }

                                    // trailing Success
                                    let success = self.build_pull_complete_metadata(&pending);
                                    let msg = ServerMessage::Success { metadata: success };
                                    let value = message::encode_server_message(msg);
                                    let mut encode_buf = std::mem::take(&mut self.encode_buf);
                                    encode_buf.clear();
                                    packstream::encode_value(&mut encode_buf, &value);
                                    framing::write_message(writer, &encode_buf).await?;
                                    self.encode_buf = encode_buf;

                                    self.transition_after_pull_complete(effective_qid);
                                    // return freed frame/morsel memory to the OS after
                                    // heavy queries: frames are allocated on rayon workers
                                    // but freed on this Bolt thread, and mimalloc keeps
                                    // cross-thread frees in per-thread delayed-free lists;
                                    // mi_collect here reclaims this thread's heap,
                                    // rayon::broadcast drains the workers' lists so their
                                    // segments can be returned
                                    if sent >= 10_000 {
                                        crate::registry::collect_allocator_caches();
                                    }
                                    return Ok(());
                                } else {
                                    // partial PULL: materialize for cursor access; a
                                    // mid-materialization error (timeout / memory ceiling)
                                    // is a FAILURE, not a dropped connection
                                    let materialized = crate::statement::blocking(|| {
                                        lazy.into_materialized(&read_tx)
                                    });
                                    self.interrupts.set_running(None);
                                    let result = match materialized {
                                        Ok(r) => r,
                                        Err(e) => {
                                            self.fail_statement(writer, ServerError::from(e))
                                                .await?;
                                            return Ok(());
                                        }
                                    };
                                    pending.stats = result.stats;
                                    pending.query_type = result.query_type;
                                    pending.execution_time = exec_elapsed;
                                    if result.plan.is_some() {
                                        pending.plan = result.plan;
                                    }
                                    pending.kind = PendingResultKind::Materialized {
                                        rows: result.rows,
                                        cursor: 0,
                                    };
                                    self.pending_results.insert(effective_qid, pending);
                                }
                            }
                            traverse_cypher::executor::QueryResultLazy::Materialized(result) => {
                                self.interrupts.set_running(None);
                                pending.stats = result.stats;
                                pending.query_type = result.query_type;
                                pending.execution_time = exec_elapsed;
                                if result.plan.is_some() {
                                    pending.plan = result.plan;
                                }
                                pending.kind = PendingResultKind::Materialized {
                                    rows: result.rows,
                                    cursor: 0,
                                };
                                self.pending_results.insert(effective_qid, pending);
                            }
                        }
                    }
                    Err(e) => {
                        self.interrupts.set_running(None);
                        self.fail_statement(writer, ServerError::from(e)).await?;
                        return Ok(());
                    }
                }
            }
        }

        // -- materialized pull: write records directly -------------------
        let pending = match self.pending_results.get_mut(&effective_qid) {
            Some(p) => p,
            None => {
                self.enter_failed();
                self.write_msg(
                    writer,
                    self.failure(
                        "Neo.ClientError.Request.Invalid",
                        "no pending result for qid",
                    ),
                )
                .await?;
                return Ok(());
            }
        };

        let bolt_major = self.version.major;
        let (total, start) = match &pending.kind {
            PendingResultKind::Materialized { rows, cursor } => (rows.len(), *cursor),
            PendingResultKind::Deferred { .. } => unreachable!("resolved above"),
        };

        let limit = if n < 0 { total } else { n as usize };
        let end = std::cmp::min(start + limit, total);

        let ser_start = Instant::now();
        if let PendingResultKind::Materialized { rows, cursor } = &mut pending.kind {
            for i in start..end {
                let fields: Vec<Value> = rows[i]
                    .iter()
                    .map(|v| cypher_value_to_packstream(v, bolt_major))
                    .collect();
                let msg = ServerMessage::Record { fields };
                let value = message::encode_server_message(msg);
                self.encode_buf.clear();
                packstream::encode_value(&mut self.encode_buf, &value);
                framing::write_message(writer, &self.encode_buf).await?;
            }
            *cursor = end;
        }
        let ser_us = ser_start.elapsed().as_micros() as u64;

        let has_more = end < total;
        if has_more {
            self.write_msg(
                writer,
                ServerMessage::Success {
                    metadata: vec![("has_more".into(), Value::Bool(true))],
                },
            )
            .await?;
        } else {
            let total_us = pending.run_start.elapsed().as_micros() as u64;
            debug!(rows = (end - start), exec_us, ser_us, total_us, "pull");
            let pending = self.pending_results.remove(&effective_qid).unwrap();
            let success = self.build_pull_complete_metadata(&pending);
            self.write_msg(writer, ServerMessage::Success { metadata: success })
                .await?;
            self.transition_after_pull_complete(effective_qid);
            // return freed frame/morsel memory to the OS after heavy queries
            if total >= 10_000 {
                crate::registry::collect_allocator_caches();
            }
        }

        if let Some(t0) = t_entry {
            let total = t0.elapsed();
            let phases_str = t_phases
                .iter()
                .map(|(k, d)| format!("{}={}ns", k, d.as_nanos()))
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("[perf] PULL total={}ns | {}", total.as_nanos(), phases_str,);
        }

        Ok(())
    }

    /// Answers a statement that failed while its rows were pulled: FAILURE, or IGNORED when a
    /// RESET stopped it (the client asked for that).
    async fn fail_statement<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut BufWriter<W>,
        e: ServerError,
    ) -> Result<()> {
        if matches!(
            e,
            ServerError::Interrupted {
                reason: traverse_cypher::query::Interrupt::Cancelled,
                ..
            }
        ) && self.interrupts.reset_pending()
        {
            return self.write_msg(writer, ServerMessage::Ignored).await;
        }
        self.enter_failed();
        self.write_msg(writer, self.error_message(&e)).await
    }

    /// Trailing SUCCESS metadata for a fully consumed PULL.
    fn build_pull_complete_metadata(&mut self, pending: &PendingResult) -> Vec<(String, Value)> {
        let bolt_major = self.version.major;
        let is_in_tx = self.state == State::TxStreaming;
        let t_last = pending.run_start.elapsed().as_millis() as i64;

        let mut metadata = vec![
            ("has_more".into(), Value::Bool(false)),
            ("t_last".into(), Value::Integer(t_last)),
            (
                "type".into(),
                Value::String(pending.query_type.as_bolt_str().into()),
            ),
        ];

        let stats = &pending.stats;
        if stats.nodes_created != 0 {
            metadata.push(("nodes-created".into(), Value::Integer(stats.nodes_created)));
        }
        if stats.nodes_deleted != 0 {
            metadata.push(("nodes-deleted".into(), Value::Integer(stats.nodes_deleted)));
        }
        if stats.relationships_created != 0 {
            metadata.push((
                "relationships-created".into(),
                Value::Integer(stats.relationships_created),
            ));
        }
        if stats.relationships_deleted != 0 {
            metadata.push((
                "relationships-deleted".into(),
                Value::Integer(stats.relationships_deleted),
            ));
        }
        if stats.properties_set != 0 {
            metadata.push((
                "properties-set".into(),
                Value::Integer(stats.properties_set),
            ));
        }
        if stats.labels_added != 0 {
            metadata.push(("labels-added".into(), Value::Integer(stats.labels_added)));
        }
        if stats.labels_removed != 0 {
            metadata.push((
                "labels-removed".into(),
                Value::Integer(stats.labels_removed),
            ));
        }
        if stats.indexes_added != 0 {
            metadata.push(("indexes-added".into(), Value::Integer(stats.indexes_added)));
        }
        if stats.indexes_removed != 0 {
            metadata.push((
                "indexes-removed".into(),
                Value::Integer(stats.indexes_removed),
            ));
        }
        if stats.constraints_added != 0 {
            metadata.push((
                "constraints-added".into(),
                Value::Integer(stats.constraints_added),
            ));
        }
        if stats.constraints_removed != 0 {
            metadata.push((
                "constraints-removed".into(),
                Value::Integer(stats.constraints_removed),
            ));
        }

        metadata.push((
            "parsing_time".into(),
            Value::Float(pending.parsing_time.as_secs_f64()),
        ));
        metadata.push((
            "planning_time".into(),
            Value::Float(pending.planning_time.as_secs_f64()),
        ));
        metadata.push((
            "plan_execution_time".into(),
            Value::Float(pending.execution_time.as_secs_f64()),
        ));
        metadata.push(("db".into(), Value::String(self.current_db_name.clone())));

        if !is_in_tx && let Some(ref bk) = self.last_bookmark {
            metadata.push(("bookmark".into(), Value::String(bk.clone())));
        }

        if let Some(ref plan) = pending.plan {
            metadata.push((
                "profile".into(),
                cypher_value_to_packstream(plan, bolt_major),
            ));
        }

        let has_updates = stats.nodes_created != 0
            || stats.nodes_deleted != 0
            || stats.relationships_created != 0
            || stats.relationships_deleted != 0
            || stats.properties_set != 0
            || stats.labels_added != 0
            || stats.labels_removed != 0
            || stats.indexes_added != 0
            || stats.indexes_removed != 0
            || stats.constraints_added != 0
            || stats.constraints_removed != 0;
        if has_updates {
            metadata.push(("contains_updates".into(), Value::Bool(true)));
        }

        let bolt_minor = self.version.minor;
        let notifs: Vec<Value> = self
            .pending_notifications
            .drain(..)
            .map(|n| notification_to_packstream(&n, bolt_major, bolt_minor))
            .collect();
        if !notifs.is_empty() {
            metadata.push(("notifications".into(), Value::List(notifs)));
        }

        metadata
    }

    /// State transition once a PULL is fully consumed (no more rows).
    fn transition_after_pull_complete(&mut self, qid: i64) {
        self.pending_results.remove(&qid);
        if self.pending_results.is_empty() {
            let is_in_tx = self.state == State::TxStreaming;
            if is_in_tx {
                self.state = State::TxReady;
            } else {
                self.state = State::Ready;
                self.read_only_tx = false;
            }
        }
    }

    pub(super) fn handle_discard(&mut self, extra: Vec<(String, Value)>) -> Vec<ServerMessage> {
        if self.state != State::Streaming && self.state != State::TxStreaming {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected DISCARD")];
        }

        let n = extra
            .iter()
            .find(|(k, _)| k == "n")
            .and_then(|(_, v)| v.as_i64())
            .unwrap_or(-1);

        // qid
        let qid = extra
            .iter()
            .find(|(k, _)| k == "qid")
            .and_then(|(_, v)| v.as_i64())
            .unwrap_or(-1);
        let effective_qid = if qid == -1 { self.last_qid } else { qid };

        // resolve deferred results before discarding (needed for partial discard)
        if let Some(pending) = self.pending_results.get(&effective_qid)
            && matches!(pending.kind, PendingResultKind::Deferred { .. })
            && let Some(mut pending) = self.pending_results.remove(&effective_qid)
            && let PendingResultKind::Deferred {
                prepared,
                params,
                statement,
            } = pending.kind
        {
            let store = Arc::clone(&self.current_entry.store);
            let mut read_tx = match store.begin_read() {
                Ok(tx) => tx,
                Err(e) => {
                    return vec![
                        self.failure("Neo.TransientError.General.DatabaseError", &e.to_string()),
                    ];
                }
            };
            let running = crate::statement::start(
                &statement.transaction,
                statement.time_limit.0,
                self.principal(),
                &statement.text,
                Some(Arc::clone(&statement.params)),
            );
            self.client_time_limit = statement.time_limit.1;
            self.interrupts.set_running(Some(Arc::clone(&running.ctx)));
            let executed = {
                let _query = running.ctx.enter();
                traverse_cypher::execute_prepared(&mut read_tx, &prepared, &params)
            };
            self.interrupts.set_running(None);
            match executed {
                Ok(result) => {
                    pending.stats = result.stats;
                    pending.query_type = result.query_type;
                    pending.kind = PendingResultKind::Materialized {
                        rows: result.rows,
                        cursor: 0,
                    };
                    self.pending_results.insert(effective_qid, pending);
                }
                Err(e) => {
                    let e = ServerError::from(e);
                    if matches!(
                        e,
                        ServerError::Interrupted {
                            reason: traverse_cypher::query::Interrupt::Cancelled,
                            ..
                        }
                    ) && self.interrupts.reset_pending()
                    {
                        return vec![ServerMessage::Ignored];
                    }
                    self.enter_failed();
                    return vec![self.error_message(&e)];
                }
            }
        }

        // partial discard: advance the cursor by n records (all if n=-1)
        let has_more = if let Some(pending) = self.pending_results.get_mut(&effective_qid) {
            if let PendingResultKind::Materialized { rows, cursor } = &mut pending.kind {
                let total = rows.len();
                if n < 0 {
                    *cursor = total;
                } else {
                    *cursor = std::cmp::min(*cursor + n as usize, total);
                }
                *cursor < total
            } else {
                false
            }
        } else {
            false
        };

        if !has_more {
            self.pending_results.remove(&effective_qid);
            let is_in_tx = self.state == State::TxStreaming;
            if self.pending_results.is_empty() {
                if is_in_tx {
                    self.state = State::TxReady;
                } else {
                    self.state = State::Ready;
                    // clear auto-commit read-only mode
                    self.read_only_tx = false;
                }
            }
        }

        vec![ServerMessage::Success {
            metadata: vec![("has_more".into(), Value::Bool(has_more))],
        }]
    }
}
