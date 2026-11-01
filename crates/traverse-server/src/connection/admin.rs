//! Procedures, user management and database commands run over Bolt.

use std::sync::Arc;
use std::time::{Duration, Instant};

use traverse_cypher::executor::{QueryStats, QueryType};
use traverse_cypher::types::CypherValue;

use crate::auth::Role;
use crate::is_admin_query;
use crate::is_write_query;
use crate::message::ServerMessage;
use crate::packstream::Value;

use super::{Connection, PendingResult, PendingResultKind, State};

impl Connection {
    /// Built-in procedure calls and database management commands: `Some(response)` for a recognized
    /// procedure/command, `None` to fall through to Cypher execution.
    pub(super) fn handle_procedure(&mut self, query_trimmed: &str) -> Option<Vec<ServerMessage>> {
        // permission checks for admin and write queries
        if self.role.is_some() {
            if is_admin_query(query_trimmed) && self.role != Some(Role::Admin) {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Security.Forbidden",
                    "admin privilege required",
                )]);
            }
            if is_write_query(query_trimmed) && self.role == Some(Role::Reader) {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Security.Forbidden",
                    "write privilege required",
                )]);
            }
        }

        // user management (CREATE/DROP/ALTER USER, SHOW USERS)
        if let Some(result) = self.handle_user_command(query_trimmed) {
            return Some(result);
        }

        // database management (SHOW/CREATE/DROP DATABASE)
        if let Some(result) = self.handle_database_command(query_trimmed) {
            return Some(result);
        }

        // normalize: strip "CALL " prefix, trailing "()", whitespace
        let call_body =
            if query_trimmed.len() > 5 && query_trimmed[..5].eq_ignore_ascii_case("CALL ") {
                query_trimmed[5..]
                    .trim()
                    .trim_end_matches("()")
                    .trim_end_matches('(')
                    .trim()
            } else {
                return None;
            };

        let (columns, rows, query_type) = match call_body.to_lowercase().as_str() {
            "db.ping" => (vec![], vec![], QueryType::Read),
            "dbms.components" => {
                let row = vec![
                    CypherValue::String("Traverse".into()),
                    CypherValue::List(vec![CypherValue::String(env!("CARGO_PKG_VERSION").into())]),
                    CypherValue::String("community".into()),
                ];
                (
                    vec!["name".into(), "versions".into(), "edition".into()],
                    vec![row],
                    QueryType::Read,
                )
            }
            "db.labels" => {
                let read_tx = match self.current_entry.store.begin_read() {
                    Ok(tx) => tx,
                    Err(e) => {
                        return Some(vec![self.failure(
                            "Neo.TransientError.General.DatabaseError",
                            &e.to_string(),
                        )]);
                    }
                };
                let strings = traverse_core::tx::GraphView::strings(&read_tx);
                let mut labels = std::collections::BTreeSet::new();
                for nid in traverse_core::tx::GraphView::nodes_vec(&read_tx) {
                    if let Ok(node) = traverse_core::tx::GraphView::get_node(&read_tx, nid) {
                        for lid in &node.labels {
                            labels.insert(strings.resolve(lid.0).to_string());
                        }
                    }
                }
                let rows: Vec<Vec<CypherValue>> = labels
                    .into_iter()
                    .map(|l| vec![CypherValue::String(l)])
                    .collect();
                (vec!["label".into()], rows, QueryType::Read)
            }
            "db.relationshiptypes" => {
                let read_tx = match self.current_entry.store.begin_read() {
                    Ok(tx) => tx,
                    Err(e) => {
                        return Some(vec![self.failure(
                            "Neo.TransientError.General.DatabaseError",
                            &e.to_string(),
                        )]);
                    }
                };
                let strings = traverse_core::tx::GraphView::strings(&read_tx);
                let mut types = std::collections::BTreeSet::new();
                for eid in traverse_core::tx::GraphView::all_edges_vec(&read_tx) {
                    if let Ok(edge) = traverse_core::tx::GraphView::get_edge(&read_tx, eid) {
                        types.insert(strings.resolve(edge.edge_type.0).to_string());
                    }
                }
                let rows: Vec<Vec<CypherValue>> = types
                    .into_iter()
                    .map(|t| vec![CypherValue::String(t)])
                    .collect();
                (vec!["relationshipType".into()], rows, QueryType::Read)
            }
            "db.propertykeys" => {
                let read_tx = match self.current_entry.store.begin_read() {
                    Ok(tx) => tx,
                    Err(e) => {
                        return Some(vec![self.failure(
                            "Neo.TransientError.General.DatabaseError",
                            &e.to_string(),
                        )]);
                    }
                };
                let strings = traverse_core::tx::GraphView::strings(&read_tx);
                let mut keys = std::collections::BTreeSet::new();
                for nid in traverse_core::tx::GraphView::nodes_vec(&read_tx) {
                    if let Ok(node) = traverse_core::tx::GraphView::get_node(&read_tx, nid) {
                        for k in node.properties.keys() {
                            keys.insert(strings.resolve(k.0).to_string());
                        }
                    }
                }
                for eid in traverse_core::tx::GraphView::all_edges_vec(&read_tx) {
                    if let Ok(edge) = traverse_core::tx::GraphView::get_edge(&read_tx, eid) {
                        for k in edge.properties.keys() {
                            keys.insert(strings.resolve(k.0).to_string());
                        }
                    }
                }
                let rows: Vec<Vec<CypherValue>> = keys
                    .into_iter()
                    .map(|k| vec![CypherValue::String(k)])
                    .collect();
                (vec!["propertyKey".into()], rows, QueryType::Read)
            }
            _ => return None,
        };

        Some(self.emit_procedure_result(columns, rows, query_type))
    }

    /// User management: CREATE USER, DROP USER, ALTER USER, SHOW USERS.
    fn handle_user_command(&mut self, query: &str) -> Option<Vec<ServerMessage>> {
        let command = match crate::auth::parse_user_command(query)? {
            Ok(command) => command,
            Err(msg) => {
                self.enter_failed();
                return Some(vec![
                    self.failure("Neo.ClientError.Statement.SyntaxError", &msg),
                ]);
            }
        };
        let outcome = crate::auth::run_user_command(
            command,
            &self.auth_catalog,
            &self.registry,
            self.username.as_deref(),
        );
        match outcome {
            Ok(Some(users)) => {
                let rows = users
                    .into_iter()
                    .map(|(name, role)| {
                        vec![
                            CypherValue::String(name),
                            CypherValue::String(role.as_str().to_string()),
                        ]
                    })
                    .collect();
                Some(self.emit_procedure_result(
                    vec!["user".into(), "role".into()],
                    rows,
                    QueryType::Read,
                ))
            }
            Ok(None) => Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema)),
            Err(e) => {
                self.enter_failed();
                Some(vec![self.failure(e.code(), &e.to_string())])
            }
        }
    }

    /// Database management: SHOW DATABASES, CREATE DATABASE, DROP DATABASE.
    fn handle_database_command(&mut self, query: &str) -> Option<Vec<ServerMessage>> {
        let upper = query.to_uppercase();
        let trimmed = upper.trim();

        if trimmed == "SHOW DATABASES" || trimmed == "SHOW DATABASES;" {
            let databases = self.registry.list_databases();
            let rows: Vec<Vec<CypherValue>> = databases
                .into_iter()
                .map(|(name, is_default, read_only, aliases)| {
                    vec![
                        CypherValue::String(name.clone()),
                        CypherValue::String("online".into()),
                        CypherValue::Bool(is_default),
                        CypherValue::Bool(name == self.current_db_name),
                        CypherValue::Bool(read_only),
                        CypherValue::List(aliases.into_iter().map(CypherValue::String).collect()),
                    ]
                })
                .collect();
            let columns = vec![
                "name".into(),
                "currentStatus".into(),
                "default".into(),
                "current".into(),
                "readOnly".into(),
                "aliases".into(),
            ];
            return Some(self.emit_procedure_result(columns, rows, QueryType::Read));
        }

        // SHOW INDEX INFO
        if trimmed == "SHOW INDEX INFO" || trimmed == "SHOW INDEX INFO;" {
            let store = Arc::clone(&self.current_entry.store);
            let read_tx = match store.begin_read() {
                Ok(tx) => tx,
                Err(e) => {
                    self.enter_failed();
                    return Some(vec![self.failure(
                        "Neo.TransientError.General.DatabaseError",
                        &e.to_string(),
                    )]);
                }
            };
            let strings = traverse_core::tx::GraphView::strings(&read_tx);
            let index_reg = traverse_core::tx::GraphView::index_catalog(&read_tx);
            let edge_index_reg = traverse_core::tx::GraphView::edge_index_catalog(&read_tx);
            let mut rows: Vec<Vec<CypherValue>> = Vec::new();
            for d in index_reg.descriptors() {
                let label = strings.resolve(d.label_id.0).to_string();
                let property = strings.resolve(d.property_key.0).to_string();
                let entries = traverse_core::tx::GraphView::index_size(&read_tx, &d.name);
                rows.push(vec![
                    CypherValue::String("label+property".into()),
                    CypherValue::String(label),
                    CypherValue::String(property),
                    CypherValue::Integer(entries.unwrap_or(0) as i64),
                ]);
            }
            // edge property and type-only indexes
            for d in edge_index_reg.descriptors() {
                let edge_type = strings.resolve(d.edge_type_id.0).to_string();
                let entries = traverse_core::tx::GraphView::index_size(&read_tx, &d.name);
                let (kind, property, entries) = match d.property_key {
                    Some(pk) => (
                        "edge+property",
                        strings.resolve(pk.0).to_string(),
                        entries.unwrap_or(0),
                    ),
                    None => ("edge+type", String::new(), 0),
                };
                rows.push(vec![
                    CypherValue::String(kind.into()),
                    CypherValue::String(edge_type),
                    CypherValue::String(property),
                    CypherValue::Integer(entries as i64),
                ]);
            }
            drop(read_tx);
            rows.sort_by(|a, b| {
                let a_type = if let CypherValue::String(s) = &a[0] {
                    s.as_str()
                } else {
                    ""
                };
                let b_type = if let CypherValue::String(s) = &b[0] {
                    s.as_str()
                } else {
                    ""
                };
                let a_label = if let CypherValue::String(s) = &a[1] {
                    s.as_str()
                } else {
                    ""
                };
                let b_label = if let CypherValue::String(s) = &b[1] {
                    s.as_str()
                } else {
                    ""
                };
                a_type.cmp(b_type).then(a_label.cmp(b_label))
            });
            let columns = vec![
                "index type".into(),
                "label".into(),
                "property".into(),
                "count".into(),
            ];
            return Some(self.emit_procedure_result(columns, rows, QueryType::Read));
        }

        // SHOW CONSTRAINT INFO
        if trimmed == "SHOW CONSTRAINT INFO" || trimmed == "SHOW CONSTRAINT INFO;" {
            let store = Arc::clone(&self.current_entry.store);
            let read_tx = match store.begin_read() {
                Ok(tx) => tx,
                Err(e) => {
                    self.enter_failed();
                    return Some(vec![self.failure(
                        "Neo.TransientError.General.DatabaseError",
                        &e.to_string(),
                    )]);
                }
            };
            let strings = traverse_core::tx::GraphView::strings(&read_tx);
            let constraint_reg = traverse_core::tx::GraphView::constraint_registry(&read_tx);
            let mut rows: Vec<Vec<CypherValue>> = Vec::new();
            for desc in constraint_reg.descriptors() {
                let ctype = match &desc.constraint_type {
                    traverse_core::graph::constraint::ConstraintType::Unique => {
                        "unique".to_string()
                    }
                    traverse_core::graph::constraint::ConstraintType::Exists => {
                        "exists".to_string()
                    }
                    traverse_core::graph::constraint::ConstraintType::Type(k) => {
                        format!("type:{}", k.surface())
                    }
                    traverse_core::graph::constraint::ConstraintType::NodeKey => {
                        "node_key".to_string()
                    }
                };
                let label = strings.resolve(desc.label_id.0).to_string();
                let property = strings.resolve(desc.property_key.0).to_string();
                rows.push(vec![
                    CypherValue::String(ctype),
                    CypherValue::String(label),
                    CypherValue::List(vec![CypherValue::String(property)]),
                    CypherValue::String(String::new()),
                ]);
            }
            drop(read_tx);
            let columns = vec![
                "constraint type".into(),
                "label".into(),
                "properties".into(),
                "data_type".into(),
            ];
            return Some(self.emit_procedure_result(columns, rows, QueryType::Read));
        }

        if let Some(rest) = trimmed.strip_prefix("CREATE DATABASE") {
            let name = rest.trim().trim_end_matches(';').trim();
            if name.is_empty() {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "CREATE DATABASE requires a database name",
                )]);
            }
            // name: alphanumerics + underscores only
            let name_lower = name.to_lowercase();
            if !name_lower
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    &format!("invalid database name '{name_lower}': must contain only alphanumeric characters and underscores"),
                )]);
            }
            match self.registry.create_database(&name_lower) {
                Ok(_) => {
                    return Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema));
                }
                Err(msg) => {
                    self.enter_failed();
                    return Some(vec![
                        self.failure("Neo.ClientError.Database.DatabaseAlreadyExists", &msg),
                    ]);
                }
            }
        }

        if let Some(rest) = trimmed.strip_prefix("DROP DATABASE") {
            let name = rest.trim().trim_end_matches(';').trim();
            if name.is_empty() {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "DROP DATABASE requires a database name",
                )]);
            }
            let name_lower = name.to_lowercase();
            match self
                .registry
                .drop_database(&name_lower, &self.current_db_name)
            {
                Ok(()) => {
                    return Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema));
                }
                Err(msg) => {
                    self.enter_failed();
                    return Some(vec![
                        self.failure("Neo.ClientError.Database.DatabaseNotFound", &msg),
                    ]);
                }
            }
        }

        // LOAD DATABASE <name> FROM '<file>'
        if let Some(rest) = trimmed.strip_prefix("LOAD DATABASE") {
            let rest = rest.trim().trim_end_matches(';').trim();
            // split on FROM
            let parts: Vec<&str> = rest.splitn(2, "FROM").collect();
            if parts.len() != 2 {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "LOAD DATABASE requires: LOAD DATABASE <name> FROM '<file>'",
                )]);
            }
            let name = parts[0].trim();
            let file = parts[1].trim().trim_matches('\'').trim_matches('"');
            if name.is_empty() || file.is_empty() {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "LOAD DATABASE requires a database name and file path",
                )]);
            }
            let name_lower = name.to_lowercase();
            if !name_lower
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    &format!("invalid database name '{name_lower}': must contain only alphanumeric characters and underscores"),
                )]);
            }
            // original case for the file path
            let file_orig = {
                let orig_rest = query.trim().trim_end_matches(';').trim();
                let orig_upper = orig_rest.to_uppercase();
                if let Some(idx) = orig_upper.rfind("FROM") {
                    orig_rest[idx + 4..]
                        .trim()
                        .trim_matches('\'')
                        .trim_matches('"')
                        .to_string()
                } else {
                    file.to_string()
                }
            };
            tracing::info!(
                "load_database: triggered from Bolt LOAD DATABASE name={} file={}",
                name_lower,
                file_orig
            );
            match self.registry.load_database(&name_lower, &file_orig) {
                Ok(_) => {
                    return Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema));
                }
                Err(msg) => {
                    let code = if msg.contains("already exists") {
                        "Neo.ClientError.Database.DatabaseAlreadyExists"
                    } else {
                        "Neo.ClientError.Statement.SyntaxError"
                    };
                    self.enter_failed();
                    return Some(vec![self.failure(code, &msg)]);
                }
            }
        }

        // SHOW ALIASES
        if trimmed == "SHOW ALIASES" || trimmed == "SHOW ALIASES;" {
            let aliases = self.registry.list_aliases();
            let rows: Vec<Vec<CypherValue>> = aliases
                .into_iter()
                .map(|(alias, database)| {
                    vec![CypherValue::String(alias), CypherValue::String(database)]
                })
                .collect();
            let columns = vec!["alias".into(), "database".into()];
            return Some(self.emit_procedure_result(columns, rows, QueryType::Read));
        }

        // CREATE ALIAS <name> FOR DATABASE <target>
        if let Some(rest) = trimmed.strip_prefix("CREATE ALIAS") {
            let rest = rest.trim().trim_end_matches(';').trim();
            let parts: Vec<&str> = rest.splitn(2, "FOR DATABASE").collect();
            if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "CREATE ALIAS requires: CREATE ALIAS <name> FOR DATABASE <target>",
                )]);
            }
            let alias = parts[0].trim().to_lowercase();
            let target = parts[1].trim().to_lowercase();
            if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    &format!("invalid alias name '{}': must contain only alphanumeric characters and underscores", alias),
                )]);
            }
            match self.registry.create_alias(&alias, &target) {
                Ok(()) => {
                    return Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema));
                }
                Err(msg) => {
                    let code = if msg.contains("already") {
                        "Neo.ClientError.Database.DatabaseAlreadyExists"
                    } else {
                        "Neo.ClientError.Database.DatabaseNotFound"
                    };
                    self.enter_failed();
                    return Some(vec![self.failure(code, &msg)]);
                }
            }
        }

        // DROP ALIAS <name>
        if let Some(rest) = trimmed.strip_prefix("DROP ALIAS") {
            let name = rest.trim().trim_end_matches(';').trim();
            if name.is_empty() {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "DROP ALIAS requires a name",
                )]);
            }
            let name_lower = name.to_lowercase();
            match self.registry.drop_alias(&name_lower) {
                Ok(()) => {
                    return Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema));
                }
                Err(msg) => {
                    self.enter_failed();
                    return Some(vec![
                        self.failure("Neo.ClientError.Database.DatabaseNotFound", &msg),
                    ]);
                }
            }
        }

        // ALTER ALIAS <name> SET DATABASE <target>
        if let Some(rest) = trimmed.strip_prefix("ALTER ALIAS") {
            let rest = rest.trim().trim_end_matches(';').trim();
            let parts: Vec<&str> = rest.splitn(2, "SET DATABASE").collect();
            if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
                self.enter_failed();
                return Some(vec![self.failure(
                    "Neo.ClientError.Statement.SyntaxError",
                    "ALTER ALIAS requires: ALTER ALIAS <name> SET DATABASE <target>",
                )]);
            }
            let alias = parts[0].trim().to_lowercase();
            let new_target = parts[1].trim().to_lowercase();
            match self.registry.alter_alias(&alias, &new_target) {
                Ok(()) => {
                    return Some(self.emit_procedure_result(vec![], vec![], QueryType::Schema));
                }
                Err(msg) => {
                    self.enter_failed();
                    return Some(vec![
                        self.failure("Neo.ClientError.Database.DatabaseNotFound", &msg),
                    ]);
                }
            }
        }

        None
    }

    /// Emits a procedure/command result with the right state transitions.
    fn emit_procedure_result(
        &mut self,
        columns: Vec<String>,
        rows: Vec<Vec<CypherValue>>,
        query_type: QueryType,
    ) -> Vec<ServerMessage> {
        let qid = self.next_qid;
        self.next_qid += 1;
        self.last_qid = qid;

        let is_in_tx = self.state == State::TxReady || self.state == State::TxStreaming;
        self.pending_results.insert(
            qid,
            PendingResult {
                kind: PendingResultKind::Materialized { rows, cursor: 0 },
                stats: QueryStats::default(),
                query_type,
                plan: None,
                run_start: Instant::now(),
                parsing_time: Duration::ZERO,
                planning_time: Duration::ZERO,
                execution_time: Duration::ZERO,
            },
        );
        self.state = if is_in_tx {
            State::TxStreaming
        } else {
            State::Streaming
        };

        let columns_value = Value::List(
            columns
                .iter()
                .map(|c: &String| Value::String(c.clone()))
                .collect(),
        );
        vec![ServerMessage::Success {
            metadata: vec![
                ("fields".into(), columns_value),
                ("t_first".into(), Value::Integer(0)),
                (
                    "type".into(),
                    Value::String(query_type.as_bolt_str().into()),
                ),
                ("qid".into(), Value::Integer(qid)),
                ("db".into(), Value::String(self.current_db_name.clone())),
            ],
        }]
    }
}
