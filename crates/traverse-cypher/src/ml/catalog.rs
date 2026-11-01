//! Model catalog procedures (`traverse.ml.model.*`) and `targetCandidates`.

use std::collections::BTreeMap;

use base64::Engine as _;
use traverse_core::tx::view::GraphView;

use crate::algorithms::procedure::ProcedureOutput;
use crate::ml::registry::TaskType;
use crate::ml::{MlError, registry};
use crate::types::CypherValue;

// --- Catalog procedures: target candidates / model list / drop / export --

/// `traverse.ml.pipeline.nodeClassification.targetCandidates()` (integer properties) and
/// `...nodeRegression.targetCandidates()` (any numeric property)
///
/// One row per candidate property with how many nodes have it; feeds Studio's "what do you want
/// to predict?" dropdown with live `labelled / total`.
///
/// YIELD: `propertyName, totalNodes, labelledNodes, labelsWithProperty, distinctValues`
///
/// `distinctValues` (unique integers over labelled nodes) lets Studio keep
/// plausible class labels (usually <= 12 values) and hide continuous ones like
/// `age` or `txn_count` behind an Advanced toggle, so nobody trains a 50-class
/// model on 6 examples.
pub fn target_candidates(
    task: TaskType,
    view: &mut dyn GraphView,
) -> Result<ProcedureOutput, MlError> {
    use std::collections::BTreeMap as BMap;
    use std::collections::BTreeSet;

    let nodes = view.nodes_vec();
    let total = nodes.len() as i64;

    // per property:
    //   .0 = nodes with it
    //   .1 = labels with it
    //   .2 = distinct integer values
    let mut per_prop: BMap<String, (i64, BTreeSet<String>, BTreeSet<i64>)> = BMap::new();

    // label ids -> names, once
    let label_id_to_name: BMap<u32, String> = {
        let mut m = BMap::new();
        for nid in &nodes {
            if let Ok(node) = view.get_node(*nid) {
                for label_id in &node.labels {
                    let name = view.strings().resolve(label_id.0);
                    m.insert(label_id.0, name.to_string());
                }
            }
        }
        m
    };

    for nid in &nodes {
        let node = match view.get_node(*nid) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let node_label_names: Vec<String> = node
            .labels
            .iter()
            .filter_map(|lid| label_id_to_name.get(&lid.0).cloned())
            .collect();
        for entry in node.properties.iter() {
            let (key_id, value) = entry;
            use traverse_core::graph::property::PropertyValue;
            // classes are integers; regression takes any number (distinct values by bit pattern)
            let ivalue = match (value, task) {
                (PropertyValue::Int(v), _) => v,
                (PropertyValue::Float(f), TaskType::NodeRegression) => f.to_bits() as i64,
                _ => continue,
            };
            let pname = view.strings().resolve(key_id.0).to_string();
            let entry = per_prop
                .entry(pname)
                .or_insert_with(|| (0, BTreeSet::new(), BTreeSet::new()));
            entry.0 += 1;
            for l in &node_label_names {
                entry.1.insert(l.clone());
            }
            entry.2.insert(ivalue);
        }
    }

    let mut rows: Vec<Vec<(std::borrow::Cow<'static, str>, CypherValue)>> =
        Vec::with_capacity(per_prop.len());
    for (name, (labelled, labels, values)) in per_prop {
        let labels_list: Vec<CypherValue> = labels.into_iter().map(CypherValue::String).collect();
        let distinct = values.len() as i64;
        rows.push(vec![
            (
                std::borrow::Cow::Borrowed("propertyName"),
                CypherValue::String(name),
            ),
            (
                std::borrow::Cow::Borrowed("totalNodes"),
                CypherValue::Integer(total),
            ),
            (
                std::borrow::Cow::Borrowed("labelledNodes"),
                CypherValue::Integer(labelled),
            ),
            (
                std::borrow::Cow::Borrowed("labelsWithProperty"),
                CypherValue::List(labels_list),
            ),
            (
                std::borrow::Cow::Borrowed("distinctValues"),
                CypherValue::Integer(distinct),
            ),
        ]);
    }

    Ok(ProcedureOutput {
        rows,
        columns: vec![
            "propertyName",
            "totalNodes",
            "labelledNodes",
            "labelsWithProperty",
            "distinctValues",
        ],
    })
}

/// `traverse.ml.model.list()`: every trained model, newest first, with what Studio's model card
/// needs. YIELD `modelName, taskType, targetProperty, targetRelationshipType, architecture,
/// pipeline, accuracy, labelledNodes, trainedAtUnix, classLabels, metrics, testMetrics,
/// trainSamples, testSamples, device, batching`. `testMetrics` maps each held-out test metric to
/// its value, measured on `testSamples` examples; `batching` is `{mode[, batchSize, fanouts]}`.
pub fn model_list() -> Result<ProcedureOutput, MlError> {
    let models = registry::snapshot_models();
    let mut rows: Vec<Vec<(std::borrow::Cow<'static, str>, CypherValue)>> =
        Vec::with_capacity(models.len());
    for (name, model) in models {
        let target = model
            .metadata
            .extra
            .get("targetProperty")
            .cloned()
            .unwrap_or_default();
        let arch = model
            .metadata
            .extra
            .get("architecture")
            .cloned()
            .unwrap_or_else(|| model.kind.clone());
        let pipeline = model
            .metadata
            .extra
            .get("pipeline")
            .cloned()
            .unwrap_or_default();
        let accuracy: f64 = model
            .metadata
            .extra
            .get("accuracy")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let labelled_samples: i64 = model
            .metadata
            .extra
            .get("labelledNodes")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        // Studio stores `classLabels` as a JSON string via `setMetadata`
        // (`{"0":"legit","1":"fraud"}`); passed through verbatim, empty = Studio shows `class N`.
        // Models from before `classValues` keep their class values there: not names
        let class_labels = model
            .metadata
            .extra
            .get("classLabels")
            .filter(|s| s.trim_start().starts_with('{'))
            .cloned()
            .unwrap_or_default();
        let extra = |k: &str| model.metadata.extra.get(k).cloned().unwrap_or_default();
        let test_metrics: BTreeMap<String, CypherValue> = model
            .metadata
            .training_metrics
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("test_")
                    .map(|m| (m.to_string(), CypherValue::Float(*v as f64)))
            })
            .collect();
        let batching = model
            .metadata
            .extra
            .get("batching")
            .and_then(|s| serde_json::from_str::<traverse_ml::selection::Batching>(s).ok())
            .map_or(CypherValue::Null, |b| {
                crate::ml::data::json_to_cypher(&crate::ml::device::batching_json(b))
            });
        // metrics JSON (F1_WEIGHTED, F1_MACRO, per-class precision/recall/F1) passed through
        // verbatim for Studio to render, no Cypher map shape needed
        let metrics_json = model
            .metadata
            .extra
            .get("metrics")
            .cloned()
            .unwrap_or_default();
        rows.push(vec![
            (
                std::borrow::Cow::Borrowed("modelName"),
                CypherValue::String(name),
            ),
            (
                std::borrow::Cow::Borrowed("taskType"),
                CypherValue::String(crate::ml::run::model_task(&model).to_string()),
            ),
            (
                std::borrow::Cow::Borrowed("targetProperty"),
                CypherValue::String(target),
            ),
            (
                std::borrow::Cow::Borrowed("targetRelationshipType"),
                CypherValue::String(extra("targetRelationshipType")),
            ),
            (
                std::borrow::Cow::Borrowed("architecture"),
                CypherValue::String(arch),
            ),
            (
                std::borrow::Cow::Borrowed("pipeline"),
                CypherValue::String(pipeline),
            ),
            (
                std::borrow::Cow::Borrowed("accuracy"),
                CypherValue::Float(accuracy),
            ),
            (
                std::borrow::Cow::Borrowed("labelledNodes"),
                CypherValue::Integer(labelled_samples),
            ),
            (
                std::borrow::Cow::Borrowed("trainedAtUnix"),
                CypherValue::Integer(model.metadata.created_at_unix as i64),
            ),
            (
                std::borrow::Cow::Borrowed("classLabels"),
                CypherValue::String(class_labels),
            ),
            (
                std::borrow::Cow::Borrowed("metrics"),
                CypherValue::String(metrics_json),
            ),
            (
                std::borrow::Cow::Borrowed("testMetrics"),
                CypherValue::Map(test_metrics),
            ),
            (
                std::borrow::Cow::Borrowed("trainSamples"),
                CypherValue::Integer(extra("trainSamples").parse().unwrap_or(0)),
            ),
            (
                std::borrow::Cow::Borrowed("testSamples"),
                CypherValue::Integer(extra("testSamples").parse().unwrap_or(0)),
            ),
            (
                std::borrow::Cow::Borrowed("device"),
                CypherValue::String(extra("device")),
            ),
            (std::borrow::Cow::Borrowed("batching"), batching),
        ]);
    }
    Ok(ProcedureOutput {
        rows,
        columns: vec![
            "modelName",
            "taskType",
            "targetProperty",
            "targetRelationshipType",
            "architecture",
            "pipeline",
            "accuracy",
            "labelledNodes",
            "trainedAtUnix",
            "classLabels",
            "metrics",
            "testMetrics",
            "trainSamples",
            "testSamples",
            "device",
            "batching",
        ],
    })
}

/// `traverse.ml.model.drop(modelName)`: removes a model from the session store.
pub fn model_drop(args: &BTreeMap<String, CypherValue>) -> Result<ProcedureOutput, MlError> {
    let name = super::required_string_arg(args, "modelName")?;
    if !registry::drop_model(&name)? {
        return Err(MlError::ModelNotFound(name));
    }
    Ok(ProcedureOutput {
        rows: vec![vec![
            (
                std::borrow::Cow::Borrowed("modelName"),
                CypherValue::String(name),
            ),
            (
                std::borrow::Cow::Borrowed("dropped"),
                CypherValue::Bool(true),
            ),
        ]],
        columns: vec!["modelName", "dropped"],
    })
}

/// `traverse.ml.model.setMetadata(modelName, key, value)`: sets one entry in
/// the model's free-form `extra` metadata and re-persists it, so the annotation
/// survives restarts. Studio uses it for class labels
/// (`classLabels = '{"0":"legit","1":"fraud"}'`) and other annotations without a
/// procedure per kind.
pub fn model_set_metadata(
    args: &BTreeMap<String, CypherValue>,
) -> Result<ProcedureOutput, MlError> {
    let name = super::required_string_arg(args, "modelName")?;
    let key = super::required_string_arg(args, "key")?;
    let value = super::required_string_arg(args, "value")?;
    registry::update_model(&name, |model| {
        model.metadata.extra.insert(key.clone(), value.clone());
    })?;
    Ok(ProcedureOutput {
        rows: vec![vec![
            (
                std::borrow::Cow::Borrowed("modelName"),
                CypherValue::String(name),
            ),
            (std::borrow::Cow::Borrowed("key"), CypherValue::String(key)),
            (
                std::borrow::Cow::Borrowed("value"),
                CypherValue::String(value),
            ),
        ]],
        columns: vec!["modelName", "key", "value"],
    })
}

/// `traverse.ml.model.export(modelName)`: the model's `.tvmodel` bytes as base64, so Studio can
/// offer a download over the same `/api/query` protocol.
pub fn model_export(args: &BTreeMap<String, CypherValue>) -> Result<ProcedureOutput, MlError> {
    let name = super::required_string_arg(args, "modelName")?;
    let model =
        registry::lookup_model(&name).ok_or_else(|| MlError::ModelNotFound(name.clone()))?;
    // full Model -> `.tvmodel` bytes (the on-disk format) -> base64
    let bytes = model
        .to_bytes()
        .map_err(|e| MlError::TrainingFailed(format!("encode model: {e}")))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ProcedureOutput {
        rows: vec![vec![
            (
                std::borrow::Cow::Borrowed("modelName"),
                CypherValue::String(name),
            ),
            (
                std::borrow::Cow::Borrowed("format"),
                CypherValue::String("tvmodel".into()),
            ),
            (
                std::borrow::Cow::Borrowed("base64Bytes"),
                CypherValue::String(b64),
            ),
            (
                std::borrow::Cow::Borrowed("sizeBytes"),
                CypherValue::Integer(bytes.len() as i64),
            ),
        ]],
        columns: vec!["modelName", "format", "base64Bytes", "sizeBytes"],
    })
}
