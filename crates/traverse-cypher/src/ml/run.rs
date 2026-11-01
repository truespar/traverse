//! `train` / `trainStatus` / `predict.*` for the three pipeline tasks, and the dispatcher for every
//! procedure that needs the training engine.

use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

use burn::tensor::backend::Backend;
use serde_json::{Value, json};
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;
use traverse_core::tx::view::GraphView;
use traverse_ml::cancel::CancellationToken;
use traverse_ml::device::{BackendTask, Device};
use traverse_ml::encoder::GraphInputs;
use traverse_ml::model::Model;
use traverse_ml::pipeline::{ModelChoice, ModelKind, TaskType as MlTask};
use traverse_ml::selection::{
    self, CandidateSpec, LinkData, Metric, MetricStats, NodeData, ParamSpace, SearchStrategy,
    SelectionConfig, SelectionResult,
};

use crate::algorithms::procedure::ProcedureOutput;
use crate::ml::data::{self, FeatureSpec, Projection, StepSpec, json_to_cypher};
use crate::ml::device::{self, batching_json, batching_of, device_of, interrupted, query_cancel};
use crate::ml::jobs::{self, JobContext, JobInfo, JobState, JobView};
use crate::ml::registry::{self, TaskType};
use crate::ml::{
    MlError, bind_positional, catalog, embed, explain, number, required_map_arg,
    required_string_arg, row,
};
use crate::types::CypherValue;

/// Every procedure that needs the `ml` feature.
pub(crate) fn dispatch_engine(
    name: &str,
    args: &[CypherValue],
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    let graph_config = || bind_positional(args, &["graphName", "configuration"]);
    match name {
        "traverse.ml.trainStatus" => return train_status(&bind_positional(args, &["jobId"])),
        "traverse.ml.trainJobs" => return train_jobs(),
        "traverse.ml.trainCancel" => return train_cancel(&bind_positional(args, &["jobId"])),
        "traverse.ml.devices" => return device::devices(),
        "traverse.ml.model.list" => return catalog::model_list(),
        "traverse.ml.model.drop" => {
            return catalog::model_drop(&bind_positional(args, &["modelName"]));
        }
        "traverse.ml.model.export" => {
            return catalog::model_export(&bind_positional(args, &["modelName"]));
        }
        "traverse.ml.model.setMetadata" => {
            return catalog::model_set_metadata(&bind_positional(
                args,
                &["modelName", "key", "value"],
            ));
        }
        "traverse.ml.graphMae.stream" => return embed::graph_mae(&graph_config(), view, false),
        "traverse.ml.graphMae.write" => return embed::graph_mae(&graph_config(), view, true),
        "traverse.ml.explain.counterfactual" => {
            return explain::counterfactual(&graph_config(), view);
        }
        "traverse.ml.explain.subgraphAudit" => {
            return explain::subgraph_audit(&graph_config(), view);
        }
        "traverse.ml.explain.pgExplainer" => return explain::pg_explainer(&graph_config(), view),
        _ => {}
    }
    let not_found = || MlError::NotFound(name.to_string());
    let rest = name
        .strip_prefix("traverse.ml.pipeline.")
        .ok_or_else(not_found)?;
    let (segment, op) = rest.split_once('.').ok_or_else(not_found)?;
    let task = TaskType::from_segment(segment).ok_or_else(not_found)?;
    match op {
        "train" => train(task, &graph_config(), view),
        "trainStatus" => train_status(&bind_positional(args, &["jobId"])),
        "predict.stream" => predict(task, &graph_config(), view, false),
        "predict.write" => predict(task, &graph_config(), view, true),
        "targetCandidates" if task != TaskType::LinkPrediction => {
            catalog::target_candidates(task, view)
        }
        _ => Err(not_found()),
    }
}

// --- config parsing ----------------------------------------------

fn ml_task(task: TaskType) -> MlTask {
    match task {
        TaskType::NodeClassification => MlTask::NodeClassification,
        TaskType::NodeRegression => MlTask::NodeRegression,
        TaskType::LinkPrediction => MlTask::LinkPrediction,
    }
}

fn opt_string(m: &BTreeMap<String, CypherValue>, key: &str) -> Option<String> {
    match m.get(key) {
        Some(CypherValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn req_string(m: &BTreeMap<String, CypherValue>, key: &'static str) -> Result<String, MlError> {
    opt_string(m, key).ok_or(MlError::MissingArgument(key))
}

fn string_list(m: &BTreeMap<String, CypherValue>, key: &str) -> Vec<String> {
    match m.get(key) {
        Some(CypherValue::String(s)) => vec![s.clone()],
        Some(CypherValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                CypherValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn get_f64(m: &BTreeMap<String, CypherValue>, key: &str) -> Option<f64> {
    m.get(key).and_then(number)
}

fn get_usize(m: &BTreeMap<String, CypherValue>, key: &str) -> Option<usize> {
    get_f64(m, key).map(|f| f.max(0.0) as usize)
}

fn hash_get_f64(m: &HashMap<String, CypherValue>, key: &str) -> Option<f64> {
    m.get(key).and_then(number)
}

/// `{range: [a, b]}` or a plain number.
fn param<T: Copy>(v: Option<&CypherValue>, fixed: impl Fn(f64) -> T, default: T) -> ParamSpace<T> {
    match v {
        Some(CypherValue::Map(m)) => match m.get("range") {
            Some(CypherValue::List(r)) if r.len() == 2 => match (number(&r[0]), number(&r[1])) {
                (Some(a), Some(b)) => ParamSpace::Range(fixed(a), fixed(b)),
                _ => ParamSpace::Fixed(default),
            },
            _ => ParamSpace::Fixed(default),
        },
        Some(other) => {
            number(other).map_or(ParamSpace::Fixed(default), |f| ParamSpace::Fixed(fixed(f)))
        }
        None => ParamSpace::Fixed(default),
    }
}

/// The pipeline's parameter space as candidate specs.
fn candidates(space: &[CypherValue], default_lr: f64) -> Result<Vec<CandidateSpec>, MlError> {
    let specs: Vec<CandidateSpec> = space
        .iter()
        .filter_map(|c| match c {
            CypherValue::Map(m) => Some(m),
            _ => None,
        })
        .map(|m| {
            let kind = match m.get("methodName") {
                Some(CypherValue::String(s)) => match s.as_str() {
                    "Gcn" => ModelKind::Gcn,
                    "GraphSage" => ModelKind::GraphSage,
                    "RGcn" => ModelKind::RGcn,
                    "Gat" => ModelKind::Gat,
                    other => {
                        return Err(MlError::InvalidConfig(format!(
                            "unknown model candidate '{other}'"
                        )));
                    }
                },
                _ => ModelKind::Gcn,
            };
            let empty = BTreeMap::new();
            let cfg = match m.get("config") {
                Some(CypherValue::Map(c)) => c,
                _ => &empty,
            };
            Ok(CandidateSpec {
                kind,
                hidden: param(cfg.get("hidden"), |f| f.max(1.0) as usize, 64),
                heads: param(cfg.get("heads"), |f| f.max(1.0) as usize, 4),
                learning_rate: param(cfg.get("learningRate"), |f| f, default_lr),
            })
        })
        .collect::<Result<_, _>>()?;
    if specs.is_empty() {
        return Err(MlError::InvalidConfig(
            "the pipeline has no model candidates; add one with addGcn, addGraphSage, addRGcn \
             or addGat"
                .into(),
        ));
    }
    Ok(specs)
}

fn selection_config(
    task: TaskType,
    pipe: &registry::PipelineEntry,
    config: &BTreeMap<String, CypherValue>,
) -> Result<SelectionConfig, MlError> {
    let mut cfg = SelectionConfig::defaults(ml_task(task));
    if let Some(f) = hash_get_f64(&pipe.split_config, "testFraction") {
        cfg.test_fraction = f;
    }
    if let Some(k) = hash_get_f64(&pipe.split_config, "validationFolds") {
        cfg.validation_folds = k as usize;
    }
    if let Some(r) = get_f64(config, "negativeSamplingRatio")
        .or_else(|| hash_get_f64(&pipe.split_config, "negativeSamplingRatio"))
    {
        cfg.negative_sampling_ratio = r;
    }
    if let Some(t) = hash_get_f64(&pipe.auto_tuning_config, "maxTrials") {
        cfg.max_trials = t.max(1.0) as usize;
    }
    if matches!(pipe.auto_tuning_config.get("strategy"), Some(CypherValue::String(s)) if s == "successiveHalving")
    {
        cfg.strategy = SearchStrategy::SuccessiveHalving {
            eta: hash_get_f64(&pipe.auto_tuning_config, "eta").map_or(3, |f| f as usize),
            min_epochs: hash_get_f64(&pipe.auto_tuning_config, "minEpochs")
                .map_or(10, |f| f as usize),
        };
    }
    let metrics = string_list(config, "metrics");
    if !metrics.is_empty() {
        cfg.metrics = metrics
            .iter()
            .map(|name| {
                Metric::parse(name)
                    .filter(|m| m.task() == ml_task(task))
                    .ok_or_else(|| {
                        let allowed: Vec<&str> = Metric::defaults(ml_task(task))
                            .iter()
                            .map(|m| m.name())
                            .collect();
                        MlError::InvalidConfig(format!(
                            "metric '{name}' doesn't apply to {task}; use one of {}",
                            allowed.join(", ")
                        ))
                    })
            })
            .collect::<Result<_, _>>()?;
    }
    if let Some(e) = get_usize(config, "epochs") {
        cfg.epochs = e.max(1);
    }
    if let Some(p) = get_f64(config, "patience") {
        cfg.patience = (p >= 1.0).then_some(p as usize);
    }
    if let Some(w) = get_f64(config, "weightDecay") {
        cfg.weight_decay = w.max(0.0);
    }
    if let Some(d) = get_usize(config, "embeddingDimension") {
        cfg.embedding_dim = d.max(1);
    }
    if let Some(s) = get_f64(config, "randomSeed") {
        cfg.seed = s as u64;
    }
    cfg.batching = batching_of(config)?;
    Ok(cfg)
}

// --- train -------------------------------------------------------

/// What a job learns to predict, read from the graph before `train` returns.
enum Targets {
    Classify {
        labels: Vec<i32>,
        classes: Vec<i64>,
    },
    Regress {
        values: Vec<f32>,
    },
    Link {
        relation: u16,
        positives: Vec<(u32, u32)>,
        context: Vec<(u32, u32, u16)>,
        sources: Option<Vec<u32>>,
        targets: Option<Vec<u32>>,
    },
}

/// `traverse.ml.pipeline.<task>.train(graphName, config)`.
///
/// Reads what the pipeline needs from the graph (the projection, property features, targets),
/// then queues a training job and returns its id: node-property steps, GraphMAE and model
/// selection run in the job, on the chosen device, after the transaction has ended.
///
/// `config` (GDS names where they exist):
///   * `pipeline`, `modelName` (required); `targetProperty` (node tasks) or
///     `targetRelationshipType` (link prediction), required.
///   * `nodeLabels`, `relationshipTypes`: the graph to train on (default everything).
///   * `targetNodeLabels`: node tasks' examples and prediction targets (default every node).
///   * `sourceNodeLabel`, `targetNodeLabel`: link prediction's pair endpoints.
///   * `metrics` (first one selects the model), `randomSeed`.
///   * Traverse extensions: `epochs` (100), `patience` (20, 0 disables early stopping),
///     `learningRate` (default for candidates without one, 0.01), `weightDecay` (5e-4),
///     `embeddingDimension` (link prediction, 64), `negativeSamplingRatio`; `device` (`'auto'`,
///     `'cpu'`, `'gpu'` or `'gpu:<n>'`, else the server default); `batching` (`'auto'`,
///     `'fullGraph'` or `'miniBatch'`), `batchSize` (512), `fanouts` ([25, 10], `-1` = all); and
///     `wait` (block until done and return the result instead of a job id).
pub(crate) fn train(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    let _graph_name = required_string_arg(args, "graphName")?;
    let config = required_map_arg(args, "configuration")?;
    let pipeline_name = req_string(&config, "pipeline")?;
    let model_name = req_string(&config, "modelName")?;
    if !registry::is_safe_model_name(&model_name) {
        return Err(MlError::InvalidConfig(format!(
            "model name '{model_name}' must be letters, digits, '-', '_' or '.'"
        )));
    }
    let pipe = registry::snapshot(&pipeline_name)
        .ok_or_else(|| MlError::PipelineNotFound(pipeline_name.clone()))?;
    if pipe.task_type != task {
        return Err(MlError::WrongTask {
            pipeline: pipeline_name,
            actual: pipe.task_type,
            wanted: task,
        });
    }
    let wait = matches!(config.get("wait"), Some(CypherValue::Bool(true)));
    let default_lr = get_f64(&config, "learningRate").unwrap_or(1e-2);
    let candidate_specs = candidates(&pipe.parameter_space, default_lr)?;
    let cfg = selection_config(task, &pipe, &config)?;
    let device = device_of(&config)?;

    // -- the projection, and what the steps and features read from it
    let node_labels = string_list(&config, "nodeLabels");
    let relationship_types = string_list(&config, "relationshipTypes");
    let proj = Projection::build(view, &node_labels, &relationship_types, None);
    if proj.num_nodes() == 0 {
        return Err(MlError::TrainingFailed(
            "the graph has no nodes to train on".into(),
        ));
    }
    if pipe.feature_properties.is_empty() {
        return Err(MlError::InvalidConfig(
            "the pipeline selects no features; call selectFeatures".into(),
        ));
    }
    let steps = data::parse_steps(&pipe.node_property_steps)?;
    let names = data::property_names(&pipe.feature_properties, &steps)?;
    let columns = data::read_columns(view, &proj, &names);
    // a property on no node is a typo: say so now rather than from the job; step outputs are
    // checked once the steps have run
    data::discover_widths(&names, &columns, &data::Computed::new())?;
    let target_node_labels = string_list(&config, "targetNodeLabels");
    let mut extra: BTreeMap<String, String> = BTreeMap::new();
    let mut info = serde_json::Map::new();
    info.insert("nodeCount".into(), json!(proj.num_nodes()));
    info.insert("edgeCount".into(), json!(proj.edges.len()));

    let targets = match task {
        TaskType::NodeClassification | TaskType::NodeRegression => {
            let target = req_string(&config, "targetProperty")?;
            let examples = proj.with_labels(view, &target_node_labels);
            extra.insert("targetProperty".into(), target.clone());
            info.insert("targetProperty".into(), json!(target));
            if task == TaskType::NodeClassification {
                let (labels, classes) = data::class_labels(view, &proj, &target, &examples);
                let labelled = labels.iter().filter(|&&l| l >= 0).count();
                if labelled == 0 {
                    return Err(MlError::TrainingFailed(format!(
                        "none of the {} target nodes has an integer value for '{target}'. \
                         Training needs examples: set the property on some nodes first (e.g. \
                         `MATCH (n) SET n.{target} = 0`), then try again",
                        examples.len()
                    )));
                }
                if classes.len() < 2 {
                    return Err(MlError::TrainingFailed(format!(
                        "'{target}' has a single class ({}); classification needs at least two",
                        classes[0]
                    )));
                }
                info.insert("labelledNodes".into(), json!(labelled));
                info.insert("numClasses".into(), json!(classes.len()));
                info.insert("classes".into(), json!(classes));
                extra.insert("labelledNodes".into(), labelled.to_string());
                // class values; `classLabels` is the display names Studio attaches
                extra.insert(
                    "classValues".into(),
                    classes
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                );
                Targets::Classify { labels, classes }
            } else {
                let values = data::regression_targets(view, &proj, &target, &examples);
                let labelled = values.iter().filter(|v| !v.is_nan()).count();
                if labelled < 2 {
                    return Err(MlError::TrainingFailed(format!(
                        "regression needs numeric '{target}' values on at least two target nodes, \
                         found {labelled}"
                    )));
                }
                info.insert("labelledNodes".into(), json!(labelled));
                extra.insert("labelledNodes".into(), labelled.to_string());
                Targets::Regress { values }
            }
        }
        TaskType::LinkPrediction => {
            let target_type = req_string(&config, "targetRelationshipType")?;
            let source_label = opt_string(&config, "sourceNodeLabel");
            let target_label = opt_string(&config, "targetNodeLabel");
            let sources = source_label
                .as_ref()
                .map(|l| proj.with_labels(view, std::slice::from_ref(l)));
            let targets = target_label
                .as_ref()
                .map(|l| proj.with_labels(view, std::slice::from_ref(l)));
            let (relation, positives, context) =
                data::link_split(&proj, &target_type, sources.as_deref(), targets.as_deref())?;
            extra.insert("targetRelationshipType".into(), target_type.clone());
            info.insert("targetRelationshipType".into(), json!(target_type));
            info.insert("targetRelationships".into(), json!(positives.len()));
            if let Some(l) = source_label {
                extra.insert("sourceNodeLabel".into(), l);
            }
            if let Some(l) = target_label {
                extra.insert("targetNodeLabel".into(), l);
            }
            Targets::Link {
                relation,
                positives,
                context,
                sources,
                targets,
            }
        }
    };

    extra.insert("pipeline".into(), pipeline_name.clone());
    extra.insert(
        "featureProperties".into(),
        pipe.feature_properties.join(","),
    );
    extra.insert("modelType".into(), task.to_string());
    extra.insert("device".into(), device.to_string());
    info.insert("modelName".into(), json!(model_name));
    info.insert("modelType".into(), json!(task.to_string()));
    info.insert("pipeline".into(), json!(pipeline_name));
    info.insert("featureProperties".into(), json!(pipe.feature_properties));
    info.insert("device".into(), json!(device.to_string()));

    let job = TrainJob {
        model_name: model_name.clone(),
        task,
        targets,
        proj,
        columns,
        steps,
        features: pipe.feature_properties.clone(),
        node_labels,
        relationship_types,
        target_node_labels,
        candidates: candidate_specs,
        cfg,
        extra,
        info,
        device,
    };
    let id = jobs::submit(
        JobInfo {
            model_name,
            pipeline: pipeline_name,
            task,
            device: device.to_string(),
        },
        move |ctx| job.run(ctx),
    );
    if wait {
        jobs::wait(&id);
    }
    let mut out = train_status(&BTreeMap::from([(
        "jobId".to_string(),
        CypherValue::String(id),
    )]))?;
    // train yields the status columns minus elapsedSeconds/progress
    out.columns
        .retain(|c| *c != "elapsedSeconds" && *c != "progress");
    for r in &mut out.rows {
        r.retain(|(k, _)| k != "elapsedSeconds" && k != "progress");
    }
    Ok(out)
}

/// A training run's inputs, owned so the job can run after the call returns.
struct TrainJob {
    model_name: String,
    task: TaskType,
    targets: Targets,
    proj: Projection,
    columns: data::Columns,
    steps: Vec<StepSpec>,
    features: Vec<String>,
    node_labels: Vec<String>,
    relationship_types: Vec<String>,
    target_node_labels: Vec<String>,
    candidates: Vec<CandidateSpec>,
    cfg: SelectionConfig,
    extra: BTreeMap<String, String>,
    info: serde_json::Map<String, Value>,
    device: Device,
}

/// Model selection for one task.
enum Selection<'a> {
    Classify {
        data: &'a NodeData,
        labels: &'a [i32],
        num_classes: usize,
    },
    Regress {
        data: &'a NodeData,
        targets: &'a [f32],
    },
    Link {
        data: &'a LinkData,
    },
}

/// [`Selection`] on a device: training uses `Autodiff` over the device's backend.
struct SelectionTask<'a> {
    what: Selection<'a>,
    candidates: &'a [CandidateSpec],
    cfg: &'a SelectionConfig,
    cancel: &'a CancellationToken,
    progress: &'a dyn Fn(usize, usize),
}

impl BackendTask for SelectionTask<'_> {
    type Output = std::io::Result<SelectionResult>;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output {
        type Ad<B> = burn::backend::Autodiff<B>;
        let (c, cfg, cancel, progress) = (self.candidates, self.cfg, self.cancel, self.progress);
        match self.what {
            Selection::Classify {
                data,
                labels,
                num_classes,
            } => selection::train_node_classification_selected::<Ad<B>>(
                data,
                labels,
                num_classes,
                c,
                cfg,
                &device,
                cancel,
                progress,
            ),
            Selection::Regress { data, targets } => {
                selection::train_node_regression_selected::<Ad<B>>(
                    data, targets, c, cfg, &device, cancel, progress,
                )
            }
            Selection::Link { data } => selection::train_link_prediction_selected::<Ad<B>>(
                data, c, cfg, &device, cancel, progress,
            ),
        }
    }
}

/// Runs `what` on `device`.
fn run_selection(
    device: Device,
    what: Selection<'_>,
    candidates: &[CandidateSpec],
    cfg: &SelectionConfig,
    cancel: &CancellationToken,
    progress: &dyn Fn(usize, usize),
) -> std::io::Result<SelectionResult> {
    traverse_ml::device::run(
        device,
        SelectionTask {
            what,
            candidates,
            cfg,
            cancel,
            progress,
        },
    )
}

impl TrainJob {
    fn run(self, ctx: &JobContext) -> JobState {
        self.train(ctx).unwrap_or_else(JobState::Failed)
    }

    fn train(self, ctx: &JobContext) -> Result<JobState, String> {
        let start = std::time::Instant::now();
        let cancel = &ctx.cancel;
        let fail = |e: MlError| e.to_string();

        // -- node-property steps, then the feature matrix
        let inputs = self.proj.inputs();
        let computed = data::run_steps(
            &self.proj,
            &inputs,
            &self.steps,
            &self.columns,
            self.device,
            cancel,
        )
        .map_err(fail)?;
        let widths =
            data::discover_widths(&self.features, &self.columns, &computed).map_err(fail)?;
        let n = self.proj.num_nodes();
        let matrix = data::feature_matrix(n, &widths, &self.columns, &computed);
        drop(computed);
        let spec = FeatureSpec {
            node_labels: self.node_labels,
            relationship_types: self.relationship_types,
            relation_types: self.proj.relation_types.clone(),
            target_node_labels: self.target_node_labels,
            features: widths,
            steps: self.steps,
        };
        let feat_dim = spec.feature_dim();

        // -- model selection on the device
        let progress = |done: usize, total: usize| ctx.progress(done, total);
        let (device, candidates, cfg) = (self.device, &self.candidates, &self.cfg);
        let (result, classes) = match self.targets {
            Targets::Classify { labels, classes } => {
                let data = NodeData {
                    inputs,
                    features: matrix,
                    feat_dim,
                };
                let what = Selection::Classify {
                    data: &data,
                    labels: &labels,
                    num_classes: classes.len(),
                };
                let r = run_selection(device, what, candidates, cfg, cancel, &progress);
                (r, Some(classes))
            }
            Targets::Regress { values } => {
                let data = NodeData {
                    inputs,
                    features: matrix,
                    feat_dim,
                };
                let what = Selection::Regress {
                    data: &data,
                    targets: &values,
                };
                (
                    run_selection(device, what, candidates, cfg, cancel, &progress),
                    None,
                )
            }
            Targets::Link {
                relation,
                positives,
                context,
                sources,
                targets,
            } => {
                // link prediction builds its own message-passing inputs per fold
                drop(inputs);
                let data = LinkData {
                    num_nodes: n,
                    features: matrix,
                    feat_dim,
                    context,
                    num_relations: self.proj.relation_types.len().max(1),
                    target_relation: relation,
                    positives,
                    source_nodes: sources,
                    target_nodes: targets,
                };
                let what = Selection::Link { data: &data };
                (
                    run_selection(device, what, candidates, cfg, cancel, &progress),
                    None,
                )
            }
        };
        let result = result.map_err(interrupted)?;
        let train_millis = start.elapsed().as_millis() as i64;

        let mut model = result.model.clone();
        for (k, v) in &self.extra {
            model.metadata.extra.insert(k.clone(), v.clone());
        }
        model
            .metadata
            .extra
            .insert("featureSpec".into(), spec.to_json());
        model
            .metadata
            .extra
            .insert("trainMillis".into(), train_millis.to_string());
        model
            .metadata
            .extra
            .insert("trainSamples".into(), result.train_size.to_string());
        model
            .metadata
            .extra
            .insert("testSamples".into(), result.test_size.to_string());
        let best = result.best();
        if let Some(classes) = &classes {
            let accuracy = result.test.get(&Metric::Accuracy).copied().unwrap_or(0.0);
            model
                .metadata
                .extra
                .insert("accuracy".into(), format!("{accuracy:.4}"));
            model.metadata.extra.insert(
                "metrics".into(),
                studio_metrics(&result, classes).to_string(),
            );
        }
        let warning = registry::register_model(&self.model_name, model)
            .err()
            .map(|e| format!("the model was trained and registered but not saved to disk: {e}"));

        let mut info = self.info.clone();
        info.insert("bestCandidate".into(), json!(best.candidate.kind()));
        info.insert("bestHidden".into(), json!(best.candidate.hidden()));
        info.insert("bestParameters".into(), parameters(best));
        info.insert("metrics".into(), metric_report(&result));
        info.insert("batching".into(), batching_json(result.batching));
        info.insert("trainSamples".into(), json!(result.train_size));
        info.insert("testSamples".into(), json!(result.test_size));
        info.insert(
            "valSamples".into(),
            json!(result.train_size / self.cfg.validation_folds.max(1)),
        );
        let first = self
            .cfg
            .metrics
            .first()
            .copied()
            .unwrap_or(Metric::Accuracy);
        let mut stats = serde_json::Map::new();
        stats.insert("bestParameters".into(), parameters(best));
        stats.insert(
            "modelCandidates".into(),
            Value::Array(
                result
                    .trials
                    .iter()
                    .map(|t| {
                        json!({
                            "parameters": parameters(t),
                            "metrics": trial_metrics(t),
                        })
                    })
                    .collect(),
            ),
        );
        if self.task == TaskType::NodeClassification {
            // the flat keys Studio's model card reads
            stats.insert(
                "valAcc".into(),
                json!(
                    best.validation
                        .get(&Metric::Accuracy)
                        .map_or(0.0, |s| s.avg)
                ),
            );
            stats.insert(
                "testAcc".into(),
                json!(result.test.get(&Metric::Accuracy).copied().unwrap_or(0.0)),
            );
        }
        stats.insert("selectionMetric".into(), json!(first.name()));
        Ok(JobState::Completed {
            train_millis,
            model_info_json: Value::Object(info).to_string(),
            selection_stats_json: Value::Object(stats).to_string(),
            warning,
        })
    }
}

fn method_name(c: &ModelChoice) -> &'static str {
    match c {
        ModelChoice::Gcn { .. } => "Gcn",
        ModelChoice::GraphSage { .. } => "GraphSage",
        ModelChoice::RGcn { .. } => "RGcn",
        ModelChoice::Gat { .. } => "Gat",
    }
}

fn parameters(t: &selection::TrialResult) -> Value {
    let mut p = json!({
        "methodName": method_name(&t.candidate),
        "hidden": t.candidate.hidden(),
        "learningRate": t.learning_rate,
        "epochs": t.epochs,
    });
    if let ModelChoice::Gat { heads, .. } = t.candidate {
        p["heads"] = json!(heads);
    }
    p
}

fn stats_json(s: &MetricStats) -> Value {
    json!({ "avg": s.avg, "min": s.min, "max": s.max })
}

fn trial_metrics(t: &selection::TrialResult) -> Value {
    let mut m = serde_json::Map::new();
    for (metric, val) in &t.validation {
        let mut entry = serde_json::Map::new();
        entry.insert("validation".into(), stats_json(val));
        if let Some(tr) = t.train.get(metric) {
            entry.insert("train".into(), stats_json(tr));
        }
        m.insert(metric.name().into(), Value::Object(entry));
    }
    Value::Object(m)
}

/// GDS's `modelInfo.metrics`: per metric, the winner's fold averages plus outer-train and test.
fn metric_report(r: &SelectionResult) -> Value {
    let best = r.best();
    let mut m = serde_json::Map::new();
    for (metric, test) in &r.test {
        let mut entry = serde_json::Map::new();
        if let Some(v) = best.validation.get(metric) {
            entry.insert("validation".into(), stats_json(v));
        }
        if let Some(v) = best.train.get(metric) {
            entry.insert("train".into(), stats_json(v));
        }
        if let Some(v) = r.outer_train.get(metric) {
            entry.insert("outerTrain".into(), json!(v));
        }
        entry.insert("test".into(), json!(test));
        m.insert(metric.name().into(), Value::Object(entry));
    }
    Value::Object(m)
}

/// The test-set report Studio's model card shows.
fn studio_metrics(r: &SelectionResult, classes: &[i64]) -> Value {
    let get = |m| r.test.get(&m).copied().unwrap_or(0.0);
    json!({
        "accuracy": get(Metric::Accuracy),
        "f1_weighted": get(Metric::F1Weighted),
        "f1_macro": get(Metric::F1Macro),
        "per_class": r.class_report.iter().map(|c| json!({
            "class": classes.get(c.class as usize).copied().unwrap_or(c.class as i64),
            "precision": c.precision,
            "recall": c.recall,
            "f1": c.f1,
            "support": c.support,
        })).collect::<Vec<_>>(),
        "test_count": r.test_size,
    })
}

// --- trainStatus / trainJobs / trainCancel -----------------------

const STATUS_COLUMNS: [&str; 8] = [
    "jobId",
    "status",
    "elapsedSeconds",
    "trainMillis",
    "modelInfo",
    "modelSelectionStats",
    "error",
    "progress",
];

fn job_id_arg(args: &BTreeMap<String, CypherValue>) -> Result<String, MlError> {
    match args.get("jobId") {
        Some(CypherValue::String(s)) => Ok(s.clone()),
        _ => Err(MlError::MissingArgument("jobId")),
    }
}

/// `{completedTrials, plannedTrials, queuePosition}`; the position is null unless queued.
fn progress_map(job: &JobView) -> CypherValue {
    CypherValue::Map(BTreeMap::from([
        (
            "completedTrials".to_string(),
            CypherValue::Integer(job.progress.0 as i64),
        ),
        (
            "plannedTrials".to_string(),
            CypherValue::Integer(job.progress.1 as i64),
        ),
        (
            "queuePosition".to_string(),
            job.queue_position
                .map_or(CypherValue::Null, |p| CypherValue::Integer(p as i64)),
        ),
    ]))
}

/// A job's error, or a completed job's warning; empty otherwise.
fn job_error(job: &JobView) -> String {
    match &job.state {
        JobState::Failed(msg) => msg.clone(),
        JobState::Completed { warning, .. } => warning.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// `traverse.ml.trainStatus(jobId)` (also `traverse.ml.pipeline.<task>.trainStatus`).
///
/// YIELD `jobId, status ('queued' | 'running' | 'completed' | 'failed' | 'cancelled'),
/// elapsedSeconds, trainMillis, modelInfo, modelSelectionStats, error, progress`
/// (`{completedTrials, plannedTrials, queuePosition}`). A completed job's `error` is empty unless
/// the model couldn't be saved to disk.
pub(crate) fn train_status(
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let job_id = job_id_arg(args)?;
    let job = jobs::get(&job_id).ok_or_else(|| MlError::JobNotFound(job_id.clone()))?;
    let empty = || CypherValue::Map(BTreeMap::new());
    let parse = |s: &str| {
        serde_json::from_str::<Value>(s)
            .map(|v| json_to_cypher(&v))
            .unwrap_or_else(|_| empty())
    };
    let (millis, info, stats) = match &job.state {
        JobState::Completed {
            train_millis,
            model_info_json,
            selection_stats_json,
            ..
        } => (
            *train_millis,
            parse(model_info_json),
            parse(selection_stats_json),
        ),
        _ => (0, empty(), empty()),
    };
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            ("jobId", CypherValue::String(job.id.clone())),
            ("status", CypherValue::String(job.state.name().into())),
            (
                "elapsedSeconds",
                CypherValue::Integer(job.elapsed_seconds as i64),
            ),
            ("trainMillis", CypherValue::Integer(millis)),
            ("modelInfo", info),
            ("modelSelectionStats", stats),
            ("error", CypherValue::String(job_error(&job))),
            ("progress", progress_map(&job)),
        ])],
        columns: STATUS_COLUMNS.to_vec(),
    })
}

/// `traverse.ml.trainJobs()`: every queued, running and recently finished training job, newest
/// first. YIELD `jobId, status, modelName, pipeline, taskType, device, submittedAtUnix,
/// elapsedSeconds, progress, error`.
pub(crate) fn train_jobs() -> Result<ProcedureOutput, MlError> {
    let rows = jobs::list()
        .iter()
        .map(|job| {
            row(vec![
                ("jobId", CypherValue::String(job.id.clone())),
                ("status", CypherValue::String(job.state.name().into())),
                (
                    "modelName",
                    CypherValue::String(job.info.model_name.clone()),
                ),
                ("pipeline", CypherValue::String(job.info.pipeline.clone())),
                ("taskType", CypherValue::String(job.info.task.to_string())),
                ("device", CypherValue::String(job.info.device.clone())),
                (
                    "submittedAtUnix",
                    CypherValue::Integer(job.submitted_at as i64),
                ),
                (
                    "elapsedSeconds",
                    CypherValue::Integer(job.elapsed_seconds as i64),
                ),
                ("progress", progress_map(job)),
                ("error", CypherValue::String(job_error(job))),
            ])
        })
        .collect();
    Ok(ProcedureOutput {
        rows,
        columns: vec![
            "jobId",
            "status",
            "modelName",
            "pipeline",
            "taskType",
            "device",
            "submittedAtUnix",
            "elapsedSeconds",
            "progress",
            "error",
        ],
    })
}

/// `traverse.ml.trainCancel(jobId)`: stops a job. A queued job is cancelled at once; a running one
/// stops at its next epoch or mini-batch (its status turns `cancelled` then); a finished one is
/// left as it is. YIELD `jobId, status, cancelRequested`.
pub(crate) fn train_cancel(
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let job_id = job_id_arg(args)?;
    let (job, requested) =
        jobs::cancel(&job_id).ok_or_else(|| MlError::JobNotFound(job_id.clone()))?;
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            ("jobId", CypherValue::String(job.id)),
            ("status", CypherValue::String(job.state.name().into())),
            ("cancelRequested", CypherValue::Bool(requested)),
        ])],
        columns: vec!["jobId", "status", "cancelRequested"],
    })
}

// --- predict -----------------------------------------------------

/// A stored model's task (models from before the `task` key are node classifiers).
pub(crate) fn model_task(model: &Model) -> TaskType {
    match model.metadata.extra.get("task").map(String::as_str) {
        Some("node_regression") => TaskType::NodeRegression,
        Some("link_prediction") => TaskType::LinkPrediction,
        _ => TaskType::NodeClassification,
    }
}

/// How a stored model rebuilds its inputs; models from before `featureSpec` read raw scalar
/// properties.
pub(crate) fn model_spec(model: &Model) -> Result<FeatureSpec, MlError> {
    if let Some(json) = model.metadata.extra.get("featureSpec") {
        return serde_json::from_str(json)
            .map_err(|e| MlError::PredictionFailed(format!("model feature spec: {e}")));
    }
    let features: Vec<(String, usize)> = model
        .metadata
        .extra
        .get("featureProperties")
        .map(|s| {
            s.split(',')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(|p| (p.to_string(), 1))
                .collect()
        })
        .unwrap_or_default();
    if features.is_empty() {
        return Err(MlError::PredictionFailed(
            "the model has no feature metadata; retrain it".into(),
        ));
    }
    Ok(FeatureSpec {
        features,
        ..Default::default()
    })
}

/// A model and the graph inputs it predicts on.
pub(crate) struct Loaded {
    pub model: Model,
    pub spec: FeatureSpec,
    pub proj: Projection,
    pub inputs: GraphInputs,
    pub features: Vec<f32>,
    /// Where it runs.
    pub device: Device,
}

/// Looks up `modelName`, checks its task and rebuilds its inputs from the current graph, running
/// its node-property steps on `device`.
pub(crate) fn load(
    task: TaskType,
    config: &BTreeMap<String, CypherValue>,
    view: &dyn GraphView,
    device: Device,
) -> Result<Loaded, MlError> {
    let name = req_string(config, "modelName")?;
    let model =
        registry::lookup_model(&name).ok_or_else(|| MlError::ModelNotFound(name.clone()))?;
    let actual = model_task(&model);
    if actual != task {
        return Err(MlError::PredictionFailed(format!(
            "model '{name}' is a {actual} model, not {task}"
        )));
    }
    let spec = model_spec(&model)?;
    let fixed = (!spec.relation_types.is_empty()).then_some(spec.relation_types.as_slice());
    let proj = Projection::build(view, &spec.node_labels, &spec.relationship_types, fixed);
    let feature_names: Vec<String> = spec.features.iter().map(|f| f.0.clone()).collect();
    let names = data::property_names(&feature_names, &spec.steps)?;
    let columns = data::read_columns(view, &proj, &names);
    let inputs = proj.inputs();
    let computed = data::run_steps(
        &proj,
        &inputs,
        &spec.steps,
        &columns,
        device,
        &query_cancel(),
    )?;
    let features = data::feature_matrix(proj.num_nodes(), &spec.features, &columns, &computed);
    Ok(Loaded {
        model,
        spec,
        proj,
        inputs,
        features,
        device,
    })
}

/// What [`infer`] computes for every node.
#[derive(Clone, Copy)]
enum Want {
    /// Predicted class and class probabilities.
    Classes,
    /// Predicted value, in target units.
    Values,
    /// Raw encoder output (link prediction's embeddings).
    Embeddings,
}

enum Inferred {
    Classes(Vec<(i32, Vec<f32>)>),
    Values(Vec<f32>),
    Embeddings(Vec<f32>),
}

struct InferTask<'a> {
    loaded: &'a Loaded,
    want: Want,
}

impl BackendTask for InferTask<'_> {
    type Output = std::io::Result<Inferred>;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output {
        let l = self.loaded;
        let dim = l.spec.feature_dim();
        Ok(match self.want {
            Want::Classes => Inferred::Classes(selection::predict_classes::<B>(
                &l.model,
                &l.inputs,
                &l.features,
                dim,
                &device,
            )?),
            Want::Values => Inferred::Values(selection::predict_values::<B>(
                &l.model,
                &l.inputs,
                &l.features,
                dim,
                &device,
            )?),
            Want::Embeddings => Inferred::Embeddings(selection::encode::<B>(
                &l.model,
                &l.inputs,
                &l.features,
                dim,
                &device,
            )?),
        })
    }
}

/// The loaded model applied to every node, on its device.
fn infer(loaded: &Loaded, want: Want) -> Result<Inferred, MlError> {
    // nothing to compute on a graph without the model's nodes
    if loaded.proj.num_nodes() == 0 {
        return Ok(match want {
            Want::Classes => Inferred::Classes(Vec::new()),
            Want::Values => Inferred::Values(Vec::new()),
            Want::Embeddings => Inferred::Embeddings(Vec::new()),
        });
    }
    traverse_ml::device::run(loaded.device, InferTask { loaded, want })
        .map_err(|e| MlError::PredictionFailed(e.to_string()))
}

/// Original class values of a classifier: `classValues`, or for models trained before it the same
/// list under `classLabels` (which now holds display names), else the class index.
pub(crate) fn class_values(model: &Model) -> Vec<i64> {
    let n = model.metadata.num_classes;
    let parse = |key: &str| {
        let s = model.metadata.extra.get(key)?;
        let v: Vec<i64> = s
            .split(',')
            .map(|c| c.trim().parse().ok())
            .collect::<Option<_>>()?;
        (v.len() == n).then_some(v)
    };
    parse("classValues")
        .or_else(|| parse("classLabels"))
        .unwrap_or_else(|| (0..n as i64).collect())
}

/// Sets one node property the way `SET n.p = v` does (index maintenance + redo log).
fn set_node_property(
    view: &mut dyn GraphView,
    node: NodeId,
    key: &str,
    value: PropertyValue,
) -> Result<(), MlError> {
    view.unindex_node_prop(node, key);
    let pid = view.intern_property_key(key);
    view.get_node_mut(node)
        .map_err(|e| MlError::PredictionFailed(format!("write failed at node {}: {e}", node.0)))?
        .properties
        .insert(pid, value);
    view.index_node_prop(node, key);
    Ok(())
}

/// `traverse.ml.pipeline.<task>.predict.stream` / `.predict.write`.
///
/// Node tasks predict the model's target nodes (`targetNodeLabels` overrides). Stream yields
/// `nodeId, predictedClass, predictedProbabilities` (classification; probabilities with
/// `includePredictedProbabilities: true`) or `nodeId, predictedValue` (regression). Write stores
/// the prediction in `writeProperty` (default: the target property), keeping existing values
/// unless `overwriteExisting: true`.
///
/// Link prediction scores node pairs that aren't linked yet, between `sourceNodeLabel` and
/// `targetNodeLabel` nodes (default: the training ones), and keeps the `topN` (10) above
/// `threshold` (0): stream yields `node1, node2, probability`; write creates
/// `writeRelationshipType` relationships with the probability in `writeProperty`
/// (`'probability'`).
pub(crate) fn predict(
    task: TaskType,
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
    write: bool,
) -> Result<ProcedureOutput, MlError> {
    let _graph_name = required_string_arg(args, "graphName")?;
    let config = required_map_arg(args, "configuration")?;
    let loaded = load(task, &config, view, device_of(&config)?)?;
    if task == TaskType::LinkPrediction {
        return predict_links(&loaded, &config, view, write);
    }
    let labels = {
        let mut l = string_list(&config, "targetNodeLabels");
        if l.is_empty() {
            l = loaded.spec.target_node_labels.clone();
        }
        l
    };
    let targets = loaded.proj.with_labels(view, &labels);
    // (value to write, stream columns) per target node
    let predictions: Vec<(PropertyValue, Vec<(&'static str, CypherValue)>)> =
        if task == TaskType::NodeClassification {
            let classes = class_values(&loaded.model);
            let with_probs = matches!(
                config.get("includePredictedProbabilities"),
                Some(CypherValue::Bool(true))
            );
            let Inferred::Classes(out) = infer(&loaded, Want::Classes)? else {
                unreachable!("asked for classes")
            };
            targets
                .iter()
                .map(|&i| {
                    let (c, probs) = &out[i as usize];
                    let class = classes.get(*c as usize).copied().unwrap_or(*c as i64);
                    let probs = if with_probs {
                        probs
                            .iter()
                            .map(|p| CypherValue::Float(*p as f64))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    (
                        PropertyValue::Int(class),
                        vec![
                            ("predictedClass", CypherValue::Integer(class)),
                            ("predictedProbabilities", CypherValue::List(probs)),
                        ],
                    )
                })
                .collect()
        } else {
            let Inferred::Values(out) = infer(&loaded, Want::Values)? else {
                unreachable!("asked for values")
            };
            targets
                .iter()
                .map(|&i| {
                    let v = out[i as usize] as f64;
                    (
                        PropertyValue::Float(v),
                        vec![("predictedValue", CypherValue::Float(v))],
                    )
                })
                .collect()
        };

    if !write {
        let mut columns = vec!["nodeId"];
        if let Some((_, cols)) = predictions.first() {
            columns.extend(cols.iter().map(|c| c.0));
        } else if task == TaskType::NodeClassification {
            columns.extend(["predictedClass", "predictedProbabilities"]);
        } else {
            columns.push("predictedValue");
        }
        let rows = targets
            .iter()
            .zip(predictions)
            .map(|(&i, (_, cols))| {
                let mut r = vec![(
                    "nodeId",
                    CypherValue::Integer(loaded.proj.node_ids[i as usize].0 as i64),
                )];
                r.extend(cols);
                row(r)
            })
            .collect();
        return Ok(ProcedureOutput { rows, columns });
    }

    let write_property = opt_string(&config, "writeProperty")
        .or_else(|| loaded.model.metadata.extra.get("targetProperty").cloned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            MlError::PredictionFailed("pass {writeProperty: '...'} to name the property".into())
        })?;
    let overwrite = matches!(
        config.get("overwriteExisting"),
        Some(CypherValue::Bool(true))
    );
    let (mut written, mut skipped) = (0i64, 0i64);
    for (&i, (value, _)) in targets.iter().zip(predictions) {
        let node = loaded.proj.node_ids[i as usize];
        // keep values already there (human labels) unless told otherwise
        if !overwrite
            && view
                .get_node_property(node, &write_property)
                .ok()
                .flatten()
                .is_some_and(|v| !matches!(v, PropertyValue::Null))
        {
            skipped += 1;
            continue;
        }
        set_node_property(view, node, &write_property, value)?;
        written += 1;
    }
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            ("nodesWritten", CypherValue::Integer(written)),
            ("nodesSkipped", CypherValue::Integer(skipped)),
            ("writeProperty", CypherValue::String(write_property)),
        ])],
        columns: vec!["nodesWritten", "nodesSkipped", "writeProperty"],
    })
}

/// A scored pair, ordered by probability (ties: lower ids first, for stable output).
#[derive(PartialEq)]
struct Scored(f64, u32, u32);

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .total_cmp(&other.0)
            .then_with(|| other.1.cmp(&self.1))
            .then_with(|| other.2.cmp(&self.2))
    }
}

/// Exhaustive top-N over `sources x targets`, in parallel per source.
fn top_links(
    z: &[f32],
    dim: usize,
    sources: &[u32],
    targets: &[u32],
    exclude: &HashSet<(u32, u32)>,
    top_n: usize,
    threshold: f64,
    symmetric: bool,
) -> Vec<Scored> {
    use rayon::prelude::*;
    let mut all: Vec<Scored> = sources
        .par_iter()
        .flat_map_iter(|&s| {
            let a = &z[s as usize * dim..(s as usize + 1) * dim];
            let mut heap: BinaryHeap<std::cmp::Reverse<Scored>> = BinaryHeap::new();
            for &t in targets {
                if t == s || (symmetric && t < s) || exclude.contains(&(s.min(t), s.max(t))) {
                    continue;
                }
                let b = &z[t as usize * dim..(t as usize + 1) * dim];
                let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                let p = 1.0 / (1.0 + (-(dot as f64)).exp());
                if p < threshold {
                    continue;
                }
                heap.push(std::cmp::Reverse(Scored(p, s, t)));
                if heap.len() > top_n {
                    heap.pop();
                }
            }
            heap.into_iter().map(|r| r.0)
        })
        .collect();
    all.sort_by(|a, b| b.cmp(a));
    all.truncate(top_n);
    all
}

fn predict_links(
    loaded: &Loaded,
    config: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
    write: bool,
) -> Result<ProcedureOutput, MlError> {
    let extra = &loaded.model.metadata.extra;
    let source_label =
        opt_string(config, "sourceNodeLabel").or_else(|| extra.get("sourceNodeLabel").cloned());
    let target_label =
        opt_string(config, "targetNodeLabel").or_else(|| extra.get("targetNodeLabel").cloned());
    let all: Vec<u32> = (0..loaded.proj.num_nodes() as u32).collect();
    let sources = source_label.as_ref().map_or_else(
        || all.clone(),
        |l| loaded.proj.with_labels(view, std::slice::from_ref(l)),
    );
    let targets = target_label.as_ref().map_or_else(
        || all.clone(),
        |l| loaded.proj.with_labels(view, std::slice::from_ref(l)),
    );
    let pairs = sources.len() as u128 * targets.len() as u128;
    if pairs > 500_000_000 {
        return Err(MlError::PredictionFailed(format!(
            "{pairs} candidate pairs is too many to score exhaustively; narrow them with \
             sourceNodeLabel / targetNodeLabel"
        )));
    }
    let target_type = extra
        .get("targetRelationshipType")
        .cloned()
        .unwrap_or_default();
    let target_rel = loaded
        .proj
        .relation_types
        .iter()
        .position(|t| *t == target_type)
        .map(|r| r as u16);
    let exclude: HashSet<(u32, u32)> = loaded
        .proj
        .edges
        .iter()
        .filter(|e| Some(e.2) == target_rel)
        .map(|&(s, d, _)| (s.min(d), s.max(d)))
        .collect();
    let dim = loaded.model.metadata.num_classes.max(1);
    let Inferred::Embeddings(z) = infer(loaded, Want::Embeddings)? else {
        unreachable!("asked for embeddings")
    };
    let top_n = get_usize(config, "topN").unwrap_or(10).max(1);
    let threshold = get_f64(config, "threshold").unwrap_or(0.0);
    let links = top_links(
        &z,
        dim,
        &sources,
        &targets,
        &exclude,
        top_n,
        threshold,
        source_label == target_label,
    );
    let id = |i: u32| loaded.proj.node_ids[i as usize];
    if !write {
        let rows = links
            .iter()
            .map(|Scored(p, s, t)| {
                row(vec![
                    ("node1", CypherValue::Integer(id(*s).0 as i64)),
                    ("node2", CypherValue::Integer(id(*t).0 as i64)),
                    ("probability", CypherValue::Float(*p)),
                ])
            })
            .collect();
        return Ok(ProcedureOutput {
            rows,
            columns: vec!["node1", "node2", "probability"],
        });
    }
    let rel_type = req_string(config, "writeRelationshipType")?;
    let property = opt_string(config, "writeProperty").unwrap_or_else(|| "probability".into());
    for Scored(p, s, t) in &links {
        let props = BTreeMap::from([(property.clone(), PropertyValue::Float(*p))]);
        view.create_edge(id(*s), id(*t), &rel_type, props)
            .map_err(|e| MlError::PredictionFailed(format!("create relationship: {e}")))?;
    }
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            (
                "relationshipsWritten",
                CypherValue::Integer(links.len() as i64),
            ),
            ("writeRelationshipType", CypherValue::String(rel_type)),
            ("writeProperty", CypherValue::String(property)),
        ])],
        columns: vec![
            "relationshipsWritten",
            "writeRelationshipType",
            "writeProperty",
        ],
    })
}
