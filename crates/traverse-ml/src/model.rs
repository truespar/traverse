//! Model catalog and `.tvmodel` sidecar persistence.
//!
//! A trained model has two parts:
//!
//! 1. **Weights**: opaque bytes from Burn's `BinBytesRecorder`. The caller
//!    knows the architecture (it calls `MyModel::default().load_record(record)`);
//!    the catalog just stores and returns the bytes.
//! 2. **Metadata**: JSON-friendly fields (kind, dims, training metrics, schema
//!    fingerprint, creation time, free-form extra map).
//!
//! Both go into one `.tvmodel` file, a framed, checksummed `traverse-codec`
//! container (see [`Model::to_bytes`]; raw-bincode files from 0.8.2 and
//! earlier still load). The catalog is a `HashMap<String, Model>` in memory
//! plus a directory on disk; `save_to_disk` / `load_from_disk` round-trip it.
//!
//! Catalogs are database-scoped. The schema fingerprint is deliberately
//! simple: a 64-bit hash of `(num_nodes, num_edges, feature_dim, num_classes)`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

/// One trained model: weights + metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    /// Architecture id, e.g. `"gcn-2layer"`; picks the right `init + load_record` path when
    /// rebuilding the module.
    pub kind: String,
    /// Everything but the weights.
    pub metadata: ModelMetadata,
    /// Burn `BinBytesRecorder` output for this model's record; opaque to the catalog.
    pub weights: Vec<u8>,
}

/// Free-form bookkeeping stored with a model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Unix time of `save_to_file` (or `now()` if unset).
    pub created_at_unix: u64,
    /// Input feature dim expected at inference.
    pub feature_dim: usize,
    /// Output dim (classes for NC, 1 for NR).
    pub num_classes: usize,
    /// Nodes in the training graph.
    pub train_graph_nodes: usize,
    /// (Directed) edges in the training CSR; an undirected source graph gives `2 *
    /// undirected_edges`.
    pub train_graph_edges: usize,
    /// Final training metrics (`"loss"`, `"val_acc"`, `"test_acc"`, ...).
    pub training_metrics: HashMap<String, f32>,
    /// 64-bit hash of the training graph shape + feature/class sizes; predict refuses a graph whose
    /// fingerprint differs (schema drift guard).
    pub schema_fingerprint: u64,
    /// Free-form key/value pairs for callers (hyperparameter snapshots, git sha, dataset name, ...).
    pub extra: HashMap<String, String>,
}

impl ModelMetadata {
    /// Metadata with `created_at_unix` = now. `SystemTime::now()` panics on
    /// `wasm32-unknown-unknown` (no clock), so it's 0 there; browser code can set it from
    /// `Date.now() / 1000`.
    pub fn now() -> Self {
        let created_at_unix = current_unix_seconds();
        Self {
            created_at_unix,
            ..Default::default()
        }
    }

    /// Computes the schema fingerprint from the training graph's shape and the model's IO dims,
    /// stores it in `self.schema_fingerprint` and returns it. FNV-1a 64-bit, not `DefaultHasher`
    /// (seeded per process), so the same trainer gives the same fingerprint across runs and train ->
    /// save -> reload -> predict works.
    pub fn set_schema_fingerprint(&mut self) -> u64 {
        let mut fp: u64 = 0xcbf29ce484222325; // FNV-1a 64 offset basis
        let prime: u64 = 0x100000001b3; // FNV-1a 64 prime
        for &v in &[
            self.train_graph_nodes as u64,
            self.train_graph_edges as u64,
            self.feature_dim as u64,
            self.num_classes as u64,
        ] {
            for byte in v.to_le_bytes() {
                fp ^= byte as u64;
                fp = fp.wrapping_mul(prime);
            }
        }
        self.schema_fingerprint = fp;
        fp
    }
}

impl Model {
    /// `.tvmodel` bytes: a framed, CRC-32-checked [`traverse_codec`] container around the postcard
    /// encoding of `self`. Exactly the content of a `.tvmodel` file and of the
    /// `traverse.ml.model.export` / HTTP download payloads.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        traverse_codec::encode(traverse_codec::Kind::Model, self)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))
    }

    /// Parses `.tvmodel` bytes: the current framed layout, or the headerless bincode layout of
    /// 0.8.2 and earlier (via `traverse_codec::legacy`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        traverse_codec::decode(traverse_codec::Kind::Model, bytes)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))
    }

    /// Writes `self` to `path` as a `.tvmodel` (the extension is up to the caller).
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let bytes = self.to_bytes()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)
    }

    /// Reads a `.tvmodel` file.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// Can this model be applied to a graph of this shape? Returns the fingerprint comparison so
    /// callers choose between error and warning.
    pub fn check_schema(&self, graph_nodes: usize, graph_edges: usize, feature_dim: usize) -> bool {
        let mut probe = ModelMetadata {
            train_graph_nodes: graph_nodes,
            train_graph_edges: graph_edges,
            feature_dim,
            num_classes: self.metadata.num_classes,
            ..Default::default()
        };
        let probe_fp = probe.set_schema_fingerprint();
        probe_fp == self.metadata.schema_fingerprint
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
fn current_unix_seconds() -> u64 {
    // no clock on wasm32-unknown-unknown (SystemTime::now() panics); callers wanting a real time
    // set it from JS `Date.now()`
    0
}

// --- storage abstraction -----------------------------------------
//
// `ModelStorage` splits the catalog into two layers, so it works where
// `std::fs` doesn't (browser WASM):
//
// * **Catalog logic**: encode each model (`Model::to_bytes`), route reads and
//   writes through a storage backend (key -> bytes).
// * **Storage backend**: the actual persistence. Native uses
//   [`FileSystemStorage`]; tests and in-process WASM use [`InMemoryStorage`];
//   browsers wanting real persistence wrap an OPFS handle in their own
//   `ModelStorage` and inject it via [`ModelCatalog::with_storage`].
//
// All methods are **sync**. With OPFS, pre-load every model into an in-memory
// map at app start (async, JS side), build the catalog on that storage, and
// flush writes back to OPFS from an async JS-side queue.

/// Pluggable persistence for [`ModelCatalog`]: entries keyed by model name (e.g. `"my_gcn"`),
/// stored as opaque blobs (the `.tvmodel` bytes from [`Model::to_bytes`]).
pub trait ModelStorage: Send + Sync {
    /// Every model key in storage, in no particular order.
    fn list_keys(&self) -> Result<Vec<String>>;

    /// Bytes for `key`; `Err(io::ErrorKind::NotFound)` if absent.
    fn read_bytes(&self, key: &str) -> Result<Vec<u8>>;

    /// Stores `bytes` under `key`, replacing any previous value.
    fn write_bytes(&self, key: &str, bytes: &[u8]) -> Result<()>;

    /// Removes `key`; no-op if absent.
    fn delete(&self, key: &str) -> Result<()>;

    /// Filesystem path backing this storage, if any: `None` by default (in-memory / OPFS / custom),
    /// [`FileSystemStorage`] returns its directory. For FS-only callers' logging.
    fn filesystem_path(&self) -> Option<&Path> {
        None
    }
}

/// Filesystem storage: each model is `{dir}/{name}.tvmodel`. The directory is created on the first
/// `write_bytes`; a missing directory lists as empty.
pub struct FileSystemStorage {
    dir: PathBuf,
}

impl FileSystemStorage {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl ModelStorage for FileSystemStorage {
    fn list_keys(&self) -> Result<Vec<String>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("tvmodel") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                keys.push(stem.to_string());
            }
        }
        Ok(keys)
    }

    fn read_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.dir.join(format!("{key}.tvmodel"));
        std::fs::read(path)
    }

    fn write_bytes(&self, key: &str, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{key}.tvmodel"));
        std::fs::write(path, bytes)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.dir.join(format!("{key}.tvmodel"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    fn filesystem_path(&self) -> Option<&Path> {
        Some(&self.dir)
    }
}

/// In-memory storage, no persistence: for tests, short-lived catalogs, and the in-process buffer of
/// WASM deployments that flush to OPFS from JS. Thread-safe (internal `Mutex`), so several catalog
/// handles can share one.
pub struct InMemoryStorage {
    entries: std::sync::Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Seeds the storage with `(key, bytes)` entries, e.g. a WASM bootstrap that reads `.tvmodel`
    /// blobs from OPFS in JS and hands them over via wasm-bindgen before the catalog is built.
    pub fn from_entries(entries: HashMap<String, Vec<u8>>) -> Self {
        Self {
            entries: std::sync::Mutex::new(entries),
        }
    }

    /// All entries, for flushing back to OPFS or shipping the whole catalog elsewhere.
    pub fn snapshot(&self) -> HashMap<String, Vec<u8>> {
        self.entries
            .lock()
            .expect("InMemoryStorage mutex poisoned")
            .clone()
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelStorage for InMemoryStorage {
    fn list_keys(&self) -> Result<Vec<String>> {
        Ok(self
            .entries
            .lock()
            .expect("InMemoryStorage mutex poisoned")
            .keys()
            .cloned()
            .collect())
    }

    fn read_bytes(&self, key: &str) -> Result<Vec<u8>> {
        self.entries
            .lock()
            .expect("InMemoryStorage mutex poisoned")
            .get(key)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no entry '{key}'")))
    }

    fn write_bytes(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.entries
            .lock()
            .expect("InMemoryStorage mutex poisoned")
            .insert(key.to_string(), bytes.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.entries
            .lock()
            .expect("InMemoryStorage mutex poisoned")
            .remove(key);
        Ok(())
    }
}

/// In-memory model catalog with pluggable persistence: models in a `HashMap`,
/// persistence delegated to a [`ModelStorage`]. Writes are **not
/// auto-flushed**: call [`save_all`](Self::save_all) after `insert`/`drop`
/// (keeps writes batchable). Native code usually uses [`new`](Self::new) or
/// [`load_from_disk`](Self::load_from_disk) (both on a [`FileSystemStorage`]);
/// browser WASM builds an [`InMemoryStorage`] (maybe pre-seeded from OPFS) and
/// passes it to [`with_storage`](Self::with_storage).
pub struct ModelCatalog {
    storage: Box<dyn ModelStorage>,
    models: HashMap<String, Model>,
}

impl ModelCatalog {
    /// Empty catalog on a [`FileSystemStorage`] at `catalog_dir`, created on the first
    /// `save_all()`.
    pub fn new(catalog_dir: PathBuf) -> Self {
        Self::with_storage(Box::new(FileSystemStorage::new(catalog_dir)))
    }

    /// Empty catalog on any storage, e.g. [`InMemoryStorage`] or a custom OPFS adapter for WASM.
    pub fn with_storage(storage: Box<dyn ModelStorage>) -> Self {
        Self {
            storage,
            models: HashMap::new(),
        }
    }

    /// Every model from the storage backend; empty storage gives an empty catalog.
    pub fn load_all(storage: Box<dyn ModelStorage>) -> Result<Self> {
        let mut catalog = Self::with_storage(storage);
        let keys = catalog.storage.list_keys()?;
        for key in keys {
            let bytes = catalog.storage.read_bytes(&key)?;
            let model = Model::from_bytes(&bytes)
                .map_err(|e| Error::new(e.kind(), format!("model '{key}': {e}")))?;
            catalog.models.insert(key, model);
        }
        Ok(catalog)
    }

    /// Filesystem shorthand for `load_all(Box::new(FileSystemStorage::new(catalog_dir)))`.
    pub fn load_from_disk(catalog_dir: PathBuf) -> Result<Self> {
        Self::load_all(Box::new(FileSystemStorage::new(catalog_dir)))
    }

    /// Flushes every in-memory model to the storage backend.
    pub fn save_all(&self) -> Result<()> {
        for (name, model) in &self.models {
            let bytes = model.to_bytes()?;
            self.storage.write_bytes(name, &bytes)?;
        }
        Ok(())
    }

    /// Filesystem-named alias of [`save_all`](Self::save_all).
    pub fn save_to_disk(&self) -> Result<()> {
        self.save_all()
    }

    /// Adds or replaces a model under `name`; storage untouched.
    pub fn insert(&mut self, name: impl Into<String>, model: Model) {
        self.models.insert(name.into(), model);
    }

    pub fn get(&self, name: &str) -> Option<&Model> {
        self.models.get(name)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.models.contains_key(name)
    }

    /// Registered model names.
    pub fn list(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(|s| s.as_str())
    }

    /// Removes from memory only; `save_all()` won't write the dropped entry,
    /// [`delete_from_storage`](Self::delete_from_storage) removes the persisted one.
    pub fn drop_model(&mut self, name: &str) -> Option<Model> {
        self.models.remove(name)
    }

    /// Removes the persisted entry for `name` (memory untouched; see `drop_model`).
    pub fn delete_from_storage(&self, name: &str) -> Result<()> {
        self.storage.delete(name)
    }

    /// Alias of [`delete_from_storage`](Self::delete_from_storage).
    pub fn delete_from_disk(&self, name: &str) -> Result<()> {
        self.delete_from_storage(name)
    }

    /// The underlying storage, e.g. for FS-only callers querying the catalog directory.
    pub fn storage(&self) -> &dyn ModelStorage {
        self.storage.as_ref()
    }

    /// Filesystem path of a [`FileSystemStorage`] backend; `None` for in-memory / OPFS / custom.
    /// For "saved to {dir}" messages.
    pub fn catalog_dir(&self) -> Option<&Path> {
        self.storage.filesystem_path()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use burn::module::Module;
    use burn::nn::{Linear, LinearConfig};
    use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
    use burn::tensor::Tensor;

    type B = burn::backend::NdArray;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    #[test]
    fn metadata_fingerprint_is_stable() {
        let mut m1 = ModelMetadata {
            train_graph_nodes: 10000,
            train_graph_edges: 200000,
            feature_dim: 16,
            num_classes: 2,
            ..Default::default()
        };
        let mut m2 = m1.clone();
        let fp1 = m1.set_schema_fingerprint();
        let fp2 = m2.set_schema_fingerprint();
        assert_eq!(fp1, fp2, "fingerprint must be deterministic");

        // different shape -> different fingerprint
        let mut m3 = m1.clone();
        m3.train_graph_nodes += 1;
        let fp3 = m3.set_schema_fingerprint();
        assert_ne!(fp1, fp3, "node-count change must shift fingerprint");
    }

    #[test]
    fn model_round_trip_to_file() {
        let dir = tempdir();
        let path = dir.path().join("test.tvmodel");

        let mut meta = ModelMetadata::now();
        meta.feature_dim = 16;
        meta.num_classes = 2;
        meta.train_graph_nodes = 10000;
        meta.train_graph_edges = 243432;
        meta.training_metrics.insert("loss".into(), 0.57);
        meta.training_metrics.insert("test_acc".into(), 0.568);
        meta.extra.insert("dataset".into(), "pokec_small".into());
        meta.set_schema_fingerprint();

        let original = Model {
            kind: "gcn-2layer".into(),
            metadata: meta,
            weights: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x00, 0x01, 0x02],
        };

        original.save_to_file(&path).expect("save");
        let loaded = Model::load_from_file(&path).expect("load");

        assert_eq!(loaded.kind, original.kind);
        assert_eq!(loaded.weights, original.weights);
        assert_eq!(loaded.metadata.feature_dim, original.metadata.feature_dim);
        assert_eq!(loaded.metadata.num_classes, original.metadata.num_classes);
        assert_eq!(
            loaded.metadata.schema_fingerprint,
            original.metadata.schema_fingerprint
        );
        assert_eq!(
            loaded.metadata.training_metrics.get("test_acc"),
            Some(&0.568)
        );
        assert_eq!(
            loaded.metadata.extra.get("dataset"),
            Some(&"pokec_small".to_string())
        );
    }

    #[test]
    fn catalog_save_load_round_trip() {
        let dir = tempdir();
        let mut cat = ModelCatalog::new(dir.path().to_path_buf());

        let mk = |kind: &str, n_classes| {
            let mut meta = ModelMetadata::now();
            meta.num_classes = n_classes;
            meta.set_schema_fingerprint();
            Model {
                kind: kind.into(),
                metadata: meta,
                weights: vec![1, 2, 3, 4, 5],
            }
        };
        cat.insert("alpha", mk("gcn-2layer", 2));
        cat.insert("beta", mk("graphsage", 7));

        assert_eq!(cat.len(), 2);
        assert!(cat.exists("alpha"));
        assert!(cat.exists("beta"));
        cat.save_to_disk().expect("save catalog");

        // files exist on disk
        assert!(dir.path().join("alpha.tvmodel").exists());
        assert!(dir.path().join("beta.tvmodel").exists());

        // reload into a fresh catalog
        let reloaded =
            ModelCatalog::load_from_disk(dir.path().to_path_buf()).expect("load catalog");
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get("alpha").unwrap().kind, "gcn-2layer");
        assert_eq!(reloaded.get("beta").unwrap().kind, "graphsage");
        assert_eq!(reloaded.get("beta").unwrap().metadata.num_classes, 7);

        // drop + delete_from_disk semantics
        let mut reloaded = reloaded;
        reloaded.drop_model("alpha");
        assert_eq!(reloaded.len(), 1);
        reloaded.delete_from_disk("alpha").expect("delete");
        assert!(!dir.path().join("alpha.tvmodel").exists());
        // beta untouched on disk
        assert!(dir.path().join("beta.tvmodel").exists());
    }

    #[test]
    fn check_schema_round_trips() {
        let mut meta = ModelMetadata::now();
        meta.train_graph_nodes = 100;
        meta.train_graph_edges = 500;
        meta.feature_dim = 8;
        meta.num_classes = 3;
        meta.set_schema_fingerprint();
        let m = Model {
            kind: "x".into(),
            metadata: meta,
            weights: vec![],
        };
        assert!(m.check_schema(100, 500, 8));
        assert!(!m.check_schema(101, 500, 8)); // node-count drift
        assert!(!m.check_schema(100, 500, 16)); // feature-dim drift
    }

    /// Burn recorder round-trip on its own: no `.tvmodel` encoding, no filesystem, no catalog.
    #[test]
    fn burn_recorder_only_round_trip_no_bias() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();

        // bias=false: does the weight tensor alone round-trip?
        let linear_a: Linear<B> = LinearConfig::new(4, 3).with_bias(false).init(&device);
        let weight_a: Vec<f32> = linear_a.weight.val().into_data().iter::<f32>().collect();
        eprintln!("weight_a (original):    {weight_a:?}");

        let bytes = recorder
            .record(linear_a.clone().into_record(), ())
            .expect("record");
        eprintln!(
            "recorded byte length:   {} (weight has {} floats = {} bytes)",
            bytes.len(),
            weight_a.len(),
            weight_a.len() * 4
        );
        let record = recorder.load(bytes, &device).expect("recorder load");

        let linear_fresh: Linear<B> = LinearConfig::new(4, 3).with_bias(false).init(&device);
        let weight_fresh: Vec<f32> = linear_fresh
            .weight
            .val()
            .into_data()
            .iter::<f32>()
            .collect();
        eprintln!("weight_fresh (init):    {weight_fresh:?}");

        let linear_b = linear_fresh.load_record(record);
        let weight_b: Vec<f32> = linear_b.weight.val().into_data().iter::<f32>().collect();
        eprintln!("weight_b (post-load):   {weight_b:?}");

        for (a, b) in weight_a.iter().zip(weight_b.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "Weight tensor drifted across record/load: {a} vs {b}"
            );
        }

        // forward outputs match: equal weights and no bias give identical forward(x)
        let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(
            [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8].as_slice(),
            &device,
        )
        .reshape([2, 4]);
        let y_a: Vec<f32> = linear_a
            .forward(x.clone())
            .into_data()
            .iter::<f32>()
            .collect();
        let y_b: Vec<f32> = linear_b.forward(x).into_data().iter::<f32>().collect();
        eprintln!("y_a (linear_a forward): {y_a:?}");
        eprintln!("y_b (linear_b forward): {y_b:?}");
        for (a, b) in y_a.iter().zip(y_b.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "Forward output drifted despite weight match: {a} vs {b}"
            );
        }
    }

    /// Same, with the default `bias=true`: the bias survives alongside the weight.
    #[test]
    fn burn_recorder_only_round_trip_with_bias() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();

        let linear_a: Linear<B> = LinearConfig::new(4, 3).init(&device);
        let weight_a: Vec<f32> = linear_a.weight.val().into_data().iter::<f32>().collect();
        let bias_a: Vec<f32> = linear_a
            .bias
            .as_ref()
            .expect("bias=true")
            .val()
            .into_data()
            .iter::<f32>()
            .collect();

        let bytes = recorder
            .record(linear_a.clone().into_record(), ())
            .expect("record");
        let record = recorder.load(bytes, &device).expect("recorder load");

        let linear_b = LinearConfig::new(4, 3).init(&device).load_record(record);
        let weight_b: Vec<f32> = linear_b.weight.val().into_data().iter::<f32>().collect();
        let bias_b: Vec<f32> = linear_b
            .bias
            .as_ref()
            .expect("bias=true")
            .val()
            .into_data()
            .iter::<f32>()
            .collect();

        for (a, b) in weight_a.iter().zip(weight_b.iter()) {
            assert!((a - b).abs() < 1e-6, "weight drift: {a} vs {b}");
        }
        for (a, b) in bias_a.iter().zip(bias_b.iter()) {
            assert!((a - b).abs() < 1e-6, "bias drift: {a} vs {b}");
        }

        // forward outputs match end to end
        let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(
            [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8].as_slice(),
            &device,
        )
        .reshape([2, 4]);
        let y_a: Vec<f32> = linear_a
            .forward(x.clone())
            .into_data()
            .iter::<f32>()
            .collect();
        let y_b: Vec<f32> = linear_b.forward(x).into_data().iter::<f32>().collect();
        for (a, b) in y_a.iter().zip(y_b.iter()) {
            assert!((a - b).abs() < 1e-6, "with-bias forward drift: {a} vs {b}");
        }
    }

    /// Burn module weights survive the whole catalog path (record -> `.tvmodel` bytes -> file ->
    /// read back -> decode -> recorder load -> load_record). `linear.clone().into_record()` keeps
    /// the original for forward comparison; weight and bias are checked directly *before* the
    /// forward output, since a forward-only check can hide partial loads.
    #[test]
    fn burn_module_weights_round_trip() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let linear_original: Linear<B> = LinearConfig::new(4, 3).init(&device);

        let weight_orig: Vec<f32> = linear_original
            .weight
            .val()
            .into_data()
            .iter::<f32>()
            .collect();
        let bias_orig: Vec<f32> = linear_original
            .bias
            .as_ref()
            .expect("bias=true")
            .val()
            .into_data()
            .iter::<f32>()
            .collect();

        // Burn -> bytes -> Model -> .tvmodel
        let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();
        let weights = recorder
            .record(linear_original.clone().into_record(), ())
            .expect("record");
        let mut meta = ModelMetadata::now();
        meta.feature_dim = 4;
        meta.num_classes = 3;
        meta.set_schema_fingerprint();
        let model = Model {
            kind: "single-linear".into(),
            metadata: meta,
            weights,
        };
        let dir = tempdir();
        let path = dir.path().join("linear.tvmodel");
        model.save_to_file(&path).expect("save");
        let loaded = Model::load_from_file(&path).expect("load");

        // back: .tvmodel -> Model -> Burn record -> fresh module
        let record = recorder
            .load(loaded.weights, &device)
            .expect("recorder load");
        let reborn = LinearConfig::new(4, 3).init(&device).load_record(record);

        let weight_reborn: Vec<f32> = reborn.weight.val().into_data().iter::<f32>().collect();
        let bias_reborn: Vec<f32> = reborn
            .bias
            .as_ref()
            .expect("bias=true")
            .val()
            .into_data()
            .iter::<f32>()
            .collect();

        for (a, b) in weight_orig.iter().zip(weight_reborn.iter()) {
            assert!((a - b).abs() < 1e-6, "weight drift: {a} vs {b}");
        }
        for (a, b) in bias_orig.iter().zip(bias_reborn.iter()) {
            assert!((a - b).abs() < 1e-6, "bias drift: {a} vs {b}");
        }

        // forward outputs agree
        let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(
            [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8].as_slice(),
            &device,
        )
        .reshape([2, 4]);
        let y_orig: Vec<f32> = linear_original
            .forward(x.clone())
            .into_data()
            .iter::<f32>()
            .collect();
        let y_reborn: Vec<f32> = reborn.forward(x).into_data().iter::<f32>().collect();
        for (a, b) in y_orig.iter().zip(y_reborn.iter()) {
            assert!((a - b).abs() < 1e-6, "forward output drift: {a} vs {b}");
        }
    }

    // --- storage abstraction tests -------------------------------

    fn sample_model() -> Model {
        let mut meta = ModelMetadata::now();
        meta.feature_dim = 4;
        meta.num_classes = 2;
        meta.set_schema_fingerprint();
        Model {
            kind: "test".into(),
            metadata: meta,
            weights: vec![0xAB, 0xCD, 0xEF, 0x42, 0x01, 0x02, 0x03, 0x04],
        }
    }

    #[test]
    fn in_memory_storage_round_trips() {
        let storage = InMemoryStorage::new();
        storage.write_bytes("alpha", b"hello").expect("write alpha");
        storage.write_bytes("beta", b"world").expect("write beta");
        let mut keys = storage.list_keys().expect("list");
        keys.sort();
        assert_eq!(keys, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(storage.read_bytes("alpha").expect("read alpha"), b"hello");
        assert_eq!(storage.read_bytes("beta").expect("read beta"), b"world");
        storage.delete("alpha").expect("delete alpha");
        assert_eq!(storage.list_keys().expect("list").len(), 1);
        assert!(storage.read_bytes("alpha").is_err());
    }

    #[test]
    fn in_memory_storage_filesystem_path_is_none() {
        let storage = InMemoryStorage::new();
        assert!(storage.filesystem_path().is_none());
    }

    #[test]
    fn filesystem_storage_filesystem_path_is_set() {
        let dir = tempdir();
        let storage = FileSystemStorage::new(dir.path().to_path_buf());
        assert_eq!(storage.filesystem_path(), Some(dir.path()));
    }

    #[test]
    fn catalog_with_in_memory_storage_does_not_touch_disk() {
        let dir = tempdir();
        // InMemoryStorage, plus a tempdir to prove nothing gets written there
        let mut cat = ModelCatalog::with_storage(Box::new(InMemoryStorage::new()));
        cat.insert("m1", sample_model());
        cat.insert("m2", sample_model());
        cat.save_all().expect("save");
        // nothing under `dir`
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "InMemoryStorage shouldn't touch disk; found {entries:?}"
        );
        // catalog_dir is None for non-FS backends
        assert!(cat.catalog_dir().is_none());
    }

    #[test]
    fn catalog_save_and_load_via_in_memory_storage() {
        // write through a shared InMemoryStorage, read back via a fresh catalog on the same
        // storage: a round trip without the filesystem
        let storage = std::sync::Arc::new(InMemoryStorage::new());
        // first catalog: insert + save
        struct SharedStorage(std::sync::Arc<InMemoryStorage>);
        impl ModelStorage for SharedStorage {
            fn list_keys(&self) -> Result<Vec<String>> {
                self.0.list_keys()
            }
            fn read_bytes(&self, key: &str) -> Result<Vec<u8>> {
                self.0.read_bytes(key)
            }
            fn write_bytes(&self, key: &str, bytes: &[u8]) -> Result<()> {
                self.0.write_bytes(key, bytes)
            }
            fn delete(&self, key: &str) -> Result<()> {
                self.0.delete(key)
            }
        }
        let mut cat = ModelCatalog::with_storage(Box::new(SharedStorage(storage.clone())));
        cat.insert("the-model", sample_model());
        cat.save_all().expect("save");

        // the shared storage now has one entry
        let snap = storage.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key("the-model"));

        // second catalog: load_all from the shared storage
        let reloaded =
            ModelCatalog::load_all(Box::new(SharedStorage(storage.clone()))).expect("load");
        assert_eq!(reloaded.len(), 1);
        let got = reloaded.get("the-model").expect("present");
        assert_eq!(got.kind, "test");
        assert_eq!(got.metadata.feature_dim, 4);
        assert_eq!(got.metadata.num_classes, 2);
    }

    #[test]
    fn in_memory_storage_seeds_from_entries() {
        // the WASM bootstrap pattern: JS pre-loads model bytes from OPFS and hands them over via
        // wasm-bindgen, Rust seeds the storage, the catalog loads from it
        let bytes = sample_model().to_bytes().expect("encode");
        let mut entries = HashMap::new();
        entries.insert("seeded".to_string(), bytes);
        let storage = InMemoryStorage::from_entries(entries);
        let cat = ModelCatalog::load_all(Box::new(storage)).expect("load");
        assert_eq!(cat.len(), 1);
        assert!(cat.exists("seeded"));
    }

    /// A 0.8.2 `.tvmodel` (raw bincode, no header) still loads, and re-saving it gives the framed
    /// layout.
    #[test]
    fn legacy_bincode_tvmodel_from_0_8_2_still_loads() {
        const LEGACY: &[u8] = include_bytes!("../tests/fixtures/tiny_gcn_0.8.2_bincode.tvmodel");
        assert!(!traverse_codec::is_framed(LEGACY));

        let model = Model::from_bytes(LEGACY).expect("legacy decode");
        assert_eq!(model.kind, "gcn-2layer");
        assert_eq!(model.metadata.feature_dim, 2);
        assert_eq!(model.metadata.num_classes, 2);
        assert_eq!(model.metadata.train_graph_nodes, 4);
        assert!(!model.weights.is_empty());

        let framed = model.to_bytes().expect("encode");
        assert!(traverse_codec::is_framed(&framed));
        assert_eq!(
            traverse_codec::kind_of(&framed),
            Some(traverse_codec::Kind::Model)
        );
        let again = Model::from_bytes(&framed).expect("framed decode");
        assert_eq!(again.kind, model.kind);
        assert_eq!(again.weights, model.weights);
        assert_eq!(
            again.metadata.schema_fingerprint,
            model.metadata.schema_fingerprint
        );
        assert_eq!(
            again.metadata.training_metrics,
            model.metadata.training_metrics
        );
        assert_eq!(again.metadata.extra, model.metadata.extra);

        // and the catalog path (used by the server and the WASM handle)
        let mut entries = HashMap::new();
        entries.insert("legacy".to_string(), LEGACY.to_vec());
        let cat = ModelCatalog::load_all(Box::new(InMemoryStorage::from_entries(entries)))
            .expect("catalog load");
        assert!(cat.exists("legacy"));
    }

    /// A corrupted framed file fails with a checksum error instead of loading silently wrong
    /// weights.
    #[test]
    fn corrupted_tvmodel_is_rejected() {
        let mut bytes = sample_model().to_bytes().expect("encode");
        let last_weight_byte = bytes.len() - traverse_codec::TRAILER_LEN - 1;
        bytes[last_weight_byte] ^= 0x01;
        let err = Model::from_bytes(&bytes).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(err.to_string().contains("CRC-32"), "{err}");
    }
}
