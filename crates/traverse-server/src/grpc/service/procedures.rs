//! User commands and built-in procedures over gRPC, and auto-commit execution.

use std::collections::HashMap;
use std::sync::Arc;

use tonic::Status;
use traverse_core::tx::{GraphView, MvccStore};
use traverse_cypher::executor::{CypherResult, QueryStats, QueryType};
use traverse_cypher::types::CypherValue;

use crate::auth::UserError;
use crate::registry::{DatabaseEntry, DatabaseRegistry};
use crate::{is_admin_query, is_write_query};

use super::{AuthInfo, cypher_err_to_status, require_admin, require_write};

// -- Helper: user management ------------------------------------------

/// User management: CREATE USER, DROP USER, ALTER USER, SHOW USERS. `Some(result)` for a user
/// command, else `None`.
fn handle_user_command(
    query: &str,
    auth_catalog: &Option<Arc<std::sync::RwLock<crate::auth::AuthCatalog>>>,
    auth: &AuthInfo,
    registry: &DatabaseRegistry,
) -> Option<Result<CypherResult, Status>> {
    let command = crate::auth::parse_user_command(query)?;
    let Some(users) = auth_catalog else {
        return Some(Err(Status::unimplemented(
            "user management not available (auth not configured)",
        )));
    };
    let command = match command {
        Ok(command) => command,
        Err(msg) => return Some(Err(Status::invalid_argument(msg))),
    };
    let outcome = crate::auth::run_user_command(command, users, registry, auth.username.as_deref());
    let (columns, rows, query_type) = match outcome {
        Ok(Some(list)) => (
            vec!["user".into(), "role".into()],
            list.into_iter()
                .map(|(name, role)| {
                    vec![
                        CypherValue::String(name),
                        CypherValue::String(role.as_str().to_string()),
                    ]
                })
                .collect(),
            QueryType::Read,
        ),
        Ok(None) => (vec![], vec![], QueryType::Schema),
        Err(e) => {
            let msg = e.to_string();
            return Some(Err(match e {
                UserError::Exists(_) => Status::already_exists(msg),
                UserError::NotFound(_) => Status::not_found(msg),
                UserError::DropSelf => Status::permission_denied(msg),
                UserError::Hash(_) | UserError::Save(_) => Status::internal(msg),
            }));
        }
    };
    Some(Ok(CypherResult {
        columns,
        rows,
        query_type,
        stats: QueryStats::default(),
        plan: None,
        bound: Vec::new(),
    }))
}

// -- Helper: handle built-in procedures via Cypher text ---------------

/// Runs a built-in procedure or admin command directly; `None` falls through to normal Cypher
/// execution.
pub(super) fn handle_procedure(
    query_trimmed: &str,
    store: &Arc<MvccStore>,
    registry: &DatabaseRegistry,
    current_db_name: &str,
    auth_catalog: &Option<Arc<std::sync::RwLock<crate::auth::AuthCatalog>>>,
    auth: &AuthInfo,
) -> Option<Result<CypherResult, Status>> {
    let upper = query_trimmed.to_uppercase();
    let trimmed_upper = upper.trim();

    // RBAC: admin commands require Admin role
    if auth.role.is_some() {
        if is_admin_query(query_trimmed)
            && let Err(e) = require_admin(auth)
        {
            return Some(Err(e));
        }
        if is_write_query(query_trimmed)
            && let Err(e) = require_write(auth)
        {
            return Some(Err(e));
        }
    }

    // Handle user management commands
    if let Some(result) = handle_user_command(query_trimmed, auth_catalog, auth, registry) {
        return Some(result);
    }

    // Handle SHOW DATABASES
    if trimmed_upper == "SHOW DATABASES" || trimmed_upper == "SHOW DATABASES;" {
        let databases = registry.list_databases();
        let rows: Vec<Vec<CypherValue>> = databases
            .into_iter()
            .map(|(name, is_default, read_only, aliases)| {
                vec![
                    CypherValue::String(name.clone()),
                    CypherValue::String("online".into()),
                    CypherValue::Bool(is_default),
                    CypherValue::Bool(name == current_db_name),
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
        return Some(Ok(CypherResult {
            columns,
            rows,
            query_type: QueryType::Read,
            stats: QueryStats::default(),
            plan: None,
            bound: Vec::new(),
        }));
    }

    // Handle CREATE DATABASE
    if let Some(rest) = trimmed_upper.strip_prefix("CREATE DATABASE") {
        let name = rest.trim().trim_end_matches(';').trim();
        if name.is_empty() {
            return Some(Err(Status::invalid_argument(
                "CREATE DATABASE requires a database name",
            )));
        }
        let name_lower = name.to_lowercase();
        if !name_lower
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Some(Err(Status::invalid_argument(format!(
                "invalid database name '{}': must contain only alphanumeric characters and underscores",
                name_lower
            ))));
        }
        return match registry.create_database(&name_lower) {
            Ok(_) => Some(Ok(CypherResult {
                columns: vec![],
                rows: vec![],
                query_type: QueryType::Schema,
                stats: QueryStats::default(),
                plan: None,
                bound: Vec::new(),
            })),
            Err(msg) => Some(Err(Status::already_exists(msg))),
        };
    }

    // Handle DROP DATABASE
    if let Some(rest) = trimmed_upper.strip_prefix("DROP DATABASE") {
        let name = rest.trim().trim_end_matches(';').trim();
        if name.is_empty() {
            return Some(Err(Status::invalid_argument(
                "DROP DATABASE requires a database name",
            )));
        }
        let name_lower = name.to_lowercase();
        return match registry.drop_database(&name_lower, current_db_name) {
            Ok(()) => Some(Ok(CypherResult {
                columns: vec![],
                rows: vec![],
                query_type: QueryType::Schema,
                stats: QueryStats::default(),
                plan: None,
                bound: Vec::new(),
            })),
            Err(msg) => Some(Err(Status::failed_precondition(msg))),
        };
    }

    // Handle LOAD DATABASE <name> FROM '<file>'
    if let Some(rest) = trimmed_upper.strip_prefix("LOAD DATABASE") {
        let rest = rest.trim().trim_end_matches(';').trim();
        let parts: Vec<&str> = rest.splitn(2, "FROM").collect();
        if parts.len() != 2 {
            return Some(Err(Status::invalid_argument(
                "LOAD DATABASE requires: LOAD DATABASE <name> FROM '<file>'",
            )));
        }
        let name = parts[0].trim();
        let file_upper = parts[1].trim().trim_matches('\'').trim_matches('"');
        if name.is_empty() || file_upper.is_empty() {
            return Some(Err(Status::invalid_argument(
                "LOAD DATABASE requires a database name and file path",
            )));
        }
        let name_lower = name.to_lowercase();
        if !name_lower
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Some(Err(Status::invalid_argument(format!(
                "invalid database name '{}': must contain only alphanumeric characters and underscores",
                name_lower
            ))));
        }
        // Extract original-case file path from query_trimmed
        let file_orig = {
            let orig_upper = query_trimmed.to_uppercase();
            if let Some(idx) = orig_upper.rfind("FROM") {
                query_trimmed[idx + 4..]
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .trim_matches('\'')
                    .trim_matches('"')
                    .to_string()
            } else {
                file_upper.to_string()
            }
        };
        return match registry.load_database(&name_lower, &file_orig) {
            Ok(_) => Some(Ok(CypherResult {
                columns: vec![],
                rows: vec![],
                query_type: QueryType::Schema,
                stats: QueryStats::default(),
                plan: None,
                bound: Vec::new(),
            })),
            Err(msg) => {
                if msg.contains("already exists") {
                    Some(Err(Status::already_exists(msg)))
                } else {
                    Some(Err(Status::invalid_argument(msg)))
                }
            }
        };
    }

    // Handle SHOW ALIASES
    if trimmed_upper == "SHOW ALIASES" || trimmed_upper == "SHOW ALIASES;" {
        let aliases = registry.list_aliases();
        let rows: Vec<Vec<CypherValue>> = aliases
            .into_iter()
            .map(|(alias, database)| {
                vec![CypherValue::String(alias), CypherValue::String(database)]
            })
            .collect();
        return Some(Ok(CypherResult {
            columns: vec!["alias".into(), "database".into()],
            rows,
            query_type: QueryType::Read,
            stats: QueryStats::default(),
            plan: None,
            bound: Vec::new(),
        }));
    }

    // Handle CREATE ALIAS <name> FOR DATABASE <target>
    if let Some(rest) = trimmed_upper.strip_prefix("CREATE ALIAS") {
        let rest = rest.trim().trim_end_matches(';').trim();
        let parts: Vec<&str> = rest.splitn(2, "FOR DATABASE").collect();
        if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
            return Some(Err(Status::invalid_argument(
                "CREATE ALIAS requires: CREATE ALIAS <name> FOR DATABASE <target>",
            )));
        }
        let alias = parts[0].trim().to_lowercase();
        let target = parts[1].trim().to_lowercase();
        if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Some(Err(Status::invalid_argument(format!(
                "invalid alias name '{}': must contain only alphanumeric characters and underscores",
                alias
            ))));
        }
        return match registry.create_alias(&alias, &target) {
            Ok(()) => Some(Ok(CypherResult {
                columns: vec![],
                rows: vec![],
                query_type: QueryType::Schema,
                stats: QueryStats::default(),
                plan: None,
                bound: Vec::new(),
            })),
            Err(msg) => {
                if msg.contains("already") {
                    Some(Err(Status::already_exists(msg)))
                } else {
                    Some(Err(Status::not_found(msg)))
                }
            }
        };
    }

    // Handle DROP ALIAS <name>
    if let Some(rest) = trimmed_upper.strip_prefix("DROP ALIAS") {
        let name = rest.trim().trim_end_matches(';').trim();
        if name.is_empty() {
            return Some(Err(Status::invalid_argument("DROP ALIAS requires a name")));
        }
        let name_lower = name.to_lowercase();
        return match registry.drop_alias(&name_lower) {
            Ok(()) => Some(Ok(CypherResult {
                columns: vec![],
                rows: vec![],
                query_type: QueryType::Schema,
                stats: QueryStats::default(),
                plan: None,
                bound: Vec::new(),
            })),
            Err(msg) => Some(Err(Status::not_found(msg))),
        };
    }

    // Handle ALTER ALIAS <name> SET DATABASE <target>
    if let Some(rest) = trimmed_upper.strip_prefix("ALTER ALIAS") {
        let rest = rest.trim().trim_end_matches(';').trim();
        let parts: Vec<&str> = rest.splitn(2, "SET DATABASE").collect();
        if parts.len() != 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
            return Some(Err(Status::invalid_argument(
                "ALTER ALIAS requires: ALTER ALIAS <name> SET DATABASE <target>",
            )));
        }
        let alias = parts[0].trim().to_lowercase();
        let new_target = parts[1].trim().to_lowercase();
        return match registry.alter_alias(&alias, &new_target) {
            Ok(()) => Some(Ok(CypherResult {
                columns: vec![],
                rows: vec![],
                query_type: QueryType::Schema,
                stats: QueryStats::default(),
                plan: None,
                bound: Vec::new(),
            })),
            Err(msg) => Some(Err(Status::not_found(msg))),
        };
    }

    // Strip "CALL " prefix and trailing "()" for procedure matching
    let call_body = if query_trimmed.len() > 5
        && query_trimmed.as_bytes()[..5].eq_ignore_ascii_case(b"CALL ")
    {
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
            let read_tx = match store.begin_read() {
                Ok(tx) => tx,
                Err(e) => return Some(Err(Status::resource_exhausted(e.to_string()))),
            };
            let strings = GraphView::strings(&read_tx);
            let mut labels = std::collections::BTreeSet::new();
            for nid in GraphView::nodes_vec(&read_tx) {
                if let Ok(node) = GraphView::get_node(&read_tx, nid) {
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
            let read_tx = match store.begin_read() {
                Ok(tx) => tx,
                Err(e) => return Some(Err(Status::resource_exhausted(e.to_string()))),
            };
            let strings = GraphView::strings(&read_tx);
            let mut types = std::collections::BTreeSet::new();
            for eid in GraphView::all_edges_vec(&read_tx) {
                if let Ok(edge) = GraphView::get_edge(&read_tx, eid) {
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
            let read_tx = match store.begin_read() {
                Ok(tx) => tx,
                Err(e) => return Some(Err(Status::resource_exhausted(e.to_string()))),
            };
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
            let rows: Vec<Vec<CypherValue>> = keys
                .into_iter()
                .map(|k| vec![CypherValue::String(k)])
                .collect();
            (vec!["propertyKey".into()], rows, QueryType::Read)
        }
        _ => return None,
    };

    Some(Ok(CypherResult {
        columns,
        rows,
        query_type,
        stats: QueryStats::default(),
        plan: None,
        bound: Vec::new(),
    }))
}

// -- Helper: execute query in auto-commit or explicit tx --------------

/// Runs an auto-commit statement under its entered context `ctx`.
pub(super) fn execute_auto_commit(
    db_entry: &DatabaseEntry,
    query: &str,
    params: &HashMap<String, CypherValue>,
    read_only: bool,
    ctx: &traverse_cypher::query::QueryContext,
) -> Result<(CypherResult, Option<String>), Status> {
    // Enforce read-only databases
    if db_entry.read_only && is_write_query(query) {
        return Err(Status::permission_denied(
            "database is read-only".to_string(),
        ));
    }
    let store = &db_entry.store;
    if read_only {
        let mut read_tx = store
            .begin_read()
            .map_err(|e| Status::resource_exhausted(e.to_string()))?;
        let result = traverse_cypher::execute_with_params(&mut read_tx, query, params)
            .map_err(cypher_err_to_status)?;
        Ok((result, None))
    } else if is_write_query(query) {
        let mut write_tx =
            crate::statement::begin_write(store, ctx).map_err(cypher_err_to_status)?;
        let result = traverse_cypher::execute_with_params(&mut write_tx, query, params)
            .map_err(cypher_err_to_status)?;
        // Invalidate plan cache if schema changed
        if result.stats.indexes_added != 0
            || result.stats.indexes_removed != 0
            || result.stats.constraints_added != 0
            || result.stats.constraints_removed != 0
        {
            db_entry.plan_cache.clear();
        }
        let ts = write_tx
            .commit()
            .map_err(|e| Status::internal(e.to_string()))?;
        db_entry.mark_dirty();
        let bookmark = format!("FB:{}:{}", ts.physical, ts.logical);
        Ok((result, Some(bookmark)))
    } else {
        let mut read_tx = store
            .begin_read()
            .map_err(|e| Status::resource_exhausted(e.to_string()))?;
        let result = traverse_cypher::execute_with_params(&mut read_tx, query, params)
            .map_err(cypher_err_to_status)?;
        Ok((result, None))
    }
}
