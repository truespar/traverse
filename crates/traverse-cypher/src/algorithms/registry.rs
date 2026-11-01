//! Compute-on-demand registry for graph algorithms.
//!
//! Sits next to the data-driven [`crate::procedure`] registry (TCK fixture
//! procedures). `CallProcedureOp` checks this one first for `traverse.*` names;
//! anything else falls through to the TCK path.
//!
//! Thread-local: each thread that initializes the executor calls
//! `register_builtins()` once, so every thread sees the same algorithms.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use traverse_core::tx::view::GraphView;

use super::procedure::ProcedureOutput;
use super::{AlgorithmError, AlgorithmMetadata};

/// Type-erased entry point for one algorithm + mode.
///
/// Takes the procedure args (already evaluated to CypherValues) and the graph
/// view, returns a materialized [`ProcedureOutput`] (rows + column metadata).
/// `CallProcedureOp` then binds yield columns to symtab slots, as for TCK
/// procedures.
pub type AlgorithmRunner = Arc<
    dyn Fn(
            &std::collections::BTreeMap<String, crate::types::CypherValue>,
            &mut dyn GraphView,
            Option<&[&str]>,
        ) -> Result<ProcedureOutput, AlgorithmError>
        + Send
        + Sync,
>;

/// One entry per algorithm, all modes.
pub struct AlgorithmEntry {
    /// Name, category, parameters and output shape, as listed by `/api/algorithms`.
    pub metadata: AlgorithmMetadata,
    /// Runs `traverse.<name>.stream`: one row per result entry.
    pub stream: AlgorithmRunner,
    /// Runs `traverse.<name>.stats`: one summary row.
    pub stats: AlgorithmRunner,
    /// Only for output kinds with `is_writable() == true`.
    pub write: Option<AlgorithmRunner>,
    /// Stream over a PRE-BUILT projection (materialized GDS projections): skips BaseConfig parsing
    /// and projection construction. Write mode stays on the live-graph runners.
    pub stream_on: ProjectionRunner,
    /// Stats over a pre-built projection; see `stream_on`.
    pub stats_on: ProjectionRunner,
}

/// Runner over a caller-supplied [`AlgorithmGraph`](super::projection::AlgorithmGraph) projection.
pub type ProjectionRunner = Arc<
    dyn Fn(
            &std::collections::BTreeMap<String, crate::types::CypherValue>,
            &dyn super::projection::AlgorithmGraph,
            Option<&[&str]>,
        ) -> Result<ProcedureOutput, AlgorithmError>
        + Send
        + Sync,
>;

thread_local! {
    static REGISTRY: RefCell<HashMap<String, Arc<AlgorithmEntry>>> = RefCell::new(HashMap::new());
}

/// Registers an algorithm under `traverse.<name>.stream`, `traverse.<name>.stats` and, when
/// writable, `traverse.<name>.write`.
pub fn register(entry: AlgorithmEntry) {
    let name = entry.metadata.name.to_string();
    REGISTRY.with(|r| {
        r.borrow_mut().insert(name, Arc::new(entry));
    });
}

/// Algorithm by unqualified name (e.g. `"pageRank"`).
pub fn get(name: &str) -> Option<Arc<AlgorithmEntry>> {
    REGISTRY.with(|r| r.borrow().get(name).cloned())
}

/// All registered algorithms (for `/api/algorithms`).
pub fn list() -> Vec<Arc<AlgorithmEntry>> {
    REGISTRY.with(|r| r.borrow().values().cloned().collect())
}

/// Clears the registry (tests).
pub fn clear() {
    REGISTRY.with(|r| r.borrow_mut().clear());
}

/// Splits a qualified procedure name into `(algorithm_name, mode)`; `None` unless it's
/// `traverse.<algo>.<mode>`.
pub fn parse_qualified(name: &str) -> Option<(&str, ProcedureMode)> {
    let rest = name.strip_prefix("traverse.")?;
    let (algo, mode_str) = rest.rsplit_once('.')?;
    let mode = match mode_str {
        "stream" => ProcedureMode::Stream,
        "stats" => ProcedureMode::Stats,
        "write" => ProcedureMode::Write,
        _ => return None,
    };
    Some((algo, mode))
}

/// Execution mode, the last segment of `traverse.<algo>.<mode>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureMode {
    /// Per-entity results as rows.
    Stream,
    /// One row of summary statistics.
    Stats,
    /// Writes results to a node property, returns write statistics.
    Write,
}
