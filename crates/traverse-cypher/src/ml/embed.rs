//! `traverse.ml.graphMae.stream` / `.write`: self-supervised node embeddings (GraphMAE, Hou et al.
//! 2022) from node features and the graph's structure. Useful as features for the pipelines (or
//! as an `addNodeProperty(pipe, 'graphMae', {...})` step) and for vector search.

use std::collections::BTreeMap;

use traverse_core::graph::property::PropertyValue;
use traverse_core::tx::view::GraphView;

use crate::algorithms::procedure::ProcedureOutput;
use crate::ml::data::{self, Projection};
use crate::ml::device::{device_of, query_cancel};
use crate::ml::{MlError, required_map_arg, required_string_arg, row};
use crate::types::CypherValue;

/// `graphMae.stream(graphName, config)` yields `nodeId, embedding`; `graphMae.write` stores each
/// embedding as a vector property `writeProperty` (default `'graphMae'`) and yields
/// `nodePropertiesWritten, writeProperty, trainMillis, finalLoss`.
///
/// `config`: `featureProperties` (required), `embeddingDimension` (64), `epochs` (100),
/// `learningRate` (0.001), `maskRatio` (0.5), `gamma` (3.0), `randomSeed` (42), `nodeLabels`,
/// `relationshipTypes`, `device`, and `batching` / `batchSize` / `fanouts` as for training.
/// Bound by the query's timeout and memory limit.
pub(crate) fn graph_mae(
    args: &BTreeMap<String, CypherValue>,
    view: &mut dyn GraphView,
    write: bool,
) -> Result<ProcedureOutput, MlError> {
    let _graph_name = required_string_arg(args, "graphName")?;
    let config = required_map_arg(args, "configuration")?;
    let list = |k: &str| match config.get(k) {
        Some(CypherValue::String(s)) => vec![s.clone()],
        Some(CypherValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                CypherValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let device = device_of(&config)?;
    let proj = Projection::build(view, &list("nodeLabels"), &list("relationshipTypes"), None);
    if proj.num_nodes() < 2 {
        return Err(MlError::TrainingFailed(
            "GraphMAE needs at least two nodes".into(),
        ));
    }
    let columns = data::read_columns(view, &proj, &data::graph_mae_inputs(&config)?);
    let start = std::time::Instant::now();
    let out = data::graph_mae_embeddings(
        &proj.inputs(),
        &config,
        &columns,
        &data::Computed::new(),
        device,
        &query_cancel(),
    )?;
    let train_millis = start.elapsed().as_millis() as i64;
    let (z, dim, losses) = (out.embeddings, out.dim, out.losses);
    let rows_of = |i: usize| z[i * dim..(i + 1) * dim].to_vec();

    if !write {
        let rows = (0..proj.num_nodes())
            .map(|i| {
                row(vec![
                    ("nodeId", CypherValue::Integer(proj.node_ids[i].0 as i64)),
                    (
                        "embedding",
                        CypherValue::List(
                            rows_of(i)
                                .into_iter()
                                .map(|x| CypherValue::Float(x as f64))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();
        return Ok(ProcedureOutput {
            rows,
            columns: vec!["nodeId", "embedding"],
        });
    }

    let property = match config.get("writeProperty") {
        Some(CypherValue::String(s)) => s.clone(),
        _ => "graphMae".to_string(),
    };
    for (i, &node) in proj.node_ids.iter().enumerate() {
        view.unindex_node_prop(node, &property);
        let pid = view.intern_property_key(&property);
        view.get_node_mut(node)
            .map_err(|e| MlError::TrainingFailed(format!("write failed at node {}: {e}", node.0)))?
            .properties
            .insert(pid, PropertyValue::Vector(rows_of(i)));
        view.index_node_prop(node, &property);
    }
    Ok(ProcedureOutput {
        rows: vec![row(vec![
            (
                "nodePropertiesWritten",
                CypherValue::Integer(proj.num_nodes() as i64),
            ),
            ("writeProperty", CypherValue::String(property)),
            ("trainMillis", CypherValue::Integer(train_millis)),
            (
                "finalLoss",
                CypherValue::Float(losses.last().copied().unwrap_or(f32::NAN) as f64),
            ),
        ])],
        columns: vec![
            "nodePropertiesWritten",
            "writeProperty",
            "trainMillis",
            "finalLoss",
        ],
    })
}
