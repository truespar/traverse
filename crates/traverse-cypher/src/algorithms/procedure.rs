//! Adapter exposing [`GraphAlgorithm`]s as Cypher procedures with `.stream` /
//! `.stats` / `.write` modes.
//!
//! # Pipeline
//!
//! 1. `CallProcedureOp` sees a `traverse.` procedure and calls [`run_for_call`].
//! 2. The name splits into `(algorithm, mode)`; the [`AlgorithmEntry`] comes from
//!    the registry.
//! 3. The mode runner:
//!    - parses `BaseConfig` + the algorithm config from the argument map;
//!    - takes a parallel-safe `&(dyn GraphView + Sync)` via `GraphView::as_sync_ref` and builds
//!      an [`AlgorithmGraphView`];
//!    - calls `GraphAlgorithm::run` with a cancellation token tied to the query timeout / memory
//!      checks;
//!    - turns the typed [`AlgorithmResult`] into named rows (`Vec<HashMap<String, CypherValue>>`),
//!      symtab-free, so YIELD binds the same way as for TCK procedures;
//!    - `.write`: once compute is done (immutable borrow ended), re-borrows the view mutably and
//!      bulk-writes via [`AlgorithmResult::write_back`].
//!
//! Generic over `A: GraphAlgorithm`, so registering an algorithm is one call.

use std::collections::BTreeMap;
use std::sync::Arc;
// `web_time::Instant` is `std::time::Instant` natively and a performance.now() shim on wasm32,
// where the std clock panics
use web_time::Instant;

use traverse_core::tx::view::GraphView;

use crate::types::CypherValue;

use super::cancellation::CancellationToken;
use super::config::BaseConfig;
use super::projection::AlgorithmGraphView;
use super::registry::{AlgorithmEntry, AlgorithmRunner, ProcedureMode};
use super::result::AlgorithmResult;
use super::{AlgorithmError, GraphAlgorithm};

// -- Entry point used by the executor ------------------------------------

/// Result of a `traverse.*` procedure call:
///
/// * `rows`: result rows keyed by column name.
/// * `columns`: declared column order (used by CallProcedureOp without YIELD).
pub struct ProcedureOutput {
    /// (column, value) pairs per row. Cow keeps stream keys as borrowed `&'static str` (no
    /// allocation) while stats/write rows may own dynamic keys.
    pub rows: Vec<Vec<(std::borrow::Cow<'static, str>, CypherValue)>>,
    /// Declared column order, used without an explicit `YIELD`.
    pub columns: Vec<&'static str>,
}

thread_local! {
    /// First-touch flag: each worker thread registers the built-in algorithms on its first call
    /// (thread-local, like `crate::procedure`).
    static REGISTRY_INITIALIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline]
fn ensure_registered() {
    REGISTRY_INITIALIZED.with(|cell| {
        if !cell.get() {
            super::register_builtins();
            cell.set(true);
        }
    });
}

/// Dispatcher:
///
/// * `Some(Ok(out))`: handled by the algorithms registry.
/// * `Some(Err(e))`: handled but failed (unknown name, bad config, cancelled, ...).
/// * `None`: not an algorithm procedure; fall back to the data-driven
///   `crate::procedure` registry.
pub fn run_for_call(
    procedure_name: &str,
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
    yield_columns: Option<&[&str]>,
) -> Option<Result<ProcedureOutput, AlgorithmError>> {
    ensure_registered();
    let (algo_name, mode) = super::registry::parse_qualified(procedure_name)?;
    let entry = super::registry::get(algo_name)?;
    Some(invoke(entry, mode, args, view, yield_columns))
}

fn invoke(
    entry: Arc<AlgorithmEntry>,
    mode: ProcedureMode,
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
    yield_columns: Option<&[&str]>,
) -> Result<ProcedureOutput, AlgorithmError> {
    let runner_opt = match mode {
        ProcedureMode::Stream => Some(&entry.stream),
        ProcedureMode::Stats => Some(&entry.stats),
        ProcedureMode::Write => entry.write.as_ref(),
    };
    let runner = runner_opt.ok_or_else(|| {
        AlgorithmError::Other(format!(
            "algorithm `{}` does not support .{} mode",
            entry.metadata.name,
            mode_str(mode),
        ))
    })?;
    runner(args, view, yield_columns)
}

fn mode_str(m: ProcedureMode) -> &'static str {
    match m {
        ProcedureMode::Stream => "stream",
        ProcedureMode::Stats => "stats",
        ProcedureMode::Write => "write",
    }
}

// -- Generic registration -------------------------------------------------

/// Wraps a [`GraphAlgorithm`] as an erased registry [`AlgorithmEntry`]; called once per algorithm
/// from its category's `register()`.
pub fn build_entry<A: GraphAlgorithm>(algorithm: A) -> AlgorithmEntry {
    let algorithm = Arc::new(algorithm);
    let metadata = algorithm.metadata();

    // three runners, each capturing the algorithm Arc and its mode, all with the erased `Fn(args,
    // view) -> Result<ProcedureOutput, _>` signature stored as `AlgorithmRunner = Arc<dyn Fn(...)>`
    let stream_runner: AlgorithmRunner = {
        let alg = algorithm.clone();
        Arc::new(move |args, view, yield_cols| {
            run_mode(&*alg, args, view, ProcedureMode::Stream, yield_cols)
        })
    };

    let stats_runner: AlgorithmRunner = {
        let alg = algorithm.clone();
        Arc::new(move |args, view, _yield_cols| {
            run_mode(&*alg, args, view, ProcedureMode::Stats, None)
        })
    };

    let write_runner: Option<AlgorithmRunner> = if metadata.output_kind.writable() {
        let alg = algorithm.clone();
        Some(Arc::new(move |args, view, _yield_cols| {
            run_mode(&*alg, args, view, ProcedureMode::Write, None)
        }))
    } else {
        None
    };

    let stream_on: super::registry::ProjectionRunner = {
        let alg = algorithm.clone();
        Arc::new(move |args, proj, yield_cols| {
            run_mode_on(&*alg, args, proj, ProcedureMode::Stream, yield_cols)
        })
    };
    let stats_on: super::registry::ProjectionRunner = {
        let alg = algorithm.clone();
        Arc::new(move |args, proj, _yield_cols| {
            run_mode_on(&*alg, args, proj, ProcedureMode::Stats, None)
        })
    };

    AlgorithmEntry {
        metadata,
        stream: stream_runner,
        stats: stats_runner,
        write: write_runner,
        stream_on,
        stats_on,
    }
}

/// [`run_for_call`] over a caller-supplied (materialized) projection. Stream/stats only; write mode
/// needs the live graph.
pub fn run_for_call_on(
    procedure_name: &str,
    args: &BTreeMap<String, CypherValue>,
    proj: &dyn super::projection::AlgorithmGraph,
    yield_columns: Option<&[&str]>,
) -> Option<Result<ProcedureOutput, AlgorithmError>> {
    ensure_registered();
    let (algo_name, mode) = super::registry::parse_qualified(procedure_name)?;
    let entry = super::registry::get(algo_name)?;
    let runner = match mode {
        ProcedureMode::Stream => entry.stream_on.clone(),
        ProcedureMode::Stats => entry.stats_on.clone(),
        ProcedureMode::Write => {
            return Some(Err(AlgorithmError::Other(
                "write mode runs on the live graph, not a materialized projection".into(),
            )));
        }
    };
    Some(runner(args, proj, yield_columns))
}

/// Stream/stats over a pre-built projection, which already encodes labels, types, orientation and
/// weights, so `BaseConfig` isn't consulted.
fn run_mode_on<A: GraphAlgorithm>(
    algorithm: &A,
    args: &BTreeMap<String, CypherValue>,
    proj: &dyn super::projection::AlgorithmGraph,
    mode: ProcedureMode,
    yield_columns: Option<&[&str]>,
) -> Result<ProcedureOutput, AlgorithmError> {
    let algo_config = algorithm.parse_config(&mode_args(args, mode, yield_columns))?;
    let cancel = CancellationToken::current();
    let started = Instant::now();
    let result = algorithm.run(proj, &algo_config, &cancel)?;
    let compute_ms = started.elapsed().as_secs_f64() * 1000.0;
    match mode {
        ProcedureMode::Stream => Ok(materialize_stream(&result, yield_columns)),
        ProcedureMode::Stats => Ok(materialize_stats(&result, compute_ms)),
        ProcedureMode::Write => Err(AlgorithmError::Other(
            "write mode is not available on materialized projections".into(),
        )),
    }
}

/// Internal config key a stats run carries, so an engine can skip what only stream rows read
/// (single-source paths: the paths themselves).
const STATS_ONLY_KEY: &str = "__stats_only";

/// Whether this run is in stats mode (see [`STATS_ONLY_KEY`]).
pub(crate) fn stats_only(params: &BTreeMap<String, CypherValue>) -> bool {
    matches!(params.get(STATS_ONLY_KEY), Some(CypherValue::Bool(true)))
}

/// Internal config key a stream run carries when its YIELD takes none of a path's lists (`nodeIds`,
/// `edgeIds`, `costs`, nor a `path` built from them), so single-source engines skip rebuilding
/// each path.
const NO_PATH_LISTS_KEY: &str = "__no_path_lists";

/// The path columns a stream row builds from the node, edge and cost lists.
const PATH_LIST_COLUMNS: &[&str] = &["nodeIds", "edgeIds", "costs", "path"];

/// Whether nothing reads this run's path lists: stats mode, or a stream yielding none of them (see
/// [`NO_PATH_LISTS_KEY`]).
pub(crate) fn path_lists_unread(params: &BTreeMap<String, CypherValue>) -> bool {
    stats_only(params) || matches!(params.get(NO_PATH_LISTS_KEY), Some(CypherValue::Bool(true)))
}

/// `args` as the algorithm parses them in `mode`, yielding `yield_columns` (`None`: every column).
fn mode_args<'a>(
    args: &'a BTreeMap<String, CypherValue>,
    mode: ProcedureMode,
    yield_columns: Option<&[&str]>,
) -> std::borrow::Cow<'a, BTreeMap<String, CypherValue>> {
    let key = match mode {
        ProcedureMode::Stats => STATS_ONLY_KEY,
        ProcedureMode::Stream
            if yield_columns.is_some_and(|y| !y.iter().any(|c| PATH_LIST_COLUMNS.contains(c))) =>
        {
            NO_PATH_LISTS_KEY
        }
        _ => return std::borrow::Cow::Borrowed(args),
    };
    let mut a = args.clone();
    a.insert(key.to_string(), CypherValue::Bool(true));
    std::borrow::Cow::Owned(a)
}

/// Builds and registers in one call.
pub fn register<A: GraphAlgorithm>(algorithm: A) {
    super::registry::register(build_entry(algorithm));
}

// -- Mode-specific execution ---------------------------------------------

fn run_mode<A: GraphAlgorithm>(
    algorithm: &A,
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
    mode: ProcedureMode,
    yield_columns: Option<&[&str]>,
) -> Result<ProcedureOutput, AlgorithmError> {
    // 1. parse configs
    let base = BaseConfig::parse(args).map_err(AlgorithmError::from)?;
    let algo_config = algorithm.parse_config(&mode_args(args, mode, yield_columns))?;
    let cancel = CancellationToken::current();

    // 2. read phase: compute on an immutable parallel-safe view, scoped so the borrow ends before
    //    `view` is used mutably
    let started = Instant::now();
    let result = {
        let sync_view = view.as_sync_ref().ok_or_else(|| {
            AlgorithmError::Other(
                "graph algorithms require a snapshot view; \
                 invoke CALL inside a read query so as_sync_ref() succeeds"
                    .into(),
            )
        })?;
        let proj = AlgorithmGraphView::new(sync_view, &base)?;
        algorithm.run(&proj, &algo_config, &cancel)?
    };
    let compute_ms = started.elapsed().as_secs_f64() * 1000.0;

    // 3. mode-specific materialization
    match mode {
        ProcedureMode::Stream => Ok(materialize_stream(&result, yield_columns)),
        ProcedureMode::Stats => Ok(materialize_stats(&result, compute_ms)),
        ProcedureMode::Write => {
            // write phase: bulk-write through the now-free `view`
            let property = base.write_property.clone().ok_or_else(|| {
                AlgorithmError::Config("writeProperty is required for .write mode".into())
            })?;
            let write_started = Instant::now();
            let stats = result.write_back(view, &property)?;
            let write_ms = write_started.elapsed().as_secs_f64() * 1000.0;
            Ok(materialize_write(&result, compute_ms, write_ms, stats))
        }
    }
}

// -- Materialization helpers ---------------------------------------------

fn materialize_stream<R: AlgorithmResult + ?Sized>(
    result: &R,
    yield_columns: Option<&[&str]>,
) -> ProcedureOutput {
    let columns = result.columns().to_vec();
    let rows = result.stream_rows_filtered(yield_columns);
    ProcedureOutput { rows, columns }
}

fn materialize_stats<R: AlgorithmResult + ?Sized>(result: &R, compute_ms: f64) -> ProcedureOutput {
    let mut stats = result.stats();
    stats.insert("computeMillis".into(), CypherValue::Float(compute_ms));
    let row: Vec<(std::borrow::Cow<'static, str>, CypherValue)> = stats
        .into_iter()
        .map(|(k, v)| (std::borrow::Cow::Owned(k), v))
        .collect();
    ProcedureOutput {
        rows: vec![row],
        // .stats columns differ per algorithm: an empty static slice here, CallProcedureOp uses the
        // row's keys when there's no YIELD
        columns: Vec::new(),
    }
}

fn materialize_write<R: AlgorithmResult + ?Sized>(
    result: &R,
    compute_ms: f64,
    write_ms: f64,
    stats: super::result::WriteStats,
) -> ProcedureOutput {
    let mut row: Vec<(std::borrow::Cow<'static, str>, CypherValue)> = Vec::new();
    for (k, v) in result.stats() {
        row.push((std::borrow::Cow::Owned(k), v));
    }
    row.push((
        std::borrow::Cow::Borrowed("computeMillis"),
        CypherValue::Float(compute_ms),
    ));
    row.push((
        std::borrow::Cow::Borrowed("writeMillis"),
        CypherValue::Float(write_ms),
    ));
    row.push((
        std::borrow::Cow::Borrowed("nodePropertiesWritten"),
        CypherValue::Integer(stats.nodes_written as i64),
    ));
    row.push((
        std::borrow::Cow::Borrowed("propertiesWritten"),
        CypherValue::Integer(stats.properties_written as i64),
    ));
    ProcedureOutput {
        rows: vec![row],
        columns: Vec::new(),
    }
}
