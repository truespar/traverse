//! APOC compatibility: a curated subset of APOC core's most-used function
//! namespaces, implemented natively for Neo4j migrations: `apoc.coll.*`,
//! `apoc.text.*`, `apoc.map.*`, `apoc.convert.*`, `apoc.date.*`,
//! `apoc.number.*`, `apoc.util.*`, plus graph-aware helpers
//! (`apoc.label.exists`, `apoc.node.degree`, ...).
//!
//! Behavior matches APOC core 2026.05 on Neo4j. Names match case-insensitively
//! (the parser lowercases dotted names). APOC null semantics: a NULL principal
//! argument gives NULL, not an error; out-of-range access is lenient.
//!
//! One module per namespace (or family of small ones), each with its own dispatch.

mod coll;
mod convert;
mod create;
mod date;
mod graph;
mod map;
mod number;
mod text;
mod util;

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

pub(crate) use convert::{cypher_to_json_pure, json_to_cypher, xml_to_map};
pub(crate) use util::static_store;

/// Evaluates a pure `apoc.*` function; `None` for names not implemented here (the caller raises
/// unknown-function).
pub(crate) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    match namespace(name) {
        "apoc.coll" => coll::eval(name, args),
        "apoc.text" => text::eval(name, args),
        "apoc.map" => map::eval(name, args),
        "apoc.convert" | "apoc.data" | "apoc.json" | "apoc.meta" | "apoc.xml" => {
            convert::eval(name, args)
        }
        "apoc.date" | "apoc.temporal" => date::eval(name, args),
        "apoc.bitwise" | "apoc.math" | "apoc.number" | "apoc.scoring" => number::eval(name, args),
        "apoc.static" | "apoc.util" | "apoc.version" => util::eval(name, args),
        "apoc.create" => create::eval(name, args),
        "apoc.any" | "apoc.path" => graph::eval(name, args),
        _ => None,
    }
}

/// Evaluates an `apoc.*` function that reads the graph (entity access); `None` for names not
/// implemented here.
pub(crate) fn eval_graph(
    name: &str,
    args: &[CypherValue],
    graph: &dyn GraphView,
) -> Option<Result<CypherValue, CypherError>> {
    match namespace(name) {
        "apoc.coll" => coll::eval_graph(name, args, graph),
        "apoc.map" => map::eval_graph(name, args, graph),
        "apoc.convert" | "apoc.meta" => convert::eval_graph(name, args, graph),
        "apoc.hashing" => util::eval_graph(name, args, graph),
        "apoc.create" => create::eval_graph(name, args, graph),
        "apoc.any" | "apoc.diff" | "apoc.label" | "apoc.node" | "apoc.nodes" | "apoc.rel"
        | "apoc.schema" => graph::eval_graph(name, args, graph),
        _ => None,
    }
}

/// `apoc.<namespace>` of a function name, or the whole name for the top-level ones (`apoc.version`).
fn namespace(name: &str) -> &str {
    match name.match_indices('.').nth(1) {
        Some((end, _)) => &name[..end],
        None => name,
    }
}

/// Edge id of an edge-ish value.
fn rel_id_of(v: Option<&CypherValue>) -> Option<traverse_core::graph::types::EdgeId> {
    match v {
        Some(CypherValue::EdgeRef { id, .. }) => Some(*id),
        Some(CypherValue::Edge { id, .. }) => Some(*id),
        _ => None,
    }
}

fn with_list(
    args: &[CypherValue],
    f: impl FnOnce(&[CypherValue]) -> CypherValue,
) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::List(l)) => Ok(f(l)),
        Some(CypherValue::Null) | None => Ok(CypherValue::Null),
        Some(other) => Err(CypherError::type_error(format!(
            "expected a LIST argument, got {}",
            other.type_name()
        ))),
    }
}

fn with_str(
    args: &[CypherValue],
    f: impl FnOnce(&str) -> CypherValue,
) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::String(s)) => Ok(f(s)),
        Some(CypherValue::Null) | None => Ok(CypherValue::Null),
        Some(other) => Err(CypherError::type_error(format!(
            "expected a STRING argument, got {}",
            other.type_name()
        ))),
    }
}

fn str_arg(args: &[CypherValue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(CypherValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn int_arg(args: &[CypherValue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(CypherValue::Integer(n)) => Some(*n),
        Some(CypherValue::Float(f)) => Some(*f as i64),
        _ => None,
    }
}

fn num_of(v: &CypherValue) -> Option<f64> {
    match v {
        CypherValue::Integer(i) => Some(*i as f64),
        CypherValue::Float(f) => Some(*f),
        _ => None,
    }
}

fn dedup_keep_order(l: &[CypherValue]) -> Vec<CypherValue> {
    let mut out: Vec<CypherValue> = Vec::with_capacity(l.len());
    for v in l {
        if !out.contains(v) {
            out.push(v.clone());
        }
    }
    out
}

fn node_id_of(v: &CypherValue) -> Option<traverse_core::graph::types::NodeId> {
    match v {
        CypherValue::NodeRef(id) => Some(*id),
        CypherValue::Node { id, .. } => Some(*id),
        CypherValue::Integer(i) if *i >= 0 => Some(traverse_core::graph::types::NodeId(*i as u64)),
        _ => None,
    }
}

fn cheap_rand(n: usize) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
        >> 33) as usize
        % n.max(1)
}

fn as_map(v: Option<&CypherValue>) -> Option<&BTreeMap<String, CypherValue>> {
    match v {
        Some(CypherValue::Map(m)) => Some(m),
        _ => None,
    }
}

/// Map key from a value: strings as they are, anything else rendered.
pub(crate) fn key_string(v: &CypherValue) -> String {
    match v {
        CypherValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// (kind, name) pairs of the implemented APOC surface, for `apoc.help`.
pub(crate) fn help_catalog() -> Vec<(&'static str, &'static str)> {
    // names only, deliberately coarse
    const FN_NAMESPACES: &[&str] = &[
        "apoc.coll.*",
        "apoc.text.*",
        "apoc.map.*",
        "apoc.convert.*",
        "apoc.date.*",
        "apoc.number.*",
        "apoc.util.*",
        "apoc.math.*",
        "apoc.agg.*",
        "apoc.node.*",
        "apoc.nodes.*",
        "apoc.rel.*",
        "apoc.path.*",
        "apoc.meta.*",
        "apoc.schema.*",
        "apoc.temporal.*",
        "apoc.hashing.*",
        "apoc.scoring.*",
        "apoc.any.*",
        "apoc.bitwise.*",
        "apoc.json.*",
        "apoc.diff.*",
        "apoc.data.*",
        "apoc.create.*",
    ];
    const PROC_NAMESPACES: &[&str] = &[
        "apoc.create.*",
        "apoc.merge.*",
        "apoc.refactor.*",
        "apoc.cypher.*",
        "apoc.periodic.*",
        "apoc.path.*",
        "apoc.meta.*",
        "apoc.graph.*",
        "apoc.nodes.*",
        "apoc.neighbors.*",
        "apoc.atomic.*",
        "apoc.search.*",
        "apoc.lock.*",
        "apoc.schema.*",
        "apoc.coll.*",
        "apoc.load.json*",
        "apoc.util.*",
        "apoc.when",
        "apoc.case",
        "apoc.help",
        "apoc.stats.degrees",
        "apoc.math.regr",
        "apoc.algo.*",
    ];
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    for n in FN_NAMESPACES {
        out.push(("function", n));
    }
    for n in PROC_NAMESPACES {
        out.push(("procedure", n));
    }
    out
}

/// Ids for virtual nodes and relationships (APOC vNode / vRelationship), functions and procedures
/// alike: from `VIRTUAL_ID_BASE` up, never a real entity's.
pub(crate) fn next_virtual_id() -> traverse_core::graph::types::NodeId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(crate::executor::VIRTUAL_ID_BASE);
    traverse_core::graph::types::NodeId(NEXT.fetch_add(1, Ordering::Relaxed))
}
