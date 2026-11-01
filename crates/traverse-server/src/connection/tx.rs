//! Explicit transactions: BEGIN, COMMIT and ROLLBACK, and bookmarks.

use std::collections::BTreeMap;
use std::time::Duration;

use tracing::debug;
use traverse_core::tx::timestamp::HybridTimestamp;
use traverse_cypher::types::CypherValue;

use crate::message::ServerMessage;
use crate::notification::Severity;
use crate::packstream::Value;

use super::{Connection, ExplicitTx, State};

/// Bolt `tx_metadata` as a map for the transaction registry; anything but a map is ignored.
pub(super) fn metadata_map(v: &Value) -> BTreeMap<String, CypherValue> {
    match v {
        Value::Map(entries) => entries
            .iter()
            .filter_map(|(k, v)| {
                crate::bolt_type::packstream_to_cypher_value(v)
                    .ok()
                    .map(|cv| (k.clone(), cv))
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

impl Connection {
    pub(super) fn handle_begin(&mut self, extra: Vec<(String, Value)>) -> Vec<ServerMessage> {
        if self.state != State::Ready {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected BEGIN")];
        }

        // extras: db, tx_timeout, bookmarks, imp_user, notifications
        if let Some(err) = self.parse_begin_extras(&extra) {
            return err;
        }

        let read_only = self.read_only_tx || self.current_entry.read_only;
        let registration = self.register_transaction();
        match ExplicitTx::begin(
            &self.current_entry.store,
            read_only,
            registration,
            self.tx_timeout,
        ) {
            Ok(tx) => self.explicit_tx = Some(tx),
            Err(e) => {
                self.enter_failed();
                return vec![self.error_message(&e)];
            }
        }
        self.terminated_tx = None;
        self.state = State::TxReady;

        vec![ServerMessage::Success { metadata: vec![] }]
    }

    pub(super) fn handle_commit(&mut self) -> Vec<ServerMessage> {
        if self.state != State::TxReady && self.state != State::TxStreaming {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected COMMIT")];
        }

        // terminated by another session or ended by a limit: fails instead of committing
        if let Some(failure) = self.take_ended_transaction() {
            return vec![failure];
        }

        // discard pending results (COMMIT from TxStreaming)
        self.pending_results.clear();

        // clear transaction-scoped state
        self.tx_timeout = None;
        self.read_only_tx = false;

        // restore connection-level notification settings after per-tx overrides
        self.notification_min_severity = self.hello_notification_min_severity;
        self.notification_disabled_categories = self.hello_notification_disabled_categories.clone();

        if let Some(tx) = self.explicit_tx.take() {
            match tx.commit() {
                Ok(bookmark) => {
                    self.last_bookmark = Some(bookmark.clone());
                    self.current_entry.mark_dirty();
                    self.state = State::Ready;
                    vec![ServerMessage::Success {
                        metadata: vec![
                            ("bookmark".into(), Value::String(bookmark)),
                            ("db".into(), Value::String(self.current_db_name.clone())),
                        ],
                    }]
                }
                Err(e) => {
                    // a conflict is Transaction.Outdated: drivers retry the transaction
                    self.enter_failed();
                    vec![self.error_message(&e)]
                }
            }
        } else {
            self.enter_failed();
            vec![self.failure("Neo.ClientError.Request.Invalid", "no active transaction")]
        }
    }

    pub(super) fn handle_rollback(&mut self) -> Vec<ServerMessage> {
        if self.state != State::TxReady && self.state != State::TxStreaming {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected ROLLBACK")];
        }

        // discard pending results (ROLLBACK from TxStreaming)
        self.pending_results.clear();

        // dropping rolls back; a server-terminated transaction rolls back cleanly too
        self.explicit_tx = None;
        self.terminated_tx = None;
        self.state = State::Ready;
        self.tx_timeout = None;
        self.read_only_tx = false;

        // restore connection-level notification settings after per-tx overrides
        self.notification_min_severity = self.hello_notification_min_severity;
        self.notification_disabled_categories = self.hello_notification_disabled_categories.clone();

        vec![ServerMessage::Success { metadata: vec![] }]
    }

    /// Parses BEGIN extras: db, tx_timeout, bookmarks, imp_user, mode, tx_metadata, notification
    /// settings. `Some(failure_response)` for an invalid db name.
    fn parse_begin_extras(&mut self, extra: &[(String, Value)]) -> Option<Vec<ServerMessage>> {
        for (k, v) in extra {
            match k.as_str() {
                "db" => {
                    if let Some(err) = self.resolve_database(v) {
                        return Some(err);
                    }
                }
                "tx_timeout" => {
                    if let Some(ms) = v.as_i64()
                        && ms > 0
                    {
                        self.tx_timeout = Some(Duration::from_millis(ms as u64));
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
                            // parse FB:physical:logical and check causal consistency
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
                    if let Some(mode) = v.as_str() {
                        debug!("transaction mode: {mode}");
                        self.read_only_tx = mode == "r";
                    }
                }
                "imp_user" => {
                    if let Some(user) = v.as_str() {
                        debug!("impersonating user: {user}");
                    }
                }
                "tx_metadata" => {
                    self.tx_metadata = metadata_map(v);
                }
                "notifications_minimum_severity" => {
                    if let Some(sev) = v.as_str() {
                        self.notification_min_severity = Severity::parse(sev);
                        debug!("per-tx notification min severity: {sev}");
                    }
                }
                "notifications_disabled_categories" | "notifications_disabled_classifications" => {
                    if let Value::List(cats) = v {
                        self.notification_disabled_categories = cats
                            .iter()
                            .filter_map(|c| c.as_str().map(|s| s.to_uppercase()))
                            .collect();
                        debug!(
                            "per-tx disabled notification categories: {:?}",
                            self.notification_disabled_categories
                        );
                    }
                }
                other => {
                    debug!("ignoring unknown BEGIN extra: {other}");
                }
            }
        }
        None
    }
}

/// Parses a Traverse bookmark (`FB:<physical>:<logical>`) into a timestamp.
pub(super) fn parse_bookmark(bk: &str) -> Option<HybridTimestamp> {
    let rest = bk.strip_prefix("FB:")?;
    let (phys_str, log_str) = rest.split_once(':')?;
    let physical = phys_str.parse::<u64>().ok()?;
    let logical = log_str.parse::<u32>().ok()?;
    Some(HybridTimestamp { physical, logical })
}
