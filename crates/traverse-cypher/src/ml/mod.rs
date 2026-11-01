//! ML pipeline procedures: `traverse.ml.pipeline.*`, shaped like Neo4j GDS's
//! `gds.beta.pipeline.*`:
//!
//! ```text
//! CALL traverse.ml.pipeline.nodeClassification.create('myPipe')
//! CALL traverse.ml.pipeline.nodeClassification.addNodeProperty('myPipe', 'pageRank',
//!      {mutateProperty: 'pr'})
//! CALL traverse.ml.pipeline.nodeClassification.selectFeatures('myPipe', ['age', 'pr'])
//! CALL traverse.ml.pipeline.nodeClassification.addGcn('myPipe', {hidden: {range: [16, 64]}})
//! CALL traverse.ml.pipeline.nodeClassification.train('mygraph', {pipeline: 'myPipe', ...})
//! CALL traverse.ml.pipeline.nodeClassification.predict.stream('mygraph', {modelName: 'm1'})
//! ```
//!
//! Three tasks share the configuration procedures: `nodeClassification`, `nodeRegression` and
//! `linkPrediction`. Training follows GDS: test holdout, k-fold cross-validated model selection
//! over the candidates (random search or successive halving when they declare ranges), the winner
//! retrained on the whole training set (`traverse-ml`'s `selection` module).
//!
//! Beyond the pipelines: `traverse.ml.graphMae.*` (self-supervised node embeddings),
//! `traverse.ml.explain.*` (PGExplainer, counterfactual, subgraph audit), the model catalog
//! `traverse.ml.model.*`, the training jobs (`trainStatus`, `trainJobs`, `trainCancel`) and
//! `traverse.ml.devices()`.
//!
//! Training, prediction, embeddings and explanations run on the CPU or, in builds with GPU
//! support, a GPU: the `device` setting of a call, else the server default
//! (`TRAVERSE_ML_DEVICE`). Graphs too big for full-graph training steps train in neighbor-sampled
//! mini-batches (`batching`, `batchSize`, `fanouts`); evaluation and prediction stay exact.
//!
//! Deliberate differences from GDS:
//!
//! 1. **Namespace**: `traverse.ml.*`, like the other `traverse.*` procedures.
//! 2. **Models**: graph neural nets (`addGcn`, `addGraphSage`, `addRGcn`, `addGat`)
//!    instead of GDS's classical `addLogisticRegression` / `addRandomForest` /
//!    `addMLP`; same map-config shape, ranges included.
//! 3. **Graph name**: no `gds.graph.project` needed; `graphName` is accepted
//!    for compatibility and maps to the active database (everything is in memory);
//!    `nodeLabels` / `relationshipTypes` in the train config narrow it.
//! 4. **Training is a job**: `train` returns a `jobId` at once (poll `trainStatus`),
//!    or blocks and returns the result with `{wait: true}`. Jobs queue: at most
//!    `TRAVERSE_ML_MAX_JOBS` (default 1) train at a time; `trainCancel` stops one.
//! 5. **Pipelines persist** next to the database files (GDS keeps them in memory only).

mod persist;
pub mod registry;
#[cfg(test)]
mod smoke;

#[cfg(feature = "ml")]
mod catalog;
#[cfg(feature = "ml")]
mod data;
#[cfg(feature = "ml")]
pub mod device;
#[cfg(feature = "ml")]
mod embed;
#[cfg(feature = "ml")]
mod explain;
#[cfg(feature = "ml")]
mod jobs;
#[cfg(feature = "ml")]
mod run;

/// How many training jobs may run at once (default 1); the server sets it at startup.
#[cfg(feature = "ml")]
pub use jobs::set_max_running as set_max_training_jobs;

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::algorithms::procedure::ProcedureOutput;
use crate::ml::registry::TaskType;
use crate::types::CypherValue;

/// Error from an ML procedure call; the dispatcher maps it to `CypherError::runtime`.
#[derive(Debug, thiserror::Error)]
pub enum MlError {
    /// No `traverse.ml.*` procedure by this name.
    #[error("ProcedureNotFound: ML procedure {0} is not registered")]
    NotFound(String),
    /// Required positional argument missing.
    #[error("missing required argument '{0}'")]
    MissingArgument(&'static str),
    /// Argument of the wrong type.
    #[error("argument '{name}' has wrong type: expected {expected}, got {actual}")]
    BadArgumentType {
        /// Argument name.
        name: &'static str,
        /// Expected type.
        expected: &'static str,
        /// Supplied type.
        actual: String,
    },
    /// A configuration value out of range or inconsistent.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// `create` with a name already in the catalog.
    #[error("pipeline '{0}' already exists")]
    PipelineExists(String),
    /// No such pipeline.
    #[error("pipeline '{0}' not found")]
    PipelineNotFound(String),
    /// A pipeline used through another task's procedures.
    #[error("pipeline '{pipeline}' is a {actual} pipeline, not {wanted}")]
    WrongTask {
        /// Pipeline name.
        pipeline: String,
        /// Its task.
        actual: TaskType,
        /// The task of the procedure called.
        wanted: TaskType,
    },
    /// No such trained model.
    #[error("model '{0}' not found in session catalog")]
    ModelNotFound(String),
    /// No such training job.
    #[error("training job '{0}' not found")]
    JobNotFound(String),
    /// Training couldn't start or finish; carries the reason.
    #[error("training failed: {0}")]
    TrainingFailed(String),
    /// Prediction failed; carries the reason.
    #[error("prediction failed: {0}")]
    PredictionFailed(String),
    /// A pipeline or model couldn't be written to the data directory; nothing changed.
    #[error("couldn't save to the data directory: {0}")]
    PersistFailed(String),
    /// Training and prediction need the crate's `ml` feature, missing in this build.
    #[error("traverse-cypher was not built with the `ml` feature - train/predict require it")]
    MlFeatureDisabled,
    /// The query running the call stopped.
    #[error("the query {}", .0.as_str())]
    Interrupted(crate::query::Interrupt),
    /// The process went over its memory ceiling.
    #[error("memory limit exceeded")]
    MemoryExhausted,
}

impl MlError {
    /// As the error the calling query fails with; `procedure` names the call in other errors.
    pub fn into_cypher(self, procedure: &str) -> crate::error::CypherError {
        match self {
            MlError::Interrupted(reason) => crate::query::interrupted_error(reason),
            MlError::MemoryExhausted => crate::memory::process_limit_error(),
            other => crate::error::CypherError::runtime(format!(
                "ML procedure `{procedure}` failed: {other}"
            )),
        }
    }
}

/// Dispatches a `traverse.ml.*` call. Arguments are positional (GDS:
/// `create('pipe')`, `addNodeProperty('pipe', 'degree', {})`); each procedure's
/// slots map to a named `BTreeMap` for its handler.
///
/// * `Some(Ok(out))`: handled.
/// * `Some(Err(e))`: handled, failed.
/// * `None`: not an ML procedure; the caller tries the algorithm registry,
///   then the TCK registry.
pub fn run_for_call(
    procedure_name: &str,
    args: &[CypherValue],
    view: &mut dyn GraphView,
    _yield_columns: Option<&[&str]>,
) -> Option<Result<ProcedureOutput, MlError>> {
    if !procedure_name.starts_with("traverse.ml.") {
        return None;
    }
    Some(dispatch(procedure_name, args, view))
}

fn dispatch(
    procedure_name: &str,
    args: &[CypherValue],
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    match procedure_name {
        "traverse.ml.pipeline.list" => return list_pipelines(),
        "traverse.ml.pipeline.drop" => {
            let m = bind_positional(args, &["pipelineName"]);
            return drop_pipeline(&m);
        }
        _ => {}
    }
    // train / predict / embeddings / explainers / model catalog: the procedures always exist;
    // without `ml` they report `MlFeatureDisabled`
    if is_engine_procedure(procedure_name) {
        #[cfg(feature = "ml")]
        return run::dispatch_engine(procedure_name, args, view);
        #[cfg(not(feature = "ml"))]
        {
            let _ = view;
            return Err(MlError::MlFeatureDisabled);
        }
    }
    let not_found = || MlError::NotFound(procedure_name.to_string());
    let rest = procedure_name
        .strip_prefix("traverse.ml.pipeline.")
        .ok_or_else(not_found)?;
    let (segment, op) = rest.split_once('.').ok_or_else(not_found)?;
    let task = TaskType::from_segment(segment).ok_or_else(not_found)?;
    match op {
        "create" => create(task, &bind_positional(args, &["pipelineName"])),
        "addNodeProperty" => add_node_property(
            task,
            &bind_positional(
                args,
                &["pipelineName", "procedureName", "procedureConfiguration"],
            ),
        ),
        "selectFeatures" => select_features(
            task,
            &bind_positional(args, &["pipelineName", "nodeProperties"]),
        ),
        "configureSplit" => configure_split(
            task,
            &bind_positional(args, &["pipelineName", "configuration"]),
        ),
        "configureAutoTuning" => configure_auto_tuning(
            task,
            &bind_positional(args, &["pipelineName", "configuration"]),
        ),
        "addGcn" | "addGraphSage" | "addRGcn" | "addGat" => add_candidate(
            task,
            &bind_positional(args, &["pipelineName", "configuration"]),
            &op[3..],
        ),
        "removeCandidate" => remove_candidate(
            task,
            &bind_positional(args, &["pipelineName", "candidateIndex"]),
        ),
        _ => Err(not_found()),
    }
}

/// Procedures that need the training engine (the `ml` feature).
fn is_engine_procedure(name: &str) -> bool {
    const ENGINE_OPS: [&str; 5] = [
        "train",
        "trainStatus",
        "predict.stream",
        "predict.write",
        "targetCandidates",
    ];
    if let Some(rest) = name.strip_prefix("traverse.ml.pipeline.")
        && let Some((segment, op)) = rest.split_once('.')
    {
        return TaskType::from_segment(segment).is_some() && ENGINE_OPS.contains(&op);
    }
    matches!(
        name,
        "traverse.ml.trainStatus"
            | "traverse.ml.trainJobs"
            | "traverse.ml.trainCancel"
            | "traverse.ml.devices"
    ) || [
        "traverse.ml.model.",
        "traverse.ml.graphMae.",
        "traverse.ml.explain.",
    ]
    .iter()
    .any(|p| name.starts_with(p))
}

/// Maps positional args to a named map per procedure signature. Extra trailing args are ignored:
/// data-driven procedures get arity checks at parse time, but algorithm/ML procedures skip those,
/// so trailing nulls are tolerated.
pub(crate) fn bind_positional(
    args: &[CypherValue],
    names: &[&'static str],
) -> BTreeMap<String, CypherValue> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            (
                name.to_string(),
                args.get(i).cloned().unwrap_or(CypherValue::Null),
            )
        })
        .collect()
}

type Row = Vec<(std::borrow::Cow<'static, str>, CypherValue)>;

pub(crate) fn row(pairs: Vec<(&'static str, CypherValue)>) -> Row {
    pairs
        .into_iter()
        .map(|(k, v)| (std::borrow::Cow::Borrowed(k), v))
        .collect()
}

// --- traverse.ml.pipeline.list / drop ----------------------------

/// Every pipeline registered in this session: `(name, taskType, candidateCount, featureStageCount)`
/// per row.
fn list_pipelines() -> Result<ProcedureOutput, MlError> {
    let rows = registry::list_pipelines()
        .into_iter()
        .map(|entry| {
            row(vec![
                ("name", CypherValue::String(entry.name)),
                ("taskType", CypherValue::String(entry.task_type.to_string())),
                (
                    "candidateCount",
                    CypherValue::Integer(entry.candidate_count as i64),
                ),
                (
                    "featureStageCount",
                    CypherValue::Integer(entry.feature_stage_count as i64),
                ),
            ])
        })
        .collect();
    Ok(ProcedureOutput {
        rows,
        columns: vec!["name", "taskType", "candidateCount", "featureStageCount"],
    })
}

fn drop_pipeline(args: &BTreeMap<String, CypherValue>) -> Result<ProcedureOutput, MlError> {
    let name = required_string_arg(args, "pipelineName")?;
    if !registry::drop_pipeline(&name)? {
        return Err(MlError::PipelineNotFound(name));
    }
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            ("name", CypherValue::String(name)),
            ("dropped", CypherValue::Bool(true)),
        ])],
        columns: vec!["name", "dropped"],
    })
}

// --- configuration procedures ------------------------------------
// Each mutates a pipeline by name and returns GDS's 6-column yield row; the mutation runs inside
// `registry::update_pipeline` under the catalog mutex.

/// Column set shared by all GDS pipeline-configuration procedures.
pub(crate) const PIPELINE_YIELD_COLUMNS: &[&str] = &[
    "name",
    "nodePropertySteps",
    "featureProperties",
    "splitConfig",
    "autoTuningConfig",
    "parameterSpace",
];

fn yield_pipeline(name: &str, entry: &registry::PipelineEntry) -> ProcedureOutput {
    ProcedureOutput {
        rows: vec![entry.into_yield_row(name)],
        columns: PIPELINE_YIELD_COLUMNS.to_vec(),
    }
}

/// `create(pipelineName)`, GDS's YIELD columns.
fn create(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let name = required_string_arg(args, "pipelineName")?;
    let entry = registry::create_pipeline(&name, task)?;
    Ok(yield_pipeline(&name, &entry))
}

/// `addNodeProperty(pipe, procedureName, procedureConfiguration)`: a step that computes node
/// properties before training and prediction, on the training graph only (GDS's mutate mode;
/// nothing is written to the database). `procedureName` is any node-property algorithm
/// (`'pageRank'`, `'degree'`, `'fastRP'`, `'louvain'`, GDS spellings like `'gds.pageRank.mutate'`
/// too) or `'graphMae'`. The result is named by `mutateProperty` (defaults to the algorithm name)
/// and can then be picked with `selectFeatures`.
fn add_node_property(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let pipe = required_string_arg(args, "pipelineName")?;
    let procedure_name = required_string_arg(args, "procedureName")?;
    let mut config = optional_map_arg(args, "procedureConfiguration").unwrap_or_default();
    if !matches!(config.get("mutateProperty"), Some(CypherValue::String(_))) {
        config.insert(
            "mutateProperty".into(),
            CypherValue::String(normalize_step_name(&procedure_name)),
        );
    }
    let mut step: BTreeMap<String, CypherValue> = BTreeMap::new();
    step.insert("procedureName".into(), CypherValue::String(procedure_name));
    step.insert("config".into(), CypherValue::Map(config));
    let entry = registry::update_pipeline(&pipe, task, |p| {
        p.node_property_steps.push(CypherValue::Map(step));
        Ok(())
    })?;
    Ok(yield_pipeline(&pipe, &entry))
}

/// `gds.pageRank.mutate` / `gds.beta.graphSage.mutate` / `pageRank` -> `pageRank`.
pub(crate) fn normalize_step_name(name: &str) -> String {
    let mut parts: Vec<&str> = name.split('.').collect();
    if parts.first() == Some(&"gds") || parts.first() == Some(&"traverse") {
        parts.remove(0);
    }
    parts.retain(|p| !matches!(*p, "alpha" | "beta"));
    if parts.len() > 1
        && matches!(
            parts.last(),
            Some(&"mutate") | Some(&"stream") | Some(&"write") | Some(&"stats")
        )
    {
        parts.pop();
    }
    parts.join(".")
}

/// `selectFeatures(pipe, nodeProperties)`: a string or list of strings, replacing
/// `featureProperties` (as in GDS, repeated calls aren't additive). List and vector properties
/// expand to one column per element.
fn select_features(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let pipe = required_string_arg(args, "pipelineName")?;
    let props = string_list_arg(args, "nodeProperties")?;
    let entry = registry::update_pipeline(&pipe, task, |p| {
        p.feature_properties = props;
        Ok(())
    })?;
    Ok(yield_pipeline(&pipe, &entry))
}

/// `configureSplit(pipe, {testFraction, validationFolds[, negativeSamplingRatio]})`: merges keys
/// into `splitConfig`, validated.
fn configure_split(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let pipe = required_string_arg(args, "pipelineName")?;
    let config = required_map_arg(args, "configuration")?;
    for (k, v) in &config {
        match k.as_str() {
            "testFraction" => {
                let f = number(v).ok_or_else(|| bad_number(k))?;
                if !(0.0..1.0).contains(&f) {
                    return Err(MlError::InvalidConfig(format!(
                        "testFraction must be in [0, 1), got {f}"
                    )));
                }
            }
            "validationFolds" => match v {
                CypherValue::Integer(i) if *i >= 2 => {}
                _ => {
                    return Err(MlError::InvalidConfig(
                        "validationFolds must be an integer >= 2".into(),
                    ));
                }
            },
            "negativeSamplingRatio" if task == TaskType::LinkPrediction => {
                if number(v).is_none_or(|f| f <= 0.0) {
                    return Err(MlError::InvalidConfig(
                        "negativeSamplingRatio must be a positive number".into(),
                    ));
                }
            }
            // GDS link prediction also has trainFraction; the folds already cover it
            "trainFraction" if task == TaskType::LinkPrediction => {}
            other => {
                return Err(MlError::InvalidConfig(format!(
                    "unknown splitConfig key '{other}'"
                )));
            }
        }
    }
    let entry = registry::update_pipeline(&pipe, task, |p| {
        p.split_config.extend(config);
        Ok(())
    })?;
    Ok(yield_pipeline(&pipe, &entry))
}

/// `configureAutoTuning(pipe, {maxTrials[, strategy, eta, minEpochs]})`. `strategy` is
/// `'randomSearch'` (GDS's, the default) or `'successiveHalving'`: rungs of growing epoch budgets,
/// the best `1/eta` advancing (`eta` default 3, `minEpochs` default 10).
fn configure_auto_tuning(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let pipe = required_string_arg(args, "pipelineName")?;
    let config = required_map_arg(args, "configuration")?;
    for (k, v) in &config {
        let ok = match k.as_str() {
            "maxTrials" | "minEpochs" => matches!(v, CypherValue::Integer(i) if *i >= 1),
            "eta" => matches!(v, CypherValue::Integer(i) if *i >= 2),
            "strategy" => matches!(v, CypherValue::String(s)
                if s == "randomSearch" || s == "successiveHalving"),
            other => {
                return Err(MlError::InvalidConfig(format!(
                    "unknown autoTuningConfig key '{other}'"
                )));
            }
        };
        if !ok {
            return Err(MlError::InvalidConfig(format!(
                "bad autoTuningConfig value for '{k}': {}",
                v.type_name()
            )));
        }
    }
    let entry = registry::update_pipeline(&pipe, task, |p| {
        p.auto_tuning_config.extend(config);
        Ok(())
    })?;
    Ok(yield_pipeline(&pipe, &entry))
}

/// `addGcn` / `addGraphSage` / `addRGcn` / `addGat(pipe, config)`: a model candidate, the
/// counterpart of GDS's `addLogisticRegression` / `addMLP`. Keys: `hidden` (default 64),
/// `learningRate` (default from the train config, else 0.01), and `heads` for GAT (default 4).
/// Each is a number or `{range: [min, max]}` for auto-tuning (learning rates sample
/// log-uniformly).
fn add_candidate(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
    method_name: &str,
) -> Result<ProcedureOutput, MlError> {
    let pipe = required_string_arg(args, "pipelineName")?;
    let config = optional_map_arg(args, "configuration").unwrap_or_default();
    for (k, v) in &config {
        let known = matches!(k.as_str(), "hidden" | "learningRate")
            || (k == "heads" && method_name == "Gat");
        if !known {
            return Err(MlError::InvalidConfig(format!(
                "unknown {method_name} parameter '{k}'"
            )));
        }
        let ok = match v {
            CypherValue::Integer(i) => *i > 0,
            CypherValue::Float(f) => *f > 0.0 && k == "learningRate",
            CypherValue::Map(m) => matches!(m.get("range"), Some(CypherValue::List(r))
                if r.len() == 2 && r.iter().all(|x| number(x).is_some_and(|f| f > 0.0))),
            _ => false,
        };
        if !ok {
            return Err(MlError::InvalidConfig(format!(
                "{k} must be a positive number or {{range: [min, max]}}"
            )));
        }
    }
    let mut spec: BTreeMap<String, CypherValue> = BTreeMap::new();
    spec.insert(
        "methodName".into(),
        CypherValue::String(method_name.to_string()),
    );
    spec.insert("config".into(), CypherValue::Map(config));
    let entry = registry::update_pipeline(&pipe, task, |p| {
        p.parameter_space.push(CypherValue::Map(spec));
        Ok(())
    })?;
    Ok(yield_pipeline(&pipe, &entry))
}

/// `removeCandidate(pipe, candidateIndex)`, a Traverse extension (GDS needs drop + recreate):
/// removes the candidate at the zero-based index.
fn remove_candidate(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let pipe = required_string_arg(args, "pipelineName")?;
    let idx = match args.get("candidateIndex") {
        Some(CypherValue::Integer(i)) => *i,
        _ => return Err(MlError::MissingArgument("candidateIndex")),
    };
    let entry = registry::update_pipeline(&pipe, task, |p| {
        if idx < 0 || idx as usize >= p.parameter_space.len() {
            return Err(MlError::InvalidConfig(format!(
                "candidateIndex {idx} out of range (0..{})",
                p.parameter_space.len()
            )));
        }
        p.parameter_space.remove(idx as usize);
        Ok(())
    })?;
    Ok(yield_pipeline(&pipe, &entry))
}

// --- argument helpers --------------------------------------------

pub(crate) fn number(v: &CypherValue) -> Option<f64> {
    match v {
        CypherValue::Integer(i) => Some(*i as f64),
        CypherValue::Float(f) => Some(*f),
        _ => None,
    }
}

fn bad_number(key: &str) -> MlError {
    MlError::InvalidConfig(format!("{key} must be a number"))
}

pub(crate) fn required_string_arg(
    args: &BTreeMap<String, CypherValue>,
    name: &'static str,
) -> Result<String, MlError> {
    match args.get(name) {
        Some(CypherValue::String(s)) => Ok(s.clone()),
        Some(CypherValue::Null) | None => Err(MlError::MissingArgument(name)),
        Some(other) => Err(MlError::BadArgumentType {
            name,
            expected: "STRING",
            actual: other.type_name().to_string(),
        }),
    }
}

pub(crate) fn required_map_arg(
    args: &BTreeMap<String, CypherValue>,
    name: &'static str,
) -> Result<BTreeMap<String, CypherValue>, MlError> {
    match args.get(name) {
        Some(CypherValue::Map(m)) => Ok(m.clone()),
        Some(CypherValue::Null) | None => Err(MlError::MissingArgument(name)),
        Some(other) => Err(MlError::BadArgumentType {
            name,
            expected: "MAP",
            actual: other.type_name().to_string(),
        }),
    }
}

/// Optional MAP: `None` for absent and explicit null alike (GDS: omitted means default).
pub(crate) fn optional_map_arg(
    args: &BTreeMap<String, CypherValue>,
    name: &'static str,
) -> Option<BTreeMap<String, CypherValue>> {
    match args.get(name) {
        Some(CypherValue::Map(m)) => Some(m.clone()),
        _ => None,
    }
}

/// A STRING or LIST<STRING> argument as a list.
pub(crate) fn string_list_arg(
    args: &BTreeMap<String, CypherValue>,
    name: &'static str,
) -> Result<Vec<String>, MlError> {
    match args.get(name) {
        Some(CypherValue::String(s)) => Ok(vec![s.clone()]),
        Some(CypherValue::List(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| match v {
                CypherValue::String(s) => Ok(s.clone()),
                other => Err(MlError::BadArgumentType {
                    name,
                    expected: "STRING or LIST<STRING>",
                    actual: format!("LIST containing {} at index {i}", other.type_name()),
                }),
            })
            .collect(),
        Some(CypherValue::Null) | None => Err(MlError::MissingArgument(name)),
        Some(other) => Err(MlError::BadArgumentType {
            name,
            expected: "STRING or LIST<STRING>",
            actual: other.type_name().to_string(),
        }),
    }
}
