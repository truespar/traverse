//! Session messages: HELLO, LOGON, LOGOFF, RESET and ROUTE.

use tracing::debug;

use crate::message::ServerMessage;
use crate::notification::Severity;
use crate::packstream::Value;
use crate::sync::Unpoisoned as _;

use super::{Connection, State};

impl Connection {
    pub(super) fn handle_hello(&mut self, extra: Vec<(String, Value)>) -> Vec<ServerMessage> {
        if self.state != State::Authentication {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected HELLO")];
        }

        // log the client's user_agent and bolt_agent
        for (k, v) in &extra {
            match k.as_str() {
                "user_agent" => {
                    if let Some(ua) = v.as_str() {
                        debug!("client user_agent: {ua}");
                    }
                }
                "bolt_agent" => {
                    debug!("client bolt_agent: {v:?}");
                }
                "notifications_minimum_severity" => {
                    if let Some(sev) = v.as_str() {
                        self.notification_min_severity = Severity::parse(sev);
                        self.hello_notification_min_severity = self.notification_min_severity;
                        debug!("notification min severity: {sev}");
                    }
                }
                "notifications_disabled_categories" | "notifications_disabled_classifications" => {
                    if let Value::List(cats) = v {
                        let categories: Vec<String> = cats
                            .iter()
                            .filter_map(|c| c.as_str().map(|s| s.to_uppercase()))
                            .collect();
                        debug!("disabled notification categories: {categories:?}");
                        self.notification_disabled_categories = categories.clone();
                        self.hello_notification_disabled_categories = categories;
                    }
                }
                other => {
                    debug!("ignoring unknown HELLO extra: {other}");
                }
            }
        }

        // Bolt server agent. Some clients (py2neo among them) refuse to talk unless it starts with
        // "Neo4j/"; TRAVERSE_SERVER_AGENT overrides it so they can be tested against Traverse (e.g.
        // TRAVERSE_SERVER_AGENT="Neo4j/5.26.0). A compatibility/debug aid; the default stays
        // Traverse/<version>.
        let server_agent = std::env::var("TRAVERSE_SERVER_AGENT")
            .unwrap_or_else(|_| concat!("Traverse/", env!("CARGO_PKG_VERSION")).to_string());

        vec![ServerMessage::Success {
            metadata: vec![
                ("server".into(), Value::String(server_agent)),
                (
                    "connection_id".into(),
                    Value::String(self.connection_id.clone()),
                ),
                (
                    "hints".into(),
                    Value::Map(vec![(
                        "connection.recv_timeout_seconds".into(),
                        Value::Integer(120),
                    )]),
                ),
                // Bolt 5.4+: enabled protocol patches (empty = none)
                ("patch_bolt".into(), Value::List(vec![])),
            ],
        }]
    }

    pub(super) fn handle_logon(&mut self, auth_data: Vec<(String, Value)>) -> Vec<ServerMessage> {
        if self.state != State::Authentication {
            return vec![self.failure("Neo.ClientError.Request.Invalid", "unexpected LOGON")];
        }

        // auth is on if the catalog has users
        let auth_enabled = !self.auth_catalog.read_unpoisoned().is_empty();

        if auth_enabled {
            let scheme = auth_data
                .iter()
                .find(|(k, _)| k == "scheme")
                .and_then(|(_, v)| v.as_str())
                .unwrap_or("none");

            match scheme {
                "none" => {
                    self.enter_failed();
                    return vec![self.failure(
                        "Neo.ClientError.Security.Unauthorized",
                        "authentication required",
                    )];
                }
                "basic" => {
                    let principal = auth_data
                        .iter()
                        .find(|(k, _)| k == "principal")
                        .and_then(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let cred = auth_data
                        .iter()
                        .find(|(k, _)| k == "credentials")
                        .and_then(|(_, v)| v.as_str())
                        .unwrap_or("");

                    match crate::auth::authenticate(&self.auth_catalog, principal, cred) {
                        Some(role) => {
                            self.role = Some(role);
                            self.username = Some(principal.to_string());
                        }
                        None => {
                            self.enter_failed();
                            return vec![self.failure(
                                "Neo.ClientError.Security.Unauthorized",
                                "invalid principal or credentials",
                            )];
                        }
                    }
                }
                other => {
                    self.enter_failed();
                    return vec![self.failure(
                        "Neo.ClientError.Security.Unauthorized",
                        &format!("unsupported auth scheme: {other}"),
                    )];
                }
            }
        }

        self.state = State::Ready;

        vec![ServerMessage::Success { metadata: vec![] }]
    }

    pub(super) fn handle_logoff(&mut self) -> Vec<ServerMessage> {
        // clean up any active transaction (dropping rolls it back) and pending results
        self.explicit_tx = None;
        self.terminated_tx = None;
        self.pending_results.clear();
        self.next_qid = 0;
        self.last_qid = -1;
        self.tx_timeout = None;
        self.tx_metadata.clear();
        self.role = None;
        self.username = None;

        self.state = State::Authentication;
        vec![ServerMessage::Success { metadata: vec![] }]
    }

    pub(super) fn handle_reset(&mut self) -> Vec<ServerMessage> {
        self.pending_results.clear();
        self.next_qid = 0;
        self.last_qid = -1;
        self.explicit_tx = None;
        self.terminated_tx = None;
        self.tx_timeout = None;
        self.tx_metadata.clear();
        self.read_only_tx = false;

        // notification settings back to the connection-level defaults
        self.notification_min_severity = self.hello_notification_min_severity;
        self.notification_disabled_categories = self.hello_notification_disabled_categories.clone();

        // back to the pre-failure state: Failed from Authentication -> Authentication (re-auth
        // needed); Failed from anything else -> Ready; not Failed -> Ready (unless in Authentication)
        if self.state == State::Failed {
            self.state = match self.pre_failure_state {
                State::Authentication => State::Authentication,
                _ => State::Ready,
            };
        } else if self.state != State::Authentication {
            self.state = State::Ready;
        }

        vec![ServerMessage::Success { metadata: vec![] }]
    }

    pub(super) fn handle_route(
        &mut self,
        routing: Vec<(String, Value)>,
        bookmarks: Vec<String>,
        db: Option<String>,
    ) -> Vec<ServerMessage> {
        // log routing context and bookmarks
        if !routing.is_empty() {
            debug!("ROUTE routing context: {routing:?}");
        }
        if !bookmarks.is_empty() {
            debug!("ROUTE bookmarks: {bookmarks:?}");
        }

        // validate db
        let resolved_db = if let Some(ref db_name) = db {
            if !db_name.is_empty() {
                if self.registry.get(db_name).is_none() {
                    return vec![self.failure(
                        "Neo.ClientError.Database.DatabaseNotFound",
                        &format!("database '{db_name}' not found"),
                    )];
                }
                db_name.clone()
            } else {
                self.current_db_name.clone()
            }
        } else {
            self.current_db_name.clone()
        };

        let addr = Value::String(self.server_addr.clone());

        let rt = vec![
            ("ttl".into(), Value::Integer(300)),
            ("db".into(), Value::String(resolved_db)),
            (
                "servers".into(),
                Value::List(vec![
                    Value::Map(vec![
                        ("role".into(), Value::String("WRITE".into())),
                        ("addresses".into(), Value::List(vec![addr.clone()])),
                    ]),
                    Value::Map(vec![
                        ("role".into(), Value::String("READ".into())),
                        ("addresses".into(), Value::List(vec![addr.clone()])),
                    ]),
                    Value::Map(vec![
                        ("role".into(), Value::String("ROUTE".into())),
                        ("addresses".into(), Value::List(vec![addr])),
                    ]),
                ]),
            ),
        ];

        vec![ServerMessage::Success { metadata: rt }]
    }
}
