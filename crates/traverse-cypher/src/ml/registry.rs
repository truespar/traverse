//! Pipeline catalog: process-global storage for ML pipeline definitions, which
//! build up over several procedure calls:
//!
//! ```text
//! CALL traverse.ml.pipeline.nodeClassification.create('p')      <- registers 'p'
//! CALL traverse.ml.pipeline.nodeClassification.addNodeProperty('p', 'degree', {})
//!                                                                 -- mutates 'p'
//! CALL traverse.ml.pipeline.nodeClassification.addGcn('p', {})    -- mutates 'p'
//! CALL traverse.ml.pipeline.nodeClassification.train('graph', {pipeline:'p', ...})
//!                                                                 -- reads 'p'
//! ```
//!
//! `CATALOG` is process-global: a pipeline created on one connection is visible on every other
//! connection and thread. Tests that `clear()` it must serialise themselves (see
//! `smoke::CATALOG_LOCK`).
//!
//! With a data directory ([`open_data_dir`], the server's `TRAVERSE_DATA`) pipelines persist in
//! [`persist::FILE_NAME`] and trained models as `<name>.tvmodel`; both reload at startup. Without
//! one (WASM, embedded, tests) everything stays in memory.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::ml::{MlError, persist};
use crate::types::CypherValue;

/// Prediction task of a pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    /// Predict a categorical node property.
    NodeClassification,
    /// Predict a numeric node property.
    NodeRegression,
    /// Predict whether an edge exists between two nodes.
    LinkPrediction,
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskType::NodeClassification => write!(f, "NodeClassification"),
            TaskType::NodeRegression => write!(f, "NodeRegression"),
            TaskType::LinkPrediction => write!(f, "LinkPrediction"),
        }
    }
}

impl TaskType {
    /// The procedure-name segment: `traverse.ml.pipeline.<segment>.*`.
    pub fn segment(self) -> &'static str {
        match self {
            TaskType::NodeClassification => "nodeClassification",
            TaskType::NodeRegression => "nodeRegression",
            TaskType::LinkPrediction => "linkPrediction",
        }
    }

    /// Inverse of [`segment`](Self::segment).
    pub fn from_segment(s: &str) -> Option<Self> {
        [
            TaskType::NodeClassification,
            TaskType::NodeRegression,
            TaskType::LinkPrediction,
        ]
        .into_iter()
        .find(|t| t.segment() == s)
    }
}

/// Pipeline entry as the Cypher surface sees it. Fields follow GDS's `addNodeProperty` /
/// `selectFeatures` / `configureSplit` / `addLogisticRegression` ... columns, so YIELD shapes stay
/// consistent.
#[derive(Debug, Clone)]
pub struct PipelineEntry {
    /// Pipeline name, unique.
    pub name: String,
    /// Prediction task.
    pub task_type: TaskType,
    /// Feature steps (degree / logDegree / property), each a GDS-style `{name, procedureName,
    /// config}` map.
    pub node_property_steps: Vec<CypherValue>,
    /// `selectFeatures` output: feature column names.
    pub feature_properties: Vec<String>,
    /// `{testFraction, validationFolds}` (GDS), plus `negativeSamplingRatio` for link prediction.
    pub split_config: HashMap<String, CypherValue>,
    /// `{maxTrials: i64, ...}` (GDS).
    pub auto_tuning_config: HashMap<String, CypherValue>,
    /// Candidate specs, each `{methodName: 'Gcn'|'GraphSage'|'RGcn'|'Gat', config: {...}}`.
    pub parameter_space: Vec<CypherValue>,
}

impl PipelineEntry {
    /// Empty pipeline with GDS's defaults for `task_type`.
    fn new(name: String, task_type: TaskType) -> Self {
        let mut split = HashMap::new();
        let test_fraction = if task_type == TaskType::LinkPrediction {
            0.1
        } else {
            0.3
        };
        split.insert(
            "testFraction".to_string(),
            CypherValue::Float(test_fraction),
        );
        split.insert("validationFolds".to_string(), CypherValue::Integer(3));
        if task_type == TaskType::LinkPrediction {
            split.insert("negativeSamplingRatio".to_string(), CypherValue::Float(1.0));
        }
        let mut auto = HashMap::new();
        auto.insert("maxTrials".to_string(), CypherValue::Integer(10));
        Self {
            name,
            task_type,
            node_property_steps: Vec::new(),
            feature_properties: Vec::new(),
            split_config: split,
            auto_tuning_config: auto,
            parameter_space: Vec::new(),
        }
    }

    /// Yield row in `PIPELINE_YIELD_COLUMNS` order.
    pub fn into_yield_row(&self, name: &str) -> Vec<(std::borrow::Cow<'static, str>, CypherValue)> {
        vec![
            (
                std::borrow::Cow::Borrowed("name"),
                CypherValue::String(name.to_string()),
            ),
            (
                std::borrow::Cow::Borrowed("nodePropertySteps"),
                CypherValue::List(self.node_property_steps.clone()),
            ),
            (
                std::borrow::Cow::Borrowed("featureProperties"),
                CypherValue::List(
                    self.feature_properties
                        .iter()
                        .map(|s| CypherValue::String(s.clone()))
                        .collect(),
                ),
            ),
            (
                std::borrow::Cow::Borrowed("splitConfig"),
                map_to_value(&self.split_config),
            ),
            (
                std::borrow::Cow::Borrowed("autoTuningConfig"),
                map_to_value(&self.auto_tuning_config),
            ),
            (
                std::borrow::Cow::Borrowed("parameterSpace"),
                CypherValue::List(self.parameter_space.clone()),
            ),
        ]
    }
}

fn map_to_value(map: &HashMap<String, CypherValue>) -> CypherValue {
    let btree: BTreeMap<String, CypherValue> =
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    CypherValue::Map(btree)
}

/// Summary for `traverse.ml.pipeline.list()`.
#[derive(Debug, Clone)]
pub struct PipelineListing {
    /// Pipeline name.
    pub name: String,
    /// Prediction task.
    pub task_type: TaskType,
    /// Model candidates in the parameter space.
    pub candidate_count: usize,
    /// Node-property feature steps.
    pub feature_stage_count: usize,
}

/// The pipelines, and where they persist.
struct Catalog {
    pipelines: BTreeMap<String, PipelineEntry>,
    /// Set once the catalog file there has been loaded; `None` keeps pipelines in memory.
    #[cfg(not(target_arch = "wasm32"))]
    dir: Option<PathBuf>,
}

impl Catalog {
    /// Sets `name` to `entry` (`None` removes it), writing the catalog first when it persists. A
    /// failed write leaves everything as it was.
    fn commit(&mut self, name: &str, entry: Option<PipelineEntry>) -> Result<(), MlError> {
        let old = match entry {
            Some(e) => self.pipelines.insert(name.to_string(), e),
            None => self.pipelines.remove(name),
        };
        if let Err(e) = self.save() {
            match old {
                Some(o) => self.pipelines.insert(name.to_string(), o),
                None => self.pipelines.remove(name),
            };
            return Err(e);
        }
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save(&self) -> Result<(), MlError> {
        match &self.dir {
            Some(dir) => persist::save(dir, self.pipelines.values()),
            None => Ok(()),
        }
    }

    /// The browser keeps pipelines in memory.
    #[cfg(target_arch = "wasm32")]
    fn save(&self) -> Result<(), MlError> {
        Ok(())
    }
}

// process-global, not thread-local: HTTP requests run on different tokio workers, so a thread-local
// pipeline would vanish between requests
static CATALOG: Mutex<Catalog> = Mutex::new(Catalog {
    pipelines: BTreeMap::new(),
    #[cfg(not(target_arch = "wasm32"))]
    dir: None,
});

// every change is applied whole or not at all, so a panic elsewhere never leaves a half-made entry
// behind and the data stays usable
fn catalog() -> MutexGuard<'static, Catalog> {
    CATALOG.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Registers a new, empty pipeline; `PipelineExists` if the name is taken.
pub fn create_pipeline(name: &str, task_type: TaskType) -> Result<PipelineEntry, MlError> {
    let mut cat = catalog();
    if cat.pipelines.contains_key(name) {
        return Err(MlError::PipelineExists(name.to_string()));
    }
    let entry = PipelineEntry::new(name.to_string(), task_type);
    cat.commit(name, Some(entry.clone()))?;
    Ok(entry)
}

/// Copy of one pipeline, if present.
pub fn snapshot(name: &str) -> Option<PipelineEntry> {
    catalog().pipelines.get(name).cloned()
}

/// Drops a pipeline; whether it existed.
pub fn drop_pipeline(name: &str) -> Result<bool, MlError> {
    let mut cat = catalog();
    if !cat.pipelines.contains_key(name) {
        return Ok(false);
    }
    cat.commit(name, None)?;
    Ok(true)
}

/// Listing for `traverse.ml.pipeline.list()`, by name.
pub fn list_pipelines() -> Vec<PipelineListing> {
    catalog()
        .pipelines
        .values()
        .map(|p| PipelineListing {
            name: p.name.clone(),
            task_type: p.task_type,
            candidate_count: p.parameter_space.len(),
            feature_stage_count: p.node_property_steps.len(),
        })
        .collect()
}

/// Test helper: empties the catalog in memory. Never called in production.
#[cfg(test)]
pub fn clear() {
    catalog().pipelines.clear();
}

/// Mutates the named pipeline of task `task` through `f`; `Err(PipelineNotFound)` if absent,
/// `Err(WrongTask)` if it belongs to another task. `f` works on a copy, so an error leaves the
/// pipeline unchanged. Backs the configuration/candidate procedures (`addNodeProperty`,
/// `selectFeatures`, `configureSplit`, `configureAutoTuning`, `add*`).
pub fn update_pipeline<F>(name: &str, task: TaskType, f: F) -> Result<PipelineEntry, MlError>
where
    F: FnOnce(&mut PipelineEntry) -> Result<(), MlError>,
{
    let mut cat = catalog();
    let mut entry = cat
        .pipelines
        .get(name)
        .cloned()
        .ok_or_else(|| MlError::PipelineNotFound(name.to_string()))?;
    if entry.task_type != task {
        return Err(MlError::WrongTask {
            pipeline: name.to_string(),
            actual: entry.task_type,
            wanted: task,
        });
    }
    f(&mut entry)?;
    // checked here, not at the next save, so a setting that can't be stored is refused even in
    // memory-only deployments
    persist::check(&entry)?;
    cat.commit(name, Some(entry.clone()))?;
    Ok(entry)
}

// --- Data directory ----------------------------------------------

static DATA_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where models persist, if anywhere.
pub fn data_dir() -> Option<PathBuf> {
    DATA_DIR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Persists ML state in `dir` from now on (the server calls it once at startup): trained models as
/// `<name>.tvmodel` (ml builds reload them with `load_models_from_dir`) and the pipeline catalog,
/// which is loaded here. Returns the pipelines loaded. If the catalog file can't be read it's
/// moved aside (named in the error) and pipelines start empty; if it can't even be moved,
/// pipelines stay in memory only, so the file is never overwritten.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_data_dir(dir: &std::path::Path) -> Result<usize, String> {
    *DATA_DIR.lock().unwrap_or_else(PoisonError::into_inner) = Some(dir.to_path_buf());
    let mut cat = catalog();
    match persist::load(dir) {
        Ok(entries) => {
            let n = entries.len();
            cat.pipelines = entries.into_iter().map(|e| (e.name.clone(), e)).collect();
            cat.dir = Some(dir.to_path_buf());
            Ok(n)
        }
        Err(e) => match e.kept_at {
            Some(kept) => {
                cat.pipelines.clear();
                cat.dir = Some(dir.to_path_buf());
                Err(format!(
                    "{}; moved it to {} and started with no pipelines",
                    e.reason,
                    kept.display()
                ))
            }
            None => {
                cat.dir = None;
                Err(format!(
                    "{}; it couldn't be moved aside, so pipelines are kept in memory only until \
                     it's fixed",
                    e.reason
                ))
            }
        },
    }
}

/// Stops persisting (tests that opened a temporary directory).
#[cfg(all(test, not(target_arch = "wasm32")))]
pub fn close_data_dir() {
    *DATA_DIR.lock().unwrap_or_else(PoisonError::into_inner) = None;
    catalog().dir = None;
}

/// Rejects names with path separators or characters problematic on common filesystems. Generated
/// names (`<target>-<date>-<short-hash>`) are safe; this catches user "Save as" names escaping the
/// directory or using reserved characters.
#[cfg(feature = "ml")]
pub fn is_safe_model_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.chars().all(|c| c == '.') {
        return false;
    }
    !name
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '<' | '>' | '|' | '\0'))
}

// --- Trained-model store -----------------------------------------
//
// Separate from the pipeline catalog: pipelines are recipes, models are
// weights, with independent lifecycles (one pipeline can train many models;
// `predict.stream` finds a model by name long after its pipeline is gone).

#[cfg(feature = "ml")]
static MODEL_STORE: Mutex<BTreeMap<String, traverse_ml::model::Model>> =
    Mutex::new(BTreeMap::new());

#[cfg(feature = "ml")]
fn models() -> MutexGuard<'static, BTreeMap<String, traverse_ml::model::Model>> {
    MODEL_STORE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// File path for the model, or `None` without persistence or for unsafe names.
#[cfg(feature = "ml")]
fn model_file_path(name: &str) -> Option<PathBuf> {
    if !is_safe_model_name(name) {
        return None;
    }
    data_dir().map(|d| d.join(format!("{name}.tvmodel")))
}

/// Writes `model` to `<name>.tvmodel` when models persist.
#[cfg(feature = "ml")]
fn save_model(name: &str, model: &traverse_ml::model::Model) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = model_file_path(name) {
        let bytes = model.to_bytes().map_err(|e| e.to_string())?;
        return traverse_core::storage::write_durable(&path, &bytes)
            .map_err(|e| format!("{}: {e}", path.display()));
    }
    let _ = (name, model);
    Ok(())
}

/// Registers a trained model, replacing any under the same name, and saves it when models persist.
/// The model is registered even if saving fails (it's usable until restart); the error says why it
/// wasn't saved.
#[cfg(feature = "ml")]
pub fn register_model(name: &str, model: traverse_ml::model::Model) -> Result<(), String> {
    let saved = save_model(name, &model);
    models().insert(name.to_string(), model);
    saved
}

/// Replaces a stored model only if it can be saved first, so memory and disk never disagree.
/// `ModelNotFound` if there's no such model.
#[cfg(feature = "ml")]
pub fn update_model(
    name: &str,
    f: impl FnOnce(&mut traverse_ml::model::Model),
) -> Result<(), MlError> {
    let mut store = models();
    let mut model = store
        .get(name)
        .cloned()
        .ok_or_else(|| MlError::ModelNotFound(name.to_string()))?;
    f(&mut model);
    save_model(name, &model).map_err(MlError::PersistFailed)?;
    store.insert(name.to_string(), model);
    Ok(())
}

/// Trained model by name.
#[cfg(feature = "ml")]
pub fn lookup_model(name: &str) -> Option<traverse_ml::model::Model> {
    models().get(name).cloned()
}

/// Drops a model and its `<name>.tvmodel`; whether it existed. If the file can't be removed the
/// model stays registered, so it doesn't come back from disk at the next start unannounced.
#[cfg(feature = "ml")]
pub fn drop_model(name: &str) -> Result<bool, MlError> {
    let mut store = models();
    if !store.contains_key(name) {
        return Ok(false);
    }
    if let Some(path) = model_file_path(name) {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(MlError::PersistFailed(format!(
                    "couldn't remove {}: {e}",
                    path.display()
                )));
            }
        }
    }
    store.remove(name);
    Ok(true)
}

/// Every model, newest first, as `(model_name, Model)` ready for yield rows.
#[cfg(feature = "ml")]
pub fn snapshot_models() -> Vec<(String, traverse_ml::model::Model)> {
    let mut out: Vec<(String, traverse_ml::model::Model)> = models()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.sort_by(|a, b| {
        b.1.metadata
            .created_at_unix
            .cmp(&a.1.metadata.created_at_unix)
    });
    out
}

/// Registers a model from raw `.tvmodel` bytes (file upload) without a restart. The caller has
/// already written the file to disk, so later saves are idempotent.
#[cfg(feature = "ml")]
pub fn register_model_from_file(name: &str, path: &std::path::Path) -> Result<(), String> {
    let model = traverse_ml::model::Model::load_from_file(path)
        .map_err(|e| format!("not a valid .tvmodel file: {e}"))?;
    models().insert(name.to_string(), model);
    Ok(())
}

/// The named model as `.tvmodel` bytes (same as `Model::save_to_file`), for downloads from servers
/// without `TRAVERSE_DATA` where no file exists on disk.
#[cfg(feature = "ml")]
pub fn encode_model_bytes(name: &str) -> Option<Vec<u8>> {
    lookup_model(name)?.to_bytes().ok()
}

/// [`register_model_from_file`] from bytes, for uploads without disk persistence; the model lives
/// only in `MODEL_STORE` and is lost on restart, like other in-memory artifacts.
#[cfg(feature = "ml")]
pub fn register_model_from_bytes(name: &str, bytes: &[u8]) -> Result<(), String> {
    let model = traverse_ml::model::Model::from_bytes(bytes)
        .map_err(|e| format!("not a valid .tvmodel file: {e}"))?;
    models().insert(name.to_string(), model);
    Ok(())
}

/// Loads every `*.tvmodel` in the directory into the store; returns how many loaded and why each
/// other one didn't. The filename becomes the model name; identity is the schema fingerprint
/// inside, so check metadata for per-database attribution.
#[cfg(feature = "ml")]
pub fn load_models_from_dir(dir: &std::path::Path) -> (usize, Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return (0, vec![format!("{}: {e}", dir.display())]),
    };
    let mut loaded = 0usize;
    let mut failed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("tvmodel") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if is_safe_model_name(s) => s.to_string(),
            _ => {
                failed.push(format!("{}: not a usable model name", path.display()));
                continue;
            }
        };
        match traverse_ml::model::Model::load_from_file(&path) {
            Ok(model) => {
                models().insert(stem, model);
                loaded += 1;
            }
            Err(e) => failed.push(format!("{}: {e}", path.display())),
        }
    }
    (loaded, failed)
}
