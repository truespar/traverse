//! `traverse.ml.explain.*`: why a node classifier predicted what it did, as relationships.
//!
//! * `pgExplainer`: PGExplainer (Luo et al. 2020), trained once over many nodes, then the most
//!   important relationships per node.
//! * `counterfactual`: CF-GNNExplainer-style; the fewest relationships whose removal changes the
//!   prediction.
//! * `subgraphAudit`: deterministic greedy elimination (SubgraphX audit mode); the small
//!   subgraph that keeps the prediction, reproducible for compliance records.
//!
//! All take a node classification model of a kind that supports edge masks (GCN, GraphSAGE,
//! GAT) and a `device` setting. They work on the explained nodes' computation subgraph: every node
//! and relationship within the model's two hops, which gives exactly the prediction the whole
//! graph gives, so an explanation costs the same on a graph of any size. Relationships are
//! reported once per node pair, undirected, as message passing sees them.

use std::collections::{BTreeMap, HashMap};

use burn::tensor::Tensor;
use burn::tensor::backend::Backend;
use traverse_core::tx::view::GraphView;
use traverse_ml::cancel::CancellationToken;
use traverse_ml::device::BackendTask;
use traverse_ml::encoder::{GraphInputs, Reads};
use traverse_ml::explain::{
    CounterfactualOutcome, PgExplainerOptions, SubgraphXAuditOutcome, pg_explain,
    receptive_field_edges, subgraphx_audit, train_counterfactual, train_pg_explainer,
};
use traverse_ml::pipeline::ModelChoice;
use traverse_ml::sampling::{Batch, computation_subgraph};
use traverse_ml::selection;

use crate::algorithms::procedure::ProcedureOutput;
use crate::ml::data::node_id_arg;
use crate::ml::device::{device_of, query_cancel, step_error};
use crate::ml::registry::TaskType;
use crate::ml::run::{Loaded, class_values, load};
use crate::ml::{MlError, number, required_map_arg, required_string_arg, row};
use crate::types::CypherValue;

/// Message passing depth of the 2-layer encoders: the receptive field explanations draw from.
const HOPS: usize = 2;

type Ad<B> = burn::backend::Autodiff<B>;

fn fail(e: std::io::Error) -> MlError {
    step_error(e, MlError::PredictionFailed)
}

/// The explained nodes' computation subgraph, with their model.
struct Subject {
    loaded: Loaded,
    batch: Batch,
    /// The batch nodes' standardized features, row-major.
    features: Vec<f32>,
    feat_dim: usize,
}

impl Subject {
    /// The classifier named in `config`, and the computation subgraph of `nodes` (dense indices).
    fn new(loaded: Loaded, nodes: &[u32]) -> Result<Self, MlError> {
        let choice = ModelChoice::of_model(&loaded.model).map_err(fail)?;
        if matches!(choice, ModelChoice::RGcn { .. }) {
            return Err(MlError::PredictionFailed(
                "explanations need a GCN, GraphSAGE or GAT model; R-GCN keeps one graph per \
                 relationship type"
                    .into(),
            ));
        }
        let feat_dim = loaded.spec.feature_dim();
        let all =
            selection::prepare_features(&loaded.model, &loaded.features, feat_dim).map_err(fail)?;
        let batch = computation_subgraph(&loaded.inputs, Reads::of(&choice), HOPS, nodes);
        let features = batch
            .nodes
            .iter()
            .flat_map(|&n| all[n as usize * feat_dim..(n as usize + 1) * feat_dim].to_vec())
            .collect();
        Ok(Self {
            loaded,
            batch,
            features,
            feat_dim,
        })
    }

    fn inputs(&self) -> &GraphInputs {
        &self.batch.inputs
    }

    /// The batch's features on `device`.
    fn x<B: Backend>(&self, device: &B::Device) -> Tensor<B, 2> {
        Tensor::<B, 1>::from_floats(self.features.as_slice(), device)
            .reshape([self.batch.nodes.len(), self.feat_dim])
    }

    /// Graph node id of subgraph node `local`.
    fn node_id(&self, local: u32) -> i64 {
        self.loaded.proj.node_ids[self.batch.nodes[local as usize] as usize].0 as i64
    }

    /// Subgraph entries as undirected node pairs (subgraph indices, self-loops dropped), keeping
    /// the best `score` per pair, best first.
    fn pairs(
        &self,
        entries: impl IntoIterator<Item = (usize, f32)>,
        higher_is_better: bool,
    ) -> Vec<((u32, u32), f32)> {
        let g = self.inputs();
        let src = entry_sources(g);
        let dst = &g.sym.raw().col_idx;
        let mut best: HashMap<(u32, u32), f32> = HashMap::new();
        for (e, score) in entries {
            let (u, v) = (src[e], dst[e]);
            if u == v {
                continue;
            }
            let key = (u.min(v), u.max(v));
            best.entry(key)
                .and_modify(|s| {
                    if (higher_is_better && score > *s) || (!higher_is_better && score < *s) {
                        *s = score;
                    }
                })
                .or_insert(score);
        }
        let mut out: Vec<((u32, u32), f32)> = best.into_iter().collect();
        out.sort_by(|a, b| {
            let ord = a.1.total_cmp(&b.1);
            (if higher_is_better { ord.reverse() } else { ord }).then(a.0.cmp(&b.0))
        });
        out
    }

    /// `{sourceNodeId, targetNodeId[, key: value]}` for a pair of subgraph nodes.
    fn relationship(&self, pair: (u32, u32), value: Option<(&'static str, f32)>) -> CypherValue {
        let mut m = BTreeMap::from([
            (
                "sourceNodeId".to_string(),
                CypherValue::Integer(self.node_id(pair.0)),
            ),
            (
                "targetNodeId".to_string(),
                CypherValue::Integer(self.node_id(pair.1)),
            ),
        ]);
        if let Some((key, v)) = value {
            m.insert(key.to_string(), CypherValue::Float(v as f64));
        }
        CypherValue::Map(m)
    }

    /// The original class value of class index `index`.
    fn class(&self, index: i32) -> CypherValue {
        let classes = class_values(&self.loaded.model);
        CypherValue::Integer(
            classes
                .get(index.max(0) as usize)
                .copied()
                .unwrap_or(index as i64),
        )
    }
}

/// Source node of every CSR entry of `g.sym`, in entry order.
fn entry_sources(g: &GraphInputs) -> Vec<u32> {
    let raw = g.sym.raw();
    let mut src = Vec::with_capacity(raw.col_idx.len());
    for i in 0..raw.num_nodes {
        for _ in raw.row_ptr[i]..raw.row_ptr[i + 1] {
            src.push(i as u32);
        }
    }
    src
}

/// The node classifier and explained nodes of a call.
fn subject(
    config: &BTreeMap<String, CypherValue>,
    view: &dyn GraphView,
    nodes: impl FnOnce(&Loaded) -> Result<Vec<u32>, MlError>,
) -> Result<Subject, MlError> {
    let loaded = load(
        TaskType::NodeClassification,
        config,
        view,
        device_of(config)?,
    )?;
    let nodes = nodes(&loaded)?;
    Subject::new(loaded, &nodes)
}

struct CounterfactualTask<'a> {
    s: &'a Subject,
    candidates: &'a [usize],
    epochs: usize,
    learning_rate: f64,
    sparsity: f32,
    cancel: &'a CancellationToken,
}

impl BackendTask for CounterfactualTask<'_> {
    type Output = std::io::Result<CounterfactualOutcome>;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output {
        let s = self.s;
        let encoder = selection::load_encoder::<Ad<B>>(&s.loaded.model, s.inputs(), &device)?;
        train_counterfactual::<Ad<B>>(
            &encoder,
            s.x(&device),
            s.inputs(),
            s.batch.seed_index[0] as usize,
            self.candidates,
            self.epochs,
            self.learning_rate,
            self.sparsity,
            &device,
            self.cancel,
        )
    }
}

/// `traverse.ml.explain.counterfactual(graphName, {modelName, nodeId[, epochs, learningRate,
/// sparsity, device]})`: one row `nodeId, originalClass, counterfactualClass, flipped,
/// originalProbability, counterfactualProbability, removedRelationships` (list of
/// `{sourceNodeId, targetNodeId, maskValue}`).
pub(crate) fn counterfactual(
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    let _graph_name = required_string_arg(args, "graphName")?;
    let config = required_map_arg(args, "configuration")?;
    let node_id = node_id_arg(config.get("nodeId"), "nodeId")?;
    let s = subject(&config, view, |l| Ok(vec![l.proj.dense(node_id)?]))?;
    let target = s.batch.seed_index[0] as usize;
    let candidates = receptive_field_edges(&s.inputs().sym, target, HOPS);
    let get = |k: &str, d: f64| config.get(k).and_then(number).unwrap_or(d);
    let cancel = query_cancel();
    let outcome = traverse_ml::device::run(
        s.loaded.device,
        CounterfactualTask {
            s: &s,
            candidates: &candidates,
            epochs: get("epochs", 100.0).max(1.0) as usize,
            learning_rate: get("learningRate", 0.1),
            sparsity: get("sparsity", 0.05) as f32,
            cancel: &cancel,
        },
    )
    .map_err(fail)?;
    let removed: Vec<CypherValue> = s
        .pairs(
            outcome.removed_edges.iter().map(|&e| (e, outcome.mask[e])),
            false,
        )
        .into_iter()
        .map(|(pair, mask)| s.relationship(pair, Some(("maskValue", mask))))
        .collect();
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            ("nodeId", CypherValue::Integer(node_id as i64)),
            ("originalClass", s.class(outcome.original_class)),
            ("counterfactualClass", s.class(outcome.counterfactual_class)),
            (
                "flipped",
                CypherValue::Bool(outcome.counterfactual_class != outcome.original_class),
            ),
            (
                "originalProbability",
                CypherValue::Float(outcome.original_prob as f64),
            ),
            (
                "counterfactualProbability",
                CypherValue::Float(outcome.flipped_prob as f64),
            ),
            ("removedRelationships", CypherValue::List(removed)),
        ])],
        columns: vec![
            "nodeId",
            "originalClass",
            "counterfactualClass",
            "flipped",
            "originalProbability",
            "counterfactualProbability",
            "removedRelationships",
        ],
    })
}

struct AuditTask<'a> {
    s: &'a Subject,
    candidates: &'a [usize],
    keep: &'a [usize],
    max_entries: usize,
    cancel: &'a CancellationToken,
}

impl BackendTask for AuditTask<'_> {
    type Output = std::io::Result<SubgraphXAuditOutcome>;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output {
        let s = self.s;
        let encoder = selection::load_encoder::<B>(&s.loaded.model, s.inputs(), &device)?;
        subgraphx_audit::<B>(
            &encoder,
            s.x(&device),
            s.inputs(),
            s.batch.seed_index[0] as usize,
            self.max_entries,
            self.candidates,
            self.keep,
            &device,
            self.cancel,
        )
    }
}

/// `traverse.ml.explain.subgraphAudit(graphName, {modelName, nodeId[, maxRelationships,
/// device]})`: one row `nodeId, predictedClass, baselineProbability, explanationProbability,
/// relationships` (the retained `{sourceNodeId, targetNodeId}` pairs, at most `maxRelationships`,
/// default 6). Deterministic: the same graph and model always give the same answer.
pub(crate) fn subgraph_audit(
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    let _graph_name = required_string_arg(args, "graphName")?;
    let config = required_map_arg(args, "configuration")?;
    let node_id = node_id_arg(config.get("nodeId"), "nodeId")?;
    let max_rel = config
        .get("maxRelationships")
        .and_then(number)
        .map_or(6, |f| f.max(1.0) as usize);
    let s = subject(&config, view, |l| Ok(vec![l.proj.dense(node_id)?]))?;
    let g = s.inputs();
    let src = entry_sources(g);
    let dst = &g.sym.raw().col_idx;
    // relationships only; self-loops stay on as the encoder expects
    let candidates: Vec<usize> =
        receptive_field_edges(&g.sym, s.batch.seed_index[0] as usize, HOPS)
            .into_iter()
            .filter(|&e| src[e] != dst[e])
            .collect();
    // the elimination costs O(E^2) forwards: refuse fields too large to finish
    if candidates.len() > 400 {
        return Err(MlError::PredictionFailed(format!(
            "node {node_id} has {} relationship entries within {HOPS} hops; the audit is for \
             small neighborhoods, use pgExplainer instead",
            candidates.len()
        )));
    }
    let keep: Vec<usize> = (0..src.len()).filter(|&e| src[e] == dst[e]).collect();
    let cancel = query_cancel();
    // each relationship is two directed entries
    let outcome = traverse_ml::device::run(
        s.loaded.device,
        AuditTask {
            s: &s,
            candidates: &candidates,
            keep: &keep,
            max_entries: max_rel * 2,
            cancel: &cancel,
        },
    )
    .map_err(fail)?;
    let kept: Vec<CypherValue> = s
        .pairs(outcome.retained_edges.iter().map(|&e| (e, 1.0)), true)
        .into_iter()
        .take(max_rel)
        .map(|(pair, _)| s.relationship(pair, None))
        .collect();
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            ("nodeId", CypherValue::Integer(node_id as i64)),
            ("predictedClass", s.class(outcome.target_class)),
            (
                "baselineProbability",
                CypherValue::Float(outcome.baseline_prob as f64),
            ),
            (
                "explanationProbability",
                CypherValue::Float(outcome.final_prob as f64),
            ),
            ("relationships", CypherValue::List(kept)),
        ])],
        columns: vec![
            "nodeId",
            "predictedClass",
            "baselineProbability",
            "explanationProbability",
            "relationships",
        ],
    })
}

struct PgExplainerTask<'a> {
    s: &'a Subject,
    opts: &'a PgExplainerOptions,
    cancel: &'a CancellationToken,
}

impl BackendTask for PgExplainerTask<'_> {
    /// Per explained node (subgraph index), its subgraph entries by importance.
    type Output = std::io::Result<Vec<(u32, Vec<(usize, f32)>)>>;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output {
        use burn::module::AutodiffModule;

        let s = self.s;
        let l = &s.loaded;
        let encoder = selection::load_encoder::<Ad<B>>(&l.model, &l.inputs, &device)?;
        // embeddings from the whole graph, exactly: the subgraph's outer nodes lack neighbors
        let all = selection::prepare_features(&l.model, &l.features, s.feat_dim)?;
        let x_all = Tensor::<B, 1>::from_floats(all.as_slice(), &device)
            .reshape([l.proj.num_nodes(), s.feat_dim]);
        let z_all = selection::infer(&encoder.valid(), x_all, &l.inputs);
        let idx: Vec<i32> = s.batch.nodes.iter().map(|&n| n as i32).collect();
        let z = z_all.select(0, Tensor::from_ints(idx.as_slice(), &device));
        let targets: Vec<usize> = s.batch.seed_index.iter().map(|&i| i as usize).collect();
        let (explainer, _) = train_pg_explainer::<Ad<B>>(
            &encoder,
            s.x(&device),
            s.inputs(),
            Tensor::from_inner(z.clone()),
            &targets,
            self.opts,
            &device,
            self.cancel,
        )?;
        Ok(s.batch
            .seed_index
            .iter()
            .map(|&t| {
                let ranked =
                    pg_explain::<B>(&explainer, z.clone(), s.inputs(), t as usize, HOPS, &device);
                (t, ranked)
            })
            .collect())
    }
}

/// `traverse.ml.explain.pgExplainer(graphName, {modelName[, nodeIds, topK, epochs, learningRate,
/// device]})`: trains a PGExplainer over `nodeIds` (default: up to 1000 of the model's target
/// nodes) and streams `nodeId, sourceNodeId, targetNodeId, importance`, the `topK` (10) most
/// important relationships per node.
pub(crate) fn pg_explainer(
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    let _graph_name = required_string_arg(args, "graphName")?;
    let config = required_map_arg(args, "configuration")?;
    let s = subject(&config, view, |l| match config.get("nodeIds") {
        Some(CypherValue::List(items)) => items
            .iter()
            .map(|v| node_id_arg(Some(v), "nodeIds").and_then(|id| l.proj.dense(id)))
            .collect(),
        Some(CypherValue::Null) | None => Ok(l
            .proj
            .with_labels(view, &l.spec.target_node_labels)
            .into_iter()
            .take(1000)
            .collect()),
        Some(other) => Err(MlError::BadArgumentType {
            name: "nodeIds",
            expected: "LIST<INTEGER>",
            actual: other.type_name().to_string(),
        }),
    })?;
    let get = |k: &str| config.get(k).and_then(number);
    let opts = PgExplainerOptions {
        epochs: get("epochs").map_or(30, |f| f.max(1.0) as usize),
        learning_rate: get("learningRate").unwrap_or(3e-3),
        ..Default::default()
    };
    let top_k = get("topK").map_or(10, |f| f.max(1.0) as usize);
    let cancel = query_cancel();
    let ranked = traverse_ml::device::run(
        s.loaded.device,
        PgExplainerTask {
            s: &s,
            opts: &opts,
            cancel: &cancel,
        },
    )
    .map_err(fail)?;
    let mut rows = Vec::new();
    for (target, entries) in ranked {
        for ((u, v), importance) in s.pairs(entries, true).into_iter().take(top_k) {
            rows.push(row(vec![
                ("nodeId", CypherValue::Integer(s.node_id(target))),
                ("sourceNodeId", CypherValue::Integer(s.node_id(u))),
                ("targetNodeId", CypherValue::Integer(s.node_id(v))),
                ("importance", CypherValue::Float(importance as f64)),
            ]));
        }
    }
    Ok(ProcedureOutput {
        rows,
        columns: vec!["nodeId", "sourceNodeId", "targetNodeId", "importance"],
    })
}
