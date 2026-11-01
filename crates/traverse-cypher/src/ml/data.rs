//! From the graph to ML inputs: the projection a pipeline trains on, its feature columns (node
//! properties and node-property-step outputs), class labels and regression targets, and the
//! [`FeatureSpec`] stored with each model so prediction rebuilds exactly the same inputs on a graph
//! that may have changed since.

use std::collections::{BTreeMap, HashMap, HashSet};

use burn::tensor::backend::Backend;
use serde::{Deserialize, Serialize};
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;
use traverse_core::tx::view::GraphView;
use traverse_ml::cancel::CancellationToken;
use traverse_ml::device::{BackendTask, Device};
use traverse_ml::encoder::GraphInputs;
use traverse_ml::selection::Batching;

use crate::ml::device::{batching_of, cancelled, step_error};
use crate::ml::{MlError, normalize_step_name, number};
use crate::types::CypherValue;

/// Everything a model needs to rebuild its inputs, stored as JSON in its metadata
/// (`featureSpec`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct FeatureSpec {
    /// Projection filters; empty = every label / type.
    pub node_labels: Vec<String>,
    pub relationship_types: Vec<String>,
    /// Relationship types in relation-index order (R-GCN keeps weights per index).
    pub relation_types: Vec<String>,
    /// Nodes that are examples and get predictions (node tasks); empty = every projected node.
    pub target_node_labels: Vec<String>,
    /// Feature columns in order, with their width (lists and vectors expand).
    pub features: Vec<(String, usize)>,
    /// Node-property steps, run before the features are read.
    pub steps: Vec<StepSpec>,
}

/// One `addNodeProperty` step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StepSpec {
    /// Normalized algorithm name (`pageRank`) or `graphMae`.
    pub procedure: String,
    /// Name its output goes by in `selectFeatures`.
    pub mutate_property: String,
    /// Algorithm configuration, `mutateProperty` removed.
    pub config: serde_json::Value,
}

impl FeatureSpec {
    pub fn feature_dim(&self) -> usize {
        self.features.iter().map(|f| f.1).sum()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Parses a pipeline's `nodePropertySteps`.
pub(crate) fn parse_steps(steps: &[CypherValue]) -> Result<Vec<StepSpec>, MlError> {
    steps
        .iter()
        .map(|step| {
            let CypherValue::Map(m) = step else {
                return Err(MlError::InvalidConfig(
                    "malformed node property step".into(),
                ));
            };
            let procedure = match m.get("procedureName") {
                Some(CypherValue::String(s)) => normalize_step_name(s),
                _ => return Err(MlError::InvalidConfig("step without procedureName".into())),
            };
            let mut config = match m.get("config") {
                Some(CypherValue::Map(c)) => c.clone(),
                _ => BTreeMap::new(),
            };
            let mutate_property = match config.remove("mutateProperty") {
                Some(CypherValue::String(s)) => s,
                _ => procedure.clone(),
            };
            Ok(StepSpec {
                procedure,
                mutate_property,
                config: cypher_to_json(&CypherValue::Map(config)),
            })
        })
        .collect()
}

// --- projection --------------------------------------------------

/// The nodes and relationships a pipeline sees.
pub(crate) struct Projection {
    /// Dense index -> node.
    pub node_ids: Vec<NodeId>,
    /// Node id -> dense index.
    pub index: HashMap<u64, u32>,
    /// Relationships as stored (source, target, relation index).
    pub edges: Vec<(u32, u32, u16)>,
    /// Relation index -> relationship type.
    pub relation_types: Vec<String>,
}

impl Projection {
    /// Nodes with any of `node_labels` (all when empty) and the relationships between them of
    /// `relationship_types` (all when empty). With `fixed_relations` (prediction), relation
    /// indices follow that list and other types are left out.
    pub fn build(
        view: &dyn GraphView,
        node_labels: &[String],
        relationship_types: &[String],
        fixed_relations: Option<&[String]>,
    ) -> Self {
        let mut node_ids: Vec<NodeId> = if node_labels.is_empty() {
            view.nodes_vec()
        } else {
            node_labels
                .iter()
                .flat_map(|l| view.nodes_by_label(l))
                .collect()
        };
        node_ids.sort_unstable_by_key(|n| n.0);
        node_ids.dedup();
        let index: HashMap<u64, u32> = node_ids
            .iter()
            .enumerate()
            .map(|(i, n)| (n.0, i as u32))
            .collect();

        let edge_ids = if relationship_types.is_empty() {
            view.all_edges_vec()
        } else {
            relationship_types
                .iter()
                .flat_map(|t| view.edges_by_type(t))
                .collect()
        };
        let mut raw: Vec<(u32, u32, String)> = Vec::with_capacity(edge_ids.len());
        for eid in edge_ids {
            let Ok(e) = view.get_edge(eid) else { continue };
            if let (Some(&s), Some(&d)) = (index.get(&e.source.0), index.get(&e.target.0)) {
                let name = view.strings().resolve(e.edge_type.0).to_string();
                raw.push((s, d, name));
            }
        }
        let relation_types: Vec<String> = match fixed_relations {
            Some(fixed) => fixed.to_vec(),
            None => {
                let mut names: Vec<String> = raw.iter().map(|r| r.2.clone()).collect();
                names.sort_unstable();
                names.dedup();
                names
            }
        };
        let rel_index: HashMap<&str, u16> = relation_types
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i as u16))
            .collect();
        let edges = raw
            .iter()
            .filter_map(|(s, d, name)| rel_index.get(name.as_str()).map(|&r| (*s, *d, r)))
            .collect();
        Self {
            node_ids,
            index,
            edges,
            relation_types,
        }
    }

    pub fn num_nodes(&self) -> usize {
        self.node_ids.len()
    }

    /// Message-passing inputs: every relationship as an undirected pair, self-loops dropped (the
    /// encoders add their own).
    pub fn inputs(&self) -> GraphInputs {
        let mut pairs = Vec::with_capacity(self.edges.len());
        let mut rel = Vec::with_capacity(self.edges.len());
        for &(s, d, r) in &self.edges {
            if s != d {
                pairs.push((s.min(d), s.max(d)));
                rel.push(r);
            }
        }
        GraphInputs::from_edges(
            self.num_nodes(),
            &pairs,
            &rel,
            self.relation_types.len().max(1),
        )
    }

    /// Dense indices of the projected nodes carrying any of `labels`; all of them when empty.
    pub fn with_labels(&self, view: &dyn GraphView, labels: &[String]) -> Vec<u32> {
        if labels.is_empty() {
            return (0..self.num_nodes() as u32).collect();
        }
        let mut out: Vec<u32> = labels
            .iter()
            .flat_map(|l| view.nodes_by_label(l))
            .filter_map(|n| self.index.get(&n.0).copied())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Dense index of a node id, as a user-facing error when it isn't projected.
    pub fn dense(&self, id: u64) -> Result<u32, MlError> {
        self.index.get(&id).copied().ok_or_else(|| {
            MlError::InvalidConfig(format!("node {id} isn't part of the model's graph"))
        })
    }
}

// --- features ----------------------------------------------------

/// Node property values a pipeline reads: per name, per projected node, `None` where absent.
/// Owned, so features can be built after the transaction ends (in a training job).
pub(crate) type Columns = HashMap<String, Vec<Option<Vec<f32>>>>;

/// Node-property step outputs: per `mutateProperty`, per projected node.
pub(crate) type Computed = HashMap<String, Vec<Vec<f32>>>;

/// A property as feature values: numbers and booleans are one column, lists and vectors one per
/// element; anything else (strings, maps) yields nothing.
fn feature_values(pv: &PropertyValue) -> Option<Vec<f32>> {
    match pv {
        PropertyValue::Vector(v) => Some(v.clone()),
        PropertyValue::List(items) => items
            .iter()
            .map(|x| {
                x.as_float()
                    .or_else(|| x.as_int().map(|i| i as f64))
                    .map(|f| f as f32)
            })
            .collect(),
        other => other
            .as_float()
            .or_else(|| other.as_int().map(|i| i as f64))
            .or_else(|| other.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
            .map(|f| vec![f as f32]),
    }
}

/// Reads `names` from the projected nodes' properties.
pub(crate) fn read_columns(view: &dyn GraphView, proj: &Projection, names: &[String]) -> Columns {
    names
        .iter()
        .map(|name| {
            let values = proj
                .node_ids
                .iter()
                .map(|&n| {
                    view.get_node_property(n, name)
                        .ok()
                        .flatten()
                        .and_then(|pv| feature_values(&pv))
                })
                .collect();
            (name.clone(), values)
        })
        .collect()
}

/// What a pipeline reads from node properties: its features and its GraphMAE steps' inputs, except
/// what a step computes.
pub(crate) fn property_names(
    features: &[String],
    steps: &[StepSpec],
) -> Result<Vec<String>, MlError> {
    let computed: HashSet<&str> = steps.iter().map(|s| s.mutate_property.as_str()).collect();
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        if !computed.contains(n) && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    };
    for f in features {
        add(f);
    }
    for step in steps.iter().filter(|s| s.procedure == "graphMae") {
        for n in graph_mae_inputs(&step_config(step))? {
            add(&n);
        }
    }
    Ok(names)
}

/// One feature per node: a step output when there is one, else the property.
fn column<'a>(name: &str, columns: &'a Columns, computed: &'a Computed) -> Vec<Option<&'a [f32]>> {
    if let Some(c) = computed.get(name) {
        return c.iter().map(|v| Some(v.as_slice())).collect();
    }
    columns
        .get(name)
        .map(|c| c.iter().map(|v| v.as_deref()).collect())
        .unwrap_or_default()
}

/// Feature columns with their widths (the longest value seen), for training. A feature missing
/// on every node is an error: it's almost always a typo.
pub(crate) fn discover_widths(
    names: &[String],
    columns: &Columns,
    computed: &Computed,
) -> Result<Vec<(String, usize)>, MlError> {
    names
        .iter()
        .map(|name| {
            let width = column(name, columns, computed)
                .into_iter()
                .flatten()
                .map(<[f32]>::len)
                .max()
                .unwrap_or(0);
            if width == 0 {
                return Err(MlError::InvalidConfig(format!(
                    "feature '{name}' has no numeric value on any node; check the name, or add a \
                     node property step that computes it"
                )));
            }
            Ok((name.clone(), width))
        })
        .collect()
}

/// Row-major `[n x sum width]` feature matrix. Missing values are 0; longer lists are cut, shorter
/// ones padded.
pub(crate) fn feature_matrix(
    n: usize,
    features: &[(String, usize)],
    columns: &Columns,
    computed: &Computed,
) -> Vec<f32> {
    let dim: usize = features.iter().map(|f| f.1).sum();
    let mut out = vec![0.0f32; n * dim];
    let mut offset = 0;
    for (name, width) in features {
        for (i, value) in column(name, columns, computed).into_iter().enumerate() {
            if let Some(v) = value {
                for (j, x) in v.iter().take(*width).enumerate() {
                    out[i * dim + offset + j] = *x;
                }
            }
        }
        offset += width;
    }
    out
}

// --- node property steps -----------------------------------------

fn step_config(step: &StepSpec) -> BTreeMap<String, CypherValue> {
    match json_to_cypher(&step.config) {
        CypherValue::Map(m) => m,
        _ => BTreeMap::new(),
    }
}

/// Runs the steps on the projection; returns each step's output per node, by `mutateProperty`.
/// Later steps can use earlier outputs (a GraphMAE step over a PageRank column, say). Needs no
/// transaction: algorithms run on the projection, GraphMAE on `device`.
pub(crate) fn run_steps(
    proj: &Projection,
    inputs: &GraphInputs,
    steps: &[StepSpec],
    columns: &Columns,
    device: Device,
    cancel: &CancellationToken,
) -> Result<Computed, MlError> {
    let mut computed = Computed::new();
    let mut materialized: Option<crate::algorithms::materialized::MaterializedGraph> = None;
    for step in steps {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let config = step_config(step);
        let values = if step.procedure == "graphMae" {
            let out = graph_mae_embeddings(inputs, &config, columns, &computed, device, cancel)?;
            out.embeddings.chunks(out.dim).map(|c| c.to_vec()).collect()
        } else {
            if materialized.is_none() {
                let ids: Vec<u64> = proj.node_ids.iter().map(|n| n.0).collect();
                let edges: Vec<(u64, u64, Option<f64>)> = proj
                    .edges
                    .iter()
                    .map(|&(s, d, _)| (ids[s as usize], ids[d as usize], None))
                    .collect();
                materialized = Some(
                    crate::algorithms::materialized::MaterializedGraph::from_rows(ids, &edges)
                        .map_err(MlError::InvalidConfig)?,
                );
            }
            let graph = materialized.as_ref().expect("built above");
            algorithm_column(&step.procedure, &config, graph, proj)?
        };
        computed.insert(step.mutate_property.clone(), values);
    }
    Ok(computed)
}

/// Streams a node-property algorithm over the projection and takes its value column (the first
/// numeric or list column besides `nodeId`).
fn algorithm_column(
    algorithm: &str,
    config: &BTreeMap<String, CypherValue>,
    graph: &crate::algorithms::materialized::MaterializedGraph,
    proj: &Projection,
) -> Result<Vec<Vec<f32>>, MlError> {
    let name = format!("traverse.{algorithm}.stream");
    let out = crate::algorithms::procedure::run_for_call_on(&name, config, graph, None)
        .ok_or_else(|| {
            MlError::InvalidConfig(format!(
                "node property step '{algorithm}' isn't a known algorithm"
            ))
        })?
        .map_err(|e| match e {
            crate::algorithms::AlgorithmError::Interrupted(reason) => MlError::Interrupted(reason),
            crate::algorithms::AlgorithmError::MemoryExhausted => MlError::MemoryExhausted,
            other => MlError::TrainingFailed(format!("node property step '{algorithm}': {other}")),
        })?;
    let mut values = vec![Vec::new(); proj.num_nodes()];
    for row in &out.rows {
        let id = row.iter().find_map(|(k, v)| match (k.as_ref(), v) {
            ("nodeId", CypherValue::Integer(i)) => Some(*i as u64),
            _ => None,
        });
        let value = row.iter().find_map(|(k, v)| {
            if k == "nodeId" {
                return None;
            }
            match v {
                CypherValue::List(items) => items
                    .iter()
                    .map(|x| number(x).map(|f| f as f32))
                    .collect::<Option<Vec<f32>>>(),
                other => number(other).map(|f| vec![f as f32]),
            }
        });
        if let (Some(id), Some(value)) = (id, value)
            && let Some(&i) = proj.index.get(&id)
        {
            values[i as usize] = value;
        }
    }
    Ok(values)
}

/// The node properties GraphMAE reconstructs: `featureProperties`, required.
pub(crate) fn graph_mae_inputs(
    config: &BTreeMap<String, CypherValue>,
) -> Result<Vec<String>, MlError> {
    match config.get("featureProperties") {
        Some(CypherValue::String(s)) => Ok(vec![s.clone()]),
        Some(CypherValue::List(items)) => Ok(items
            .iter()
            .filter_map(|v| match v {
                CypherValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect()),
        _ => Err(MlError::InvalidConfig(
            "GraphMAE needs featureProperties: the node properties to reconstruct".into(),
        )),
    }
}

/// GraphMAE's result.
pub(crate) struct GraphMaeOutput {
    /// Row-major `[N x dim]`.
    pub embeddings: Vec<f32>,
    pub dim: usize,
    /// Mean loss per epoch.
    pub losses: Vec<f32>,
}

struct GraphMaeTask<'a> {
    features: &'a [f32],
    dim: usize,
    inputs: &'a GraphInputs,
    opts: &'a traverse_ml::ssl::GraphMaeOptions,
    cancel: &'a CancellationToken,
}

impl BackendTask for GraphMaeTask<'_> {
    type Output = std::io::Result<(Vec<f32>, Vec<f32>, Batching)>;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output {
        traverse_ml::ssl::train_graph_mae::<burn::backend::Autodiff<B>>(
            self.features,
            self.dim,
            self.inputs,
            self.opts,
            &device,
            self.cancel,
        )
    }
}

/// GraphMAE embeddings for the projection: `config` takes `featureProperties` (required),
/// `embeddingDimension`, `epochs`, `learningRate`, `maskRatio`, `gamma`, `randomSeed` and the
/// batching settings (`batching`, `batchSize`, `fanouts`).
pub(crate) fn graph_mae_embeddings(
    inputs: &GraphInputs,
    config: &BTreeMap<String, CypherValue>,
    columns: &Columns,
    computed: &Computed,
    device: Device,
    cancel: &CancellationToken,
) -> Result<GraphMaeOutput, MlError> {
    let names = graph_mae_inputs(config)?;
    let widths = discover_widths(&names, columns, computed)?;
    let feats = feature_matrix(inputs.num_nodes(), &widths, columns, computed);
    let dim: usize = widths.iter().map(|w| w.1).sum();
    let int = |k: &str, d: usize| {
        config
            .get(k)
            .and_then(number)
            .map(|f| f.max(1.0) as usize)
            .unwrap_or(d)
    };
    let float = |k: &str, d: f64| config.get(k).and_then(number).unwrap_or(d);
    let opts = traverse_ml::ssl::GraphMaeOptions {
        embedding_dim: int("embeddingDimension", 64),
        epochs: int("epochs", 100),
        learning_rate: float("learningRate", 1e-3),
        mask_ratio: float("maskRatio", 0.5).clamp(0.05, 0.95),
        sce_gamma: float("gamma", 3.0),
        seed: int("randomSeed", 42) as u64,
        batching: batching_of(config)?,
        ..Default::default()
    };
    let task = GraphMaeTask {
        features: &feats,
        dim,
        inputs,
        opts: &opts,
        cancel,
    };
    let (embeddings, losses, _) = traverse_ml::device::run(device, task)
        .map_err(|e| step_error(e, |m| MlError::TrainingFailed(format!("GraphMAE: {m}"))))?;
    Ok(GraphMaeOutput {
        embeddings,
        dim: opts.embedding_dim,
        losses,
    })
}

// --- targets -----------------------------------------------------

/// Classes of `targets` (dense indices) from integer or boolean `property`: per node, the index
/// into the returned sorted class values, or -1 (not a target, or no value).
pub(crate) fn class_labels(
    view: &dyn GraphView,
    proj: &Projection,
    property: &str,
    targets: &[u32],
) -> (Vec<i32>, Vec<i64>) {
    let raw: Vec<(u32, i64)> = targets
        .iter()
        .filter_map(|&i| {
            let pv = view
                .get_node_property(proj.node_ids[i as usize], property)
                .ok()
                .flatten()?;
            pv.as_int()
                .or_else(|| pv.as_bool().map(i64::from))
                .map(|c| (i, c))
        })
        .collect();
    let mut classes: Vec<i64> = raw.iter().map(|r| r.1).collect();
    classes.sort_unstable();
    classes.dedup();
    let mut labels = vec![-1i32; proj.num_nodes()];
    for (i, c) in raw {
        labels[i as usize] = classes.binary_search(&c).expect("collected above") as i32;
    }
    (labels, classes)
}

/// Numeric `property` of `targets` as regression targets; `NaN` elsewhere.
pub(crate) fn regression_targets(
    view: &dyn GraphView,
    proj: &Projection,
    property: &str,
    targets: &[u32],
) -> Vec<f32> {
    let mut out = vec![f32::NAN; proj.num_nodes()];
    for &i in targets {
        if let Some(v) = view
            .get_node_property(proj.node_ids[i as usize], property)
            .ok()
            .flatten()
            .and_then(|pv| pv.as_float().or_else(|| pv.as_int().map(|x| x as f64)))
            && v.is_finite()
        {
            out[i as usize] = v as f32;
        }
    }
    out
}

/// Relationships of `target_type` as undirected pairs, optionally only between `sources` and
/// `targets` (either orientation), plus every other relationship as context.
pub(crate) fn link_split(
    proj: &Projection,
    target_type: &str,
    sources: Option<&[u32]>,
    targets: Option<&[u32]>,
) -> Result<(u16, Vec<(u32, u32)>, Vec<(u32, u32, u16)>), MlError> {
    let rel = proj
        .relation_types
        .iter()
        .position(|t| t == target_type)
        .ok_or_else(|| {
            MlError::InvalidConfig(format!(
                "no relationships of type '{target_type}' in the graph"
            ))
        })? as u16;
    let src: Option<HashSet<u32>> = sources.map(|s| s.iter().copied().collect());
    let dst: Option<HashSet<u32>> = targets.map(|s| s.iter().copied().collect());
    let allowed = |a: u32, b: u32| {
        let in_src = |x| src.as_ref().is_none_or(|s| s.contains(&x));
        let in_dst = |x| dst.as_ref().is_none_or(|s| s.contains(&x));
        (in_src(a) && in_dst(b)) || (in_src(b) && in_dst(a))
    };
    let mut positives: Vec<(u32, u32)> = Vec::new();
    let mut context = Vec::new();
    for &(s, d, r) in &proj.edges {
        if s == d {
            continue;
        }
        let pair = (s.min(d), s.max(d));
        if r == rel && allowed(s, d) {
            positives.push(pair);
        } else {
            context.push((pair.0, pair.1, r));
        }
    }
    positives.sort_unstable();
    positives.dedup();
    Ok((rel, positives, context))
}

// --- JSON <-> Cypher -----------------------------------------------

pub(crate) fn cypher_to_json(v: &CypherValue) -> serde_json::Value {
    use serde_json::Value;
    match v {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Bool(*b),
        CypherValue::Integer(i) => Value::from(*i),
        CypherValue::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        CypherValue::String(s) => Value::String(s.clone()),
        CypherValue::List(items) => Value::Array(items.iter().map(cypher_to_json).collect()),
        CypherValue::Map(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), cypher_to_json(v)))
                .collect(),
        ),
        other => Value::String(format!("{other:?}")),
    }
}

pub(crate) fn json_to_cypher(v: &serde_json::Value) -> CypherValue {
    use serde_json::Value;
    match v {
        Value::Null => CypherValue::Null,
        Value::Bool(b) => CypherValue::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(CypherValue::Integer)
            .or_else(|| n.as_f64().map(CypherValue::Float))
            .unwrap_or(CypherValue::Null),
        Value::String(s) => CypherValue::String(s.clone()),
        Value::Array(items) => CypherValue::List(items.iter().map(json_to_cypher).collect()),
        Value::Object(m) => CypherValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), json_to_cypher(v)))
                .collect(),
        ),
    }
}

/// A node argument: an id, or a node value.
pub(crate) fn node_id_arg(v: Option<&CypherValue>, name: &'static str) -> Result<u64, MlError> {
    match v {
        Some(CypherValue::Integer(i)) if *i >= 0 => Ok(*i as u64),
        Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => Ok(id.0),
        Some(other) => Err(MlError::BadArgumentType {
            name,
            expected: "INTEGER node id or NODE",
            actual: other.type_name().to_string(),
        }),
        None => Err(MlError::MissingArgument(name)),
    }
}
