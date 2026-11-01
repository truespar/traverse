//! `apoc.trigger.*`: registered Cypher statements run at the end of every
//! successful top-level WRITE query, with the transaction's mutation events
//! bound as parameters (`$createdNodes`, `$deletedNodes`, `$assignedLabels`,
//! `$assignedNodeProperties`, ...).
//!
//! Differences from APOC:
//! * The registry lives as long as the PROCESS (like `apoc.static.*`): not
//!   persisted, shared across databases.
//! * Triggers fire synchronously at the query boundary: `before` / `after` /
//!   `afterAsync` phases all fire there; `rollback` triggers never fire (a
//!   failed query never gets that far).
//! * Deleted entities arrive as MATERIALIZED snapshots in the virtual-id region,
//!   so trigger statements can read their labels and properties (APOC passes
//!   virtual entities too).
//! * Trigger statements run with capture off, so triggers don't cascade (APOC
//!   guards the same way).

use std::collections::BTreeMap;
use std::sync::RwLock;

use traverse_core::graph::events::GraphEvent;
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

/// A registered trigger.
#[derive(Debug, Clone)]
pub struct Trigger {
    /// Name (registry key).
    pub name: String,
    /// Cypher statement to run.
    pub statement: String,
    /// Selector map as given (`{phase: 'after'}`, ...).
    pub selector: BTreeMap<String, CypherValue>,
    /// Extra params from the config map (`{params: {...}}`).
    pub params: BTreeMap<String, CypherValue>,
    /// Paused triggers stay registered but don't fire.
    pub paused: bool,
}

fn registry() -> &'static RwLock<Vec<Trigger>> {
    use std::sync::OnceLock;
    static REG: OnceLock<RwLock<Vec<Trigger>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(Vec::new()))
}

/// Any non-paused trigger installed? (cheap check)
pub fn any_active() -> bool {
    registry()
        .read()
        .map(|r| r.iter().any(|t| !t.paused))
        .unwrap_or(false)
}

/// Installs or replaces a trigger; returns the previous definition.
pub fn install(t: Trigger) -> Option<Trigger> {
    let mut reg = registry().write().ok()?;
    let prev = reg
        .iter()
        .position(|x| x.name == t.name)
        .map(|i| reg.remove(i));
    reg.push(t);
    prev
}

/// Removes a trigger by name, returning it.
pub fn remove(name: &str) -> Option<Trigger> {
    let mut reg = registry().write().ok()?;
    reg.iter()
        .position(|x| x.name == name)
        .map(|i| reg.remove(i))
}

/// Removes all triggers, returning them.
pub fn remove_all() -> Vec<Trigger> {
    registry()
        .write()
        .map(|mut r| std::mem::take(&mut *r))
        .unwrap_or_default()
}

/// All triggers, sorted by name.
pub fn list() -> Vec<Trigger> {
    let mut v = registry().read().map(|r| r.clone()).unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Pauses / resumes a trigger; returns the updated definition.
pub fn set_paused(name: &str, paused: bool) -> Option<Trigger> {
    let mut reg = registry().write().ok()?;
    let t = reg.iter_mut().find(|x| x.name == name)?;
    t.paused = paused;
    Some(t.clone())
}

thread_local! {
    /// Re-entrancy guard: no triggers while firing triggers.
    static FIRING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TX_COUNTER: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// True while trigger statements run.
pub fn firing() -> bool {
    FIRING.with(std::cell::Cell::get)
}

/// APOC trigger parameter map from a drained event log.
fn build_params(events: &[GraphEvent]) -> BTreeMap<String, CypherValue> {
    use CypherValue as V;
    let mut created_nodes = Vec::new();
    let created_rels = Vec::new();
    let mut deleted_nodes = Vec::new();
    let mut deleted_rels = Vec::new();
    let mut assigned_labels: BTreeMap<String, Vec<V>> = BTreeMap::new();
    let mut removed_labels: BTreeMap<String, Vec<V>> = BTreeMap::new();
    let mut assigned_node_props: BTreeMap<String, Vec<V>> = BTreeMap::new();
    let mut removed_node_props: BTreeMap<String, Vec<V>> = BTreeMap::new();
    let mut assigned_rel_props: BTreeMap<String, Vec<V>> = BTreeMap::new();
    let mut removed_rel_props: BTreeMap<String, Vec<V>> = BTreeMap::new();

    // deleted entities become materialized values with virtual ids, so property/label access
    // needs no graph lookup
    for ev in events {
        match ev {
            GraphEvent::NodeCreated(id) => created_nodes.push(V::NodeRef(*id)),
            GraphEvent::EdgeCreated(_) => {}
            GraphEvent::NodeDeleted {
                labels, properties, ..
            } => {
                deleted_nodes.push(V::Node {
                    id: crate::apoc::next_virtual_id(),
                    labels: labels.clone(),
                    properties: properties.iter().cloned().collect(),
                });
            }
            GraphEvent::EdgeDeleted {
                edge_type,
                source,
                target,
                properties,
                ..
            } => {
                deleted_rels.push(V::Edge {
                    id: traverse_core::graph::types::EdgeId(crate::apoc::next_virtual_id().0),
                    edge_type: edge_type.clone(),
                    source: *source,
                    target: *target,
                    properties: properties.iter().cloned().collect(),
                });
            }
            GraphEvent::LabelAdded { node, label } => {
                assigned_labels
                    .entry(label.clone())
                    .or_default()
                    .push(V::NodeRef(*node));
            }
            GraphEvent::LabelRemoved { node, label } => {
                removed_labels
                    .entry(label.clone())
                    .or_default()
                    .push(V::NodeRef(*node));
            }
            GraphEvent::NodePropChanged {
                node,
                key,
                old,
                new,
            } => {
                let mut m = BTreeMap::new();
                m.insert("node".to_string(), V::NodeRef(*node));
                m.insert("key".to_string(), V::String(key.clone()));
                m.insert(
                    "old".to_string(),
                    old.as_ref()
                        .map(CypherValue::from_property)
                        .unwrap_or(V::Null),
                );
                m.insert(
                    "new".to_string(),
                    new.as_ref()
                        .map(CypherValue::from_property)
                        .unwrap_or(V::Null),
                );
                let bucket = if new.is_some() {
                    &mut assigned_node_props
                } else {
                    &mut removed_node_props
                };
                bucket.entry(key.clone()).or_default().push(V::Map(m));
            }
            GraphEvent::EdgePropChanged {
                edge,
                key,
                old,
                new,
            } => {
                let mut m = BTreeMap::new();
                m.insert("key".to_string(), V::String(key.clone()));
                m.insert("relationshipId".to_string(), V::Integer(edge.0 as i64));
                m.insert(
                    "old".to_string(),
                    old.as_ref()
                        .map(CypherValue::from_property)
                        .unwrap_or(V::Null),
                );
                m.insert(
                    "new".to_string(),
                    new.as_ref()
                        .map(CypherValue::from_property)
                        .unwrap_or(V::Null),
                );
                let bucket = if new.is_some() {
                    &mut assigned_rel_props
                } else {
                    &mut removed_rel_props
                };
                bucket.entry(key.clone()).or_default().push(V::Map(m));
            }
        }
    }

    // EdgeCreated events carry only ids; refs are materialized at fire time (the caller passes the
    // graph), collected separately below
    let map_of =
        |m: BTreeMap<String, Vec<V>>| V::Map(m.into_iter().map(|(k, v)| (k, V::List(v))).collect());
    let mut out = BTreeMap::new();
    out.insert("createdNodes".to_string(), V::List(created_nodes));
    out.insert("createdRelationships".to_string(), V::List(created_rels));
    out.insert("deletedNodes".to_string(), V::List(deleted_nodes));
    out.insert("deletedRelationships".to_string(), V::List(deleted_rels));
    out.insert("assignedLabels".to_string(), map_of(assigned_labels));
    out.insert("removedLabels".to_string(), map_of(removed_labels));
    out.insert(
        "assignedNodeProperties".to_string(),
        map_of(assigned_node_props),
    );
    out.insert(
        "removedNodeProperties".to_string(),
        map_of(removed_node_props),
    );
    out.insert(
        "assignedRelationshipProperties".to_string(),
        map_of(assigned_rel_props),
    );
    out.insert(
        "removedRelationshipProperties".to_string(),
        map_of(removed_rel_props),
    );
    out
}

/// Fires all active triggers against the drained event log. Trigger statement errors propagate,
/// naming the trigger; the query itself already succeeded and its effects stand.
pub fn fire(graph: &mut dyn GraphView, events: Vec<GraphEvent>) -> Result<(), CypherError> {
    if events.is_empty() || firing() {
        return Ok(());
    }
    let triggers: Vec<Trigger> = list().into_iter().filter(|t| !t.paused).collect();
    if triggers.is_empty() {
        return Ok(());
    }
    FIRING.with(|f| f.set(true));
    let result = (|| {
        let mut params = build_params(&events);
        // materialize created relationship refs (graph available now)
        let created_rels: Vec<CypherValue> = events
            .iter()
            .filter_map(|ev| match ev {
                GraphEvent::EdgeCreated(id) => {
                    let e = graph.get_edge(*id).ok()?;
                    Some(CypherValue::EdgeRef {
                        id: *id,
                        edge_type_id: e.edge_type,
                        source: e.source,
                        target: e.target,
                    })
                }
                _ => None,
            })
            .collect();
        params.insert(
            "createdRelationships".to_string(),
            CypherValue::List(created_rels),
        );
        let tx_id = TX_COUNTER.with(|c| {
            let v = c.get() + 1;
            c.set(v);
            v
        });
        params.insert("transactionId".to_string(), CypherValue::Integer(tx_id));
        params.insert(
            "commitTime".to_string(),
            CypherValue::Integer(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            ),
        );

        for t in &triggers {
            // rollback-phase triggers never fire on success
            if matches!(t.selector.get("phase"), Some(CypherValue::String(p)) if p == "rollback") {
                continue;
            }
            let mut p: std::collections::HashMap<String, CypherValue> =
                params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, v) in &t.params {
                p.insert(k.clone(), v.clone());
            }
            // part of the statement that fired it: its limits and clock, the trigger's parameters
            let query = crate::query::enter_nested(p);
            let r = crate::stack::with_stack_for_query(&t.statement, || {
                let (stmt, parse_depth) = crate::parser::parse_with_stats(&t.statement)?;
                let graph = &mut *graph;
                crate::stack::with_stack_for_ast(&parse_depth, move || {
                    crate::validate::validate(&stmt)?;
                    let hints = crate::build_index_hints_for(&*graph);
                    let plan = crate::planner::plan_statement_with_hints(&stmt, &hints)?;
                    crate::stack::run_deep_plan(plan, |plan| {
                        let plan = crate::optimizer::optimize(plan, &*graph, &hints);
                        crate::executor::execute_plan(&plan, graph)
                    })
                })
            });
            drop(query);
            if let Err(e) = r {
                return Err(CypherError::runtime(format!(
                    "trigger `{}` failed: {e}",
                    t.name
                )));
            }
        }
        Ok(())
    })();
    FIRING.with(|f| f.set(false));
    result
}
