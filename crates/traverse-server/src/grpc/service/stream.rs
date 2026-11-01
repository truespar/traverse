//! Streaming a result: row batches over the server stream or the bidirectional query
//! channel, with notifications.

use std::time::Instant;

use tokio::sync::mpsc;
use tonic::Status;
use traverse_cypher::executor::CypherResult;

use crate::notification::Notification;

use super::super::convert;
use super::super::proto;
use super::cypher_err_to_status;

// -- Helper: convert notifications to proto ----------------------------

fn notifications_to_proto(notifs: &[Notification]) -> Vec<proto::Notification> {
    notifs
        .iter()
        .map(|n| proto::Notification {
            code: n.code.to_string(),
            title: n.title.to_string(),
            description: n.description.clone(),
            severity: n.severity.as_str().to_string(),
        })
        .collect()
}

// -- Helper: stream a CypherResult over the gRPC channel --------------

/// Rows per streamed batch: one async/blocking context switch per batch instead of per row.
const ROW_BATCH_SIZE: usize = 256;

pub(super) fn send_result(
    rt: &tokio::runtime::Handle,
    tx: &mpsc::Sender<Result<proto::ExecuteQueryResponse, Status>>,
    result: CypherResult,
    bookmark: Option<String>,
    database_name: String,
    t_start: Instant,
    notifications: Vec<Notification>,
) {
    // destructure by value to move fields instead of cloning
    let CypherResult {
        columns,
        rows,
        query_type,
        stats,
        plan,
        ..
    } = result;

    // header (columns moved, not cloned)
    let _ = rt.block_on(tx.send(Ok(proto::ExecuteQueryResponse {
        payload: Some(proto::execute_query_response::Payload::Header(
            proto::QueryHeader { columns },
        )),
    })));

    let t_first_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    // Send rows in batches to reduce per-row context switching
    let mut batch: Vec<Result<proto::ExecuteQueryResponse, Status>> =
        Vec::with_capacity(ROW_BATCH_SIZE);

    for row in rows {
        let values: Vec<proto::Value> = row
            .into_iter()
            .map(convert::cypher_value_to_proto_owned)
            .collect();
        batch.push(Ok(proto::ExecuteQueryResponse {
            payload: Some(proto::execute_query_response::Payload::Row(
                proto::QueryRow { values },
            )),
        }));
        if batch.len() >= ROW_BATCH_SIZE {
            let b = std::mem::replace(&mut batch, Vec::with_capacity(ROW_BATCH_SIZE));
            rt.block_on(async {
                for msg in b {
                    let _ = tx.send(msg).await;
                }
            });
        }
    }
    // Flush remaining rows
    if !batch.is_empty() {
        rt.block_on(async {
            for msg in batch {
                let _ = tx.send(msg).await;
            }
        });
    }

    let t_last_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    // Convert EXPLAIN/PROFILE plan if present (consumed by move)
    let plan = plan.map(convert::cypher_value_to_proto_owned);

    let contains_updates = !stats.is_empty();

    // Send footer with timing metadata
    let _ = rt.block_on(tx.send(Ok(proto::ExecuteQueryResponse {
        payload: Some(proto::execute_query_response::Payload::Footer(
            proto::QueryFooter {
                stats: Some(convert::stats_to_proto(&stats)),
                query_type: query_type.as_bolt_str().to_string(),
                bookmark: bookmark.unwrap_or_default(),
                database: database_name,
                t_first_ms,
                t_last_ms,
                notifications: notifications_to_proto(&notifications),
                plan,
                contains_updates,
            },
        )),
    })));
}

/// Stream rows from a `CypherResultStream` over the gRPC channel.
///
/// Same protocol shape as `send_result` (header -> rows -> footer) but pulls
/// rows lazily from the operator tree instead of from a pre-materialized Vec.
pub(super) fn send_result_streaming(
    rt: &tokio::runtime::Handle,
    tx: &mpsc::Sender<Result<proto::ExecuteQueryResponse, Status>>,
    mut stream: traverse_cypher::executor::CypherResultStream<'_>,
    database_name: String,
    t_start: Instant,
    notifications: Vec<Notification>,
) {
    // Send header
    let _ = rt.block_on(tx.send(Ok(proto::ExecuteQueryResponse {
        payload: Some(proto::execute_query_response::Payload::Header(
            proto::QueryHeader {
                columns: stream.columns().to_vec(),
            },
        )),
    })));

    let t_first_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    // Pull rows from the stream in batches
    let mut batch: Vec<Result<proto::ExecuteQueryResponse, Status>> =
        Vec::with_capacity(ROW_BATCH_SIZE);

    loop {
        match stream.next_row() {
            Ok(Some(row)) => {
                let values: Vec<proto::Value> = row
                    .into_iter()
                    .map(convert::cypher_value_to_proto_owned)
                    .collect();
                batch.push(Ok(proto::ExecuteQueryResponse {
                    payload: Some(proto::execute_query_response::Payload::Row(
                        proto::QueryRow { values },
                    )),
                }));
                if batch.len() >= ROW_BATCH_SIZE {
                    let b = std::mem::replace(&mut batch, Vec::with_capacity(ROW_BATCH_SIZE));
                    rt.block_on(async {
                        for msg in b {
                            let _ = tx.send(msg).await;
                        }
                    });
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = rt.block_on(tx.send(Err(cypher_err_to_status(e))));
                return;
            }
        }
    }

    // Flush remaining rows
    if !batch.is_empty() {
        rt.block_on(async {
            for msg in batch {
                let _ = tx.send(msg).await;
            }
        });
    }

    let t_last_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    // Finalize stream and build footer
    let footer = stream.finish();
    let plan = footer.plan.map(convert::cypher_value_to_proto_owned);
    let contains_updates = !footer.stats.is_empty();

    let _ = rt.block_on(tx.send(Ok(proto::ExecuteQueryResponse {
        payload: Some(proto::execute_query_response::Payload::Footer(
            proto::QueryFooter {
                stats: Some(convert::stats_to_proto(&footer.stats)),
                query_type: footer.query_type.as_bolt_str().to_string(),
                bookmark: String::new(),
                database: database_name,
                t_first_ms,
                t_last_ms,
                notifications: notifications_to_proto(&notifications),
                plan,
                contains_updates,
            },
        )),
    })));
}

// -- Helper: stream a CypherResult over the QueryChannel --------------

pub(super) fn send_channel_result(
    rt: &tokio::runtime::Handle,
    tx: &mpsc::Sender<Result<proto::QueryChannelResponse, Status>>,
    request_id: u64,
    result: CypherResult,
    database_name: String,
    t_start: Instant,
) {
    let CypherResult {
        columns,
        rows,
        query_type,
        stats,
        plan,
        ..
    } = result;

    // Header
    let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
        request_id,
        payload: Some(proto::query_channel_response::Payload::Header(
            proto::QueryHeader { columns },
        )),
    })));

    let t_first_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    // Rows (batched sends)
    let mut batch: Vec<Result<proto::QueryChannelResponse, Status>> =
        Vec::with_capacity(ROW_BATCH_SIZE);

    for row in rows {
        let values: Vec<proto::Value> = row
            .into_iter()
            .map(convert::cypher_value_to_proto_owned)
            .collect();
        batch.push(Ok(proto::QueryChannelResponse {
            request_id,
            payload: Some(proto::query_channel_response::Payload::Row(
                proto::QueryRow { values },
            )),
        }));
        if batch.len() >= ROW_BATCH_SIZE {
            let b = std::mem::replace(&mut batch, Vec::with_capacity(ROW_BATCH_SIZE));
            rt.block_on(async {
                for msg in b {
                    let _ = tx.send(msg).await;
                }
            });
        }
    }
    if !batch.is_empty() {
        rt.block_on(async {
            for msg in batch {
                let _ = tx.send(msg).await;
            }
        });
    }

    let t_last_ms = t_start.elapsed().as_secs_f64() * 1000.0;
    let plan = plan.map(convert::cypher_value_to_proto_owned);
    let contains_updates = !stats.is_empty();

    // Footer
    let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
        request_id,
        payload: Some(proto::query_channel_response::Payload::Footer(
            proto::QueryFooter {
                stats: Some(convert::stats_to_proto(&stats)),
                query_type: query_type.as_bolt_str().to_string(),
                bookmark: String::new(),
                database: database_name,
                t_first_ms,
                t_last_ms,
                notifications: vec![],
                plan,
                contains_updates,
            },
        )),
    })));
}

pub(super) fn send_channel_result_streaming(
    rt: &tokio::runtime::Handle,
    tx: &mpsc::Sender<Result<proto::QueryChannelResponse, Status>>,
    request_id: u64,
    mut stream: traverse_cypher::executor::CypherResultStream<'_>,
    database_name: String,
    t_start: Instant,
) {
    // Header
    let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
        request_id,
        payload: Some(proto::query_channel_response::Payload::Header(
            proto::QueryHeader {
                columns: stream.columns().to_vec(),
            },
        )),
    })));

    let t_first_ms = t_start.elapsed().as_secs_f64() * 1000.0;

    // Rows
    let mut batch: Vec<Result<proto::QueryChannelResponse, Status>> =
        Vec::with_capacity(ROW_BATCH_SIZE);

    loop {
        match stream.next_row() {
            Ok(Some(row)) => {
                let values: Vec<proto::Value> = row
                    .into_iter()
                    .map(convert::cypher_value_to_proto_owned)
                    .collect();
                batch.push(Ok(proto::QueryChannelResponse {
                    request_id,
                    payload: Some(proto::query_channel_response::Payload::Row(
                        proto::QueryRow { values },
                    )),
                }));
                if batch.len() >= ROW_BATCH_SIZE {
                    let b = std::mem::replace(&mut batch, Vec::with_capacity(ROW_BATCH_SIZE));
                    rt.block_on(async {
                        for msg in b {
                            let _ = tx.send(msg).await;
                        }
                    });
                }
            }
            Ok(None) => break,
            Err(e) => {
                let msg = format!("{e}");
                let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
                    request_id,
                    payload: Some(proto::query_channel_response::Payload::Error(
                        proto::QueryError {
                            code: 1,
                            message: msg,
                        },
                    )),
                })));
                return;
            }
        }
    }

    if !batch.is_empty() {
        rt.block_on(async {
            for msg in batch {
                let _ = tx.send(msg).await;
            }
        });
    }

    let t_last_ms = t_start.elapsed().as_secs_f64() * 1000.0;
    let footer = stream.finish();
    let plan = footer.plan.map(convert::cypher_value_to_proto_owned);
    let contains_updates = !footer.stats.is_empty();

    let _ = rt.block_on(tx.send(Ok(proto::QueryChannelResponse {
        request_id,
        payload: Some(proto::query_channel_response::Payload::Footer(
            proto::QueryFooter {
                stats: Some(convert::stats_to_proto(&footer.stats)),
                query_type: footer.query_type.as_bolt_str().to_string(),
                bookmark: String::new(),
                database: database_name,
                t_first_ms,
                t_last_ms,
                notifications: vec![],
                plan,
                contains_updates,
            },
        )),
    })));
}
