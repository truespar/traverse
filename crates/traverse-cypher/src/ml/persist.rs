//! The pipeline catalog on disk: one checksummed [`traverse_codec`] file, [`FILE_NAME`], in the
//! data directory next to the `.tvdb` files and `.tvmodel`s. The registry rewrites it on every
//! change, before the change takes effect in memory, so a restart loads exactly what clients were
//! told is configured.
//!
//! Settings are plain data: [`check`] refuses anything else (nodes, dates, ...) when a pipeline is
//! configured, so whatever the catalog holds can always be written. The browser build has no
//! data directory: it keeps the check and compiles out the rest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ml::MlError;
use crate::ml::registry::{PipelineEntry, TaskType};
use crate::types::CypherValue;

/// The catalog's file name inside the data directory.
#[cfg(not(target_arch = "wasm32"))]
pub const FILE_NAME: &str = "ml-pipelines.tvpipelines";

/// A setting: the plain-data part of [`CypherValue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// On-disk task tag, decoupled from [`TaskType`] so the enum can change without moving the format.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Task {
    NodeClassification,
    NodeRegression,
    LinkPrediction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Pipeline {
    name: String,
    task: Task,
    node_property_steps: Vec<Value>,
    feature_properties: Vec<String>,
    split_config: BTreeMap<String, Value>,
    auto_tuning_config: BTreeMap<String, Value>,
    parameter_space: Vec<Value>,
}

/// The file's payload. A later layout is a new variant, so older files keep loading.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Serialize, Deserialize)]
enum Catalog {
    V1(Vec<Pipeline>),
}

fn value(v: &CypherValue, at: &str) -> Result<Value, MlError> {
    Ok(match v {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Bool(*b),
        CypherValue::Integer(i) => Value::Int(*i),
        CypherValue::Float(f) => Value::Float(*f),
        CypherValue::String(s) => Value::String(s.clone()),
        CypherValue::List(items) => Value::List(
            items
                .iter()
                .enumerate()
                .map(|(i, x)| value(x, &format!("{at}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        CypherValue::Map(m) => Value::Map(
            m.iter()
                .map(|(k, x)| Ok((k.clone(), value(x, &format!("{at}.{k}"))?)))
                .collect::<Result<_, MlError>>()?,
        ),
        other => {
            return Err(MlError::InvalidConfig(format!(
                "pipeline settings must be plain values (null, boolean, number, string, list or \
                 map); {at} is a {}",
                other.type_name()
            )));
        }
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn cypher(v: Value) -> CypherValue {
    match v {
        Value::Null => CypherValue::Null,
        Value::Bool(b) => CypherValue::Bool(b),
        Value::Int(i) => CypherValue::Integer(i),
        Value::Float(f) => CypherValue::Float(f),
        Value::String(s) => CypherValue::String(s),
        Value::List(items) => CypherValue::List(items.into_iter().map(cypher).collect()),
        Value::Map(m) => CypherValue::Map(m.into_iter().map(|(k, x)| (k, cypher(x))).collect()),
    }
}

fn values(items: &[CypherValue], at: &str) -> Result<Vec<Value>, MlError> {
    items
        .iter()
        .enumerate()
        .map(|(i, x)| value(x, &format!("{at}[{i}]")))
        .collect()
}

fn settings<'a>(
    map: impl IntoIterator<Item = (&'a String, &'a CypherValue)>,
    at: &str,
) -> Result<BTreeMap<String, Value>, MlError> {
    map.into_iter()
        .map(|(k, x)| Ok((k.clone(), value(x, &format!("{at}.{k}"))?)))
        .collect()
}

fn pipeline(e: &PipelineEntry) -> Result<Pipeline, MlError> {
    Ok(Pipeline {
        name: e.name.clone(),
        task: match e.task_type {
            TaskType::NodeClassification => Task::NodeClassification,
            TaskType::NodeRegression => Task::NodeRegression,
            TaskType::LinkPrediction => Task::LinkPrediction,
        },
        node_property_steps: values(&e.node_property_steps, "nodePropertySteps")?,
        feature_properties: e.feature_properties.clone(),
        split_config: settings(&e.split_config, "splitConfig")?,
        auto_tuning_config: settings(&e.auto_tuning_config, "autoTuningConfig")?,
        parameter_space: values(&e.parameter_space, "parameterSpace")?,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn entry(p: Pipeline) -> PipelineEntry {
    let map = |m: BTreeMap<String, Value>| m.into_iter().map(|(k, x)| (k, cypher(x))).collect();
    PipelineEntry {
        name: p.name,
        task_type: match p.task {
            Task::NodeClassification => TaskType::NodeClassification,
            Task::NodeRegression => TaskType::NodeRegression,
            Task::LinkPrediction => TaskType::LinkPrediction,
        },
        node_property_steps: p.node_property_steps.into_iter().map(cypher).collect(),
        feature_properties: p.feature_properties,
        split_config: map(p.split_config),
        auto_tuning_config: map(p.auto_tuning_config),
        parameter_space: p.parameter_space.into_iter().map(cypher).collect(),
    }
}

/// Ok if `entry` can be stored: every setting is plain data.
pub fn check(entry: &PipelineEntry) -> Result<(), MlError> {
    pipeline(entry).map(drop)
}

/// The catalog file's bytes.
#[cfg(not(target_arch = "wasm32"))]
pub fn encode<'a>(
    entries: impl IntoIterator<Item = &'a PipelineEntry>,
) -> Result<Vec<u8>, MlError> {
    let pipelines = entries
        .into_iter()
        .map(pipeline)
        .collect::<Result<_, _>>()?;
    traverse_codec::encode(traverse_codec::Kind::Pipelines, &Catalog::V1(pipelines))
        .map_err(|e| MlError::PersistFailed(e.to_string()))
}

/// Pipelines from catalog file bytes.
#[cfg(not(target_arch = "wasm32"))]
pub fn decode(bytes: &[u8]) -> Result<Vec<PipelineEntry>, String> {
    if !traverse_codec::is_framed(bytes) {
        return Err("not an ML pipeline catalog".into());
    }
    match traverse_codec::decode::<Catalog>(traverse_codec::Kind::Pipelines, bytes)
        .map_err(|e| e.to_string())?
    {
        Catalog::V1(pipelines) => Ok(pipelines.into_iter().map(entry).collect()),
    }
}

/// Writes the catalog to `dir`, replacing the file whole (see [`traverse_core::storage::write_durable`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn save<'a>(
    dir: &std::path::Path,
    entries: impl IntoIterator<Item = &'a PipelineEntry>,
) -> Result<(), MlError> {
    let path = dir.join(FILE_NAME);
    let bytes = encode(entries)?;
    traverse_core::storage::write_durable(&path, &bytes)
        .map_err(|e| MlError::PersistFailed(format!("{}: {e}", path.display())))
}

/// The catalog in `dir`: empty when there's no file yet. A file that doesn't load is moved aside
/// (to `...damaged-<unix time>`, so nothing overwrites it) and reported as the error; if it can't
/// be moved either, the error says so and the caller must not write to `dir`.
#[cfg(not(target_arch = "wasm32"))]
pub fn load(dir: &std::path::Path) -> Result<Vec<PipelineEntry>, LoadError> {
    let path = dir.join(FILE_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(LoadError {
                reason: format!("{}: {e}", path.display()),
                kept_at: None,
            });
        }
    };
    decode(&bytes).map_err(|reason| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let aside = dir.join(format!("{FILE_NAME}.damaged-{stamp}"));
        LoadError {
            reason: format!("{}: {reason}", path.display()),
            kept_at: std::fs::rename(&path, &aside).ok().map(|()| aside),
        }
    })
}

/// Why [`load`] failed.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct LoadError {
    pub reason: String,
    /// Where the unreadable file was moved; `None` if it's still in place (so don't overwrite it).
    pub kept_at: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PipelineEntry {
        let mut step = BTreeMap::new();
        step.insert(
            "procedureName".into(),
            CypherValue::String("pageRank".into()),
        );
        step.insert(
            "config".into(),
            CypherValue::Map(BTreeMap::from([
                ("mutateProperty".into(), CypherValue::String("pr".into())),
                ("dampingFactor".into(), CypherValue::Float(0.85)),
            ])),
        );
        let mut candidate = BTreeMap::new();
        candidate.insert("methodName".into(), CypherValue::String("Gat".into()));
        candidate.insert(
            "config".into(),
            CypherValue::Map(BTreeMap::from([(
                "hidden".into(),
                CypherValue::Map(BTreeMap::from([(
                    "range".into(),
                    CypherValue::List(vec![CypherValue::Integer(16), CypherValue::Integer(64)]),
                )])),
            )])),
        );
        PipelineEntry {
            name: "fraud ✓/v2".into(),
            task_type: TaskType::LinkPrediction,
            node_property_steps: vec![CypherValue::Map(step)],
            feature_properties: vec!["age".into(), "pr".into()],
            split_config: [
                ("testFraction".to_string(), CypherValue::Float(0.1)),
                ("validationFolds".to_string(), CypherValue::Integer(3)),
            ]
            .into_iter()
            .collect(),
            auto_tuning_config: [("maxTrials".to_string(), CypherValue::Integer(4))]
                .into_iter()
                .collect(),
            parameter_space: vec![CypherValue::Map(candidate), CypherValue::Null],
        }
    }

    fn same(a: &PipelineEntry, b: &PipelineEntry) -> bool {
        a.name == b.name
            && a.task_type == b.task_type
            && a.node_property_steps == b.node_property_steps
            && a.feature_properties == b.feature_properties
            && a.split_config == b.split_config
            && a.auto_tuning_config == b.auto_tuning_config
            && a.parameter_space == b.parameter_space
    }

    #[test]
    fn round_trips_every_setting() {
        let e = sample();
        let back = decode(&encode([&e]).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert!(same(&back[0], &e), "{back:?}");
    }

    #[test]
    fn refuses_values_that_are_not_plain_data() {
        let mut e = sample();
        e.parameter_space
            .push(CypherValue::List(vec![CypherValue::NodeRef(
                traverse_core::graph::types::NodeId(1),
            )]));
        let err = check(&e).unwrap_err().to_string();
        assert!(err.contains("parameterSpace[2][0]"), "{err}");
    }

    #[test]
    fn rejects_other_containers_and_damage() {
        let bytes = encode([&sample()]).unwrap();
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x10;
        assert!(decode(&bad).unwrap_err().contains("CRC"));
        assert!(decode(b"{\"json\": true}").is_err());
        let model = traverse_codec::encode(traverse_codec::Kind::Model, &1u32).unwrap();
        assert!(decode(&model).unwrap_err().contains("model"));
    }

    #[test]
    fn save_and_load_through_a_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
        let (a, mut b) = (sample(), sample());
        b.name = "second".into();
        b.task_type = TaskType::NodeRegression;
        save(dir.path(), [&a, &b]).unwrap();
        let back = load(dir.path()).unwrap();
        assert_eq!(back.len(), 2);
        assert!(same(&back[0], &a) && same(&back[1], &b));
    }

    #[test]
    fn a_damaged_file_is_moved_aside_not_lost() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"\xFFTVP\x01garbage").unwrap();
        let err = load(dir.path()).unwrap_err();
        let kept = err.kept_at.expect("moved aside");
        assert_eq!(std::fs::read(&kept).unwrap(), b"\xFFTVP\x01garbage");
        assert!(!dir.path().join(FILE_NAME).exists());
        // the next start begins empty instead of failing again
        assert!(load(dir.path()).unwrap().is_empty());
    }
}
