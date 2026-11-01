use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tracing::info;

use traverse_core::graph::Graph;
use traverse_core::storage::database::Database;
use traverse_core::storage::rlog::{self, LogWriter};
use traverse_core::tx::{Checkpoint, GraphView, MvccStore};
use traverse_cypher::plan_cache::PlanCache;

use crate::auth::{AuthCatalog, UserChange, UserError};
use crate::sync::{LockUnpoisoned, Unpoisoned};

/// Installs the rlog writer (if any) on a freshly built store, so commits stay durable after
/// `Database::into_parts` hands the graph to the server's own store. In `wal-async` mode commits
/// become non-blocking; the background fsync task (startup) bounds the crash window to the sync
/// interval.
pub(crate) fn install_writer(store: MvccStore, writer: Option<Arc<Mutex<LogWriter>>>) -> MvccStore {
    match writer {
        Some(w) => {
            let store = store.with_log_writer(w);
            if durability_mode() == DurabilityMode::WalAsync {
                store.set_sync_on_commit(false);
            }
            store
        }
        None => store,
    }
}

/// `install_writer`, carrying over the writerless-replay floor from `Database::into_parts`
/// (snapshot mode replaying a prior WAL run's tail: the floor must reach the server's store so the
/// next snapshot save stamps it).
pub(crate) fn install_writer_with_floor(
    store: MvccStore,
    writer: Option<Arc<Mutex<LogWriter>>>,
    recovery_floor: u64,
) -> MvccStore {
    let store = install_writer(store, writer);
    if recovery_floor > 0 {
        store.set_recovery_floor(recovery_floor);
    }
    store
}

/// Durability tiers. Default `Snapshot`: in memory, snapshotted on save/shutdown/checkpoint.
/// `wal-async` appends every commit to the rlog but acks without waiting for fdatasync; a
/// background task syncs on an interval, bounding crash loss to that interval at near-snapshot
/// throughput (Postgres `synchronous_commit=off` / Redis `appendfsync everysec`). `wal` blocks each
/// ack on fdatasync: no loss, amortized by group commit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DurabilityMode {
    Snapshot,
    WalAsync,
    WalSync,
}

pub(crate) fn durability_mode() -> DurabilityMode {
    // startup resolves config (CLI > env > TOML) and re-exports the result here; raw env spellings
    // work for embedded/test use
    match std::env::var("TRAVERSE_DURABILITY").as_deref() {
        Ok("wal") | Ok("WAL") | Ok("full") | Ok("1") | Ok("true") => DurabilityMode::WalSync,
        Ok("wal-async") | Ok("wal_async") | Ok("walasync") | Ok("async") => {
            DurabilityMode::WalAsync
        }
        _ => DurabilityMode::Snapshot,
    }
}

/// Should an rlog writer be attached (either WAL tier)?
pub(crate) fn wal_durability_enabled() -> bool {
    durability_mode() != DurabilityMode::Snapshot
}

/// Saves `store` to `path` as of a checkpoint of it, installed durably, then drops the log
/// segments the file covers (best effort: replay skips them anyway). Reads and commits carry on
/// while it's written. Returns `(nodes, edges)` saved.
pub(crate) fn save_store(
    path: &Path,
    store: &MvccStore,
    metadata: &[u8],
) -> traverse_core::error::Result<(usize, usize)> {
    let checkpoint = store.checkpoint()?;
    install_checkpoint(path, store, &checkpoint, metadata)
}

/// Installs `checkpoint` of `store` at `path`, then drops the log segments it covers.
fn install_checkpoint(
    path: &Path,
    store: &MvccStore,
    checkpoint: &Checkpoint<'_>,
    metadata: &[u8],
) -> traverse_core::error::Result<(usize, usize)> {
    let counts = Database::save_checkpoint(path, checkpoint, metadata)?;
    if let Some(w) = store.log_writer()
        && let Err(e) = w.lock_unpoisoned().truncate_at(checkpoint.position())
    {
        tracing::warn!(
            "rlog truncation after saving {} failed: {e}",
            path.display()
        );
    }
    Ok(counts)
}

/// A database file as client-facing errors name it: the file, not where the server keeps it.
fn file_label(path: &Path) -> std::borrow::Cow<'_, str> {
    match path.file_name() {
        Some(name) => name.to_string_lossy(),
        None => path.to_string_lossy(),
    }
}

/// Moves a database's files from `old_path` to `new_path`. A database with a redo log (WAL modes,
/// or one replayed at open) moves its log too: commits pause while a checkpoint goes to the new
/// path and the writer moves to the new name, and the old files go once that checkpoint covers
/// them. Reads carry on. Without a log it's a rename.
fn move_database_files(store: &MvccStore, old_path: &Path, new_path: &Path) -> Result<(), String> {
    let segments = |path: &Path| {
        rlog::segments_of(path)
            .map_err(|e| format!("couldn't list the redo log of {}: {e}", file_label(path)))
    };
    let writer = store.log_writer();
    if writer.is_none() && segments(old_path)?.is_empty() {
        if old_path.exists() {
            std::fs::rename(old_path, new_path)
                .map_err(|e| format!("failed to rename file: {e}"))?;
        }
        return Ok(());
    }

    let pause = store.pause_commits();
    let old_segments = segments(old_path)?;
    let checkpoint = pause
        .checkpoint()
        .map_err(|e| format!("failed to save {}: {e}", file_label(new_path)))?;
    Database::save_checkpoint(new_path, &checkpoint, &[])
        .map_err(|e| format!("failed to save {}: {e}", file_label(new_path)))?;
    drop(checkpoint);
    if let Some(w) = &writer
        && let Err(e) = w.lock_unpoisoned().retarget(new_path)
    {
        let _ = std::fs::remove_file(new_path);
        return Err(format!("couldn't move the redo log: {e}"));
    }
    drop(pause);

    if let Err(e) = rlog::remove_segments(&old_segments) {
        tracing::warn!(
            "couldn't remove the old redo log of {}: {e}",
            old_path.display()
        );
    }
    if let Err(e) = std::fs::remove_file(old_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("couldn't remove {}: {e}", old_path.display());
    }
    Ok(())
}

/// Opens a fresh rlog writer for a database created directly (not via `Database::open`), e.g.
/// `CREATE DATABASE`. Best-effort: on failure the database just lacks per-commit durability instead
/// of failing to create.
pub(crate) fn open_writer_for(path: &Path) -> Option<Arc<Mutex<LogWriter>>> {
    use traverse_core::storage::rlog::WriterOptions;
    let opts = WriterOptions::from_tvdb_path(path).ok()?;
    match LogWriter::open(opts) {
        Ok(w) => Some(Arc::new(Mutex::new(w))),
        Err(e) => {
            tracing::warn!("rlog writer open failed for {}: {e}", path.display());
            None
        }
    }
}

/// One database: store, plan cache, optional persistence path.
pub struct DatabaseEntry {
    pub store: Arc<MvccStore>,
    pub plan_cache: Arc<PlanCache>,
    /// The `.tvdb` file; a rename moves it.
    db_path: RwLock<Option<PathBuf>>,
    pub read_only: bool,
    /// Studio sidecar (saved queries, styles) of a database without a file; for one with a file,
    /// its lock serializes edits of the `.studio.json`.
    pub sidecar: RwLock<serde_json::Value>,
    /// Modified since the last save? Databases loaded from disk start clean; writes set it.
    pub dirty: AtomicBool,
}

impl DatabaseEntry {
    fn new(
        store: Arc<MvccStore>,
        db_path: Option<PathBuf>,
        read_only: bool,
        dirty: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            plan_cache: Arc::new(PlanCache::new()),
            db_path: RwLock::new(db_path),
            read_only,
            sidecar: RwLock::new(serde_json::Value::Null),
            dirty: AtomicBool::new(dirty),
        })
    }

    /// Marks the database modified, so the next save persists it.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// The `.tvdb` file, for a database that persists.
    pub fn db_path(&self) -> Option<PathBuf> {
        self.db_path.read_unpoisoned().clone()
    }
}

/// Named databases in one server. Queries never wait on it: connections cache their
/// `Arc<DatabaseEntry>`, and the maps' locks are only held for a lookup, insert or remove.
pub struct DatabaseRegistry {
    databases: RwLock<HashMap<String, Arc<DatabaseEntry>>>,
    aliases: RwLock<HashMap<String, String>>,
    default_db: String,
    data_dir: Option<PathBuf>,
    memory_budget: Option<usize>,
    /// The default database's metadata page, holding the user catalog; its saves write it back.
    metadata: Mutex<Vec<u8>>,
    /// Serializes changing the maps and writing database files (create, load, unload, drop,
    /// rename, aliases, users, saves), so a check and the change it guards can't interleave with
    /// another, and two writers never race on one file. Never held by queries.
    admin: Mutex<()>,
}

impl DatabaseRegistry {
    /// Registry with one named database.
    pub fn single(name: String, store: Arc<MvccStore>, db_path: Option<PathBuf>) -> Self {
        let data_dir = db_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let mut databases = HashMap::new();
        databases.insert(
            name.clone(),
            DatabaseEntry::new(store, db_path, false, true),
        );
        Self {
            databases: RwLock::new(databases),
            aliases: RwLock::new(HashMap::new()),
            default_db: name,
            data_dir,
            memory_budget: None,
            metadata: Mutex::new(Vec::new()),
            admin: Mutex::new(()),
        }
    }

    /// The default database's metadata as read from its file, so saves keep it.
    pub fn with_metadata(self, metadata: Vec<u8>) -> Self {
        Self {
            metadata: Mutex::new(metadata),
            ..self
        }
    }

    /// Opens every `*.tvdb` in a directory as a database named after the file stem (`mydb.tvdb` ->
    /// `"mydb"`). A database that can't be opened is skipped, except the default one: starting
    /// without it would save an empty database (and no users) over its file. No default database
    /// file: an empty one, rebuilt from any redo log it left.
    pub fn from_directory(
        dir: PathBuf,
        default_db: String,
        memory_budget: Option<usize>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut databases = HashMap::new();
        let mut metadata = Vec::new();

        if dir.is_dir() {
            let dir_iter = match std::fs::read_dir(&dir) {
                Ok(rd) => Some(rd),
                Err(e) => {
                    // an unreadable data directory counts as empty so the server still starts; the
                    // warning says why
                    tracing::warn!("read_dir({}) failed: {e}, treating as empty", dir.display());
                    None
                }
            };
            for entry in dir_iter.into_iter().flatten() {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "tvdb") && path.is_file() {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    info!("opening database '{}' from {}", name, path.display());

                    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let opened = traverse_core::memory::check_memory_preflight(file_size)
                        .map_err(|e| e.to_string())
                        .and_then(|()| {
                            Database::open_with_durability(&path, wal_durability_enabled())
                                .map_err(|e| e.to_string())
                        });
                    let db = match opened {
                        Ok(db) => db,
                        Err(e) if name == default_db => {
                            return Err(format!(
                                "can't open the default database '{name}' ({}): {e}",
                                path.display()
                            )
                            .into());
                        }
                        Err(e) => {
                            tracing::warn!("skipping '{name}' ({}): {e}", path.display());
                            continue;
                        }
                    };
                    // commits replayed from the log aren't in the file yet: a save must write them
                    let unsaved = db.replayed_transactions() > 0;
                    let (graph, db_path, meta, log_writer, recovery_floor) = db.into_parts();
                    if name == default_db {
                        metadata = meta;
                    }
                    info!(
                        "database '{}': {} nodes, {} edges",
                        name,
                        graph.node_count(),
                        graph.edge_count()
                    );
                    // memory breakdown, for diagnostics
                    for (component, bytes) in graph.memory_breakdown() {
                        info!(
                            "  {}: {:.2} GB ({} bytes)",
                            component,
                            bytes as f64 / 1_073_741_824.0,
                            bytes
                        );
                    }
                    // keep the rlog writer so commits stay durable
                    let store = Arc::new(install_writer_with_floor(
                        MvccStore::with_budget(graph, memory_budget),
                        log_writer,
                        recovery_floor,
                    ));
                    databases.insert(
                        name,
                        DatabaseEntry::new(store, Some(db_path), false, unsaved),
                    );
                }
            }
        }

        if !databases.contains_key(&default_db) {
            info!(
                "no '{}' database found in {}, creating empty default",
                default_db,
                dir.display()
            );
            let db_path = dir.join(format!("{}.tvdb", default_db));
            // Database::open even without a .tvdb: it replays orphaned rlog segments (a prior run
            // that logged commits but crashed before a snapshot) and attaches the resumed writer.
            // A log it can't read stops startup rather than be skipped by the next snapshot.
            let db = Database::open_with_durability(&db_path, wal_durability_enabled()).map_err(
                |e| {
                    format!(
                        "can't recover the default database '{default_db}' from its redo log: {e}"
                    )
                },
            )?;
            let (graph, _p, meta, log_writer, recovery_floor) = db.into_parts();
            metadata = meta;
            let store = Arc::new(install_writer_with_floor(
                MvccStore::with_budget(graph, memory_budget),
                log_writer,
                recovery_floor,
            ));
            databases.insert(
                default_db.clone(),
                DatabaseEntry::new(store, Some(db_path), false, true),
            );
        }

        // purge mimalloc caches, returning deserialization temporaries to the OS
        purge_allocator();

        let aliases = Self::load_aliases(&dir);
        if !aliases.is_empty() {
            info!("loaded {} alias(es) from aliases.json", aliases.len());
        }

        Ok(Self {
            databases: RwLock::new(databases),
            aliases: RwLock::new(aliases),
            default_db,
            data_dir: Some(dir),
            memory_budget,
            metadata: Mutex::new(metadata),
            admin: Mutex::new(()),
        })
    }

    /// Database by name, aliases resolved.
    pub fn get(&self, name: &str) -> Option<Arc<DatabaseEntry>> {
        let resolved = self.resolve_alias(name);
        self.databases.read_unpoisoned().get(&resolved).cloned()
    }

    /// Default database name.
    pub fn default_name(&self) -> &str {
        &self.default_db
    }

    /// Default database entry.
    pub fn default_entry(&self) -> Arc<DatabaseEntry> {
        self.databases
            .read_unpoisoned()
            .get(&self.default_db)
            .cloned()
            .expect("the default database is never removed")
    }

    /// The default database's metadata: the user catalog as last saved.
    pub fn metadata(&self) -> Vec<u8> {
        self.metadata.lock_unpoisoned().clone()
    }

    /// `Err` unless `name` is free: no database or alias by that name. Under the admin lock.
    fn check_free(&self, name: &str) -> Result<(), String> {
        if self.aliases.read_unpoisoned().contains_key(name) {
            return Err(format!("'{}' is already an alias", name));
        }
        if self.databases.read_unpoisoned().contains_key(name) {
            return Err(format!("database '{}' already exists", name));
        }
        Ok(())
    }

    /// `Err` unless `name` may leave the registry (unload, drop, rename): not the default
    /// database, not an alias target. Under the admin lock.
    fn check_movable(&self, name: &str, verb: &str) -> Result<(), String> {
        if name == self.default_db {
            return Err(format!("cannot {verb} the default database '{name}'"));
        }
        let refs = self.aliases_targeting(name);
        if !refs.is_empty() {
            return Err(format!(
                "cannot {verb} database '{name}': referenced by alias(es): {}",
                refs.join(", "),
            ));
        }
        Ok(())
    }

    /// `Err` if a database already holds the file at `path`: two stores on one file would both
    /// log and save over each other. Under the admin lock.
    fn check_not_open(&self, path: &Path) -> Result<(), String> {
        let Ok(wanted) = path.canonicalize() else {
            return Ok(());
        };
        let dbs = self.databases.read_unpoisoned();
        for (name, entry) in dbs.iter() {
            if let Some(p) = entry.db_path()
                && p.canonicalize().is_ok_and(|p| p == wanted)
            {
                return Err(format!(
                    "{} is already open as database '{name}'",
                    file_label(path)
                ));
            }
        }
        Ok(())
    }

    /// Creates an empty database; `Err` if the name is taken, or a `.tvdb` by that name sits in
    /// the data directory unopened (an unloaded database, or one that failed to open), which this
    /// won't overwrite. Saved right away when there's a data directory.
    pub fn create_database(&self, name: &str) -> Result<Arc<DatabaseEntry>, String> {
        let _admin = self.admin.lock_unpoisoned();
        self.check_free(name)?;

        let db_path = self
            .data_dir
            .as_ref()
            .map(|d| d.join(format!("{}.tvdb", name)));
        if let Some(path) = &db_path {
            if path.exists() {
                return Err(format!(
                    "{} already exists: load it, or remove the file first",
                    file_label(path)
                ));
            }
            // redo-log segments without their .tvdb are a dropped database's: a new database by
            // that name mustn't replay them
            rlog::segments_of(path)
                .and_then(|s| rlog::remove_segments(&s))
                .map_err(|e| {
                    format!(
                        "couldn't clear an old redo log for {}: {e}",
                        file_label(path)
                    )
                })?;
        }
        // WAL mode only: a fresh rlog writer, so the new database's commits are durable like loaded
        // ones
        let writer = if wal_durability_enabled() {
            db_path.as_ref().and_then(|p| open_writer_for(p))
        } else {
            None
        };
        let store = Arc::new(install_writer(
            MvccStore::with_budget(Graph::new(), self.memory_budget),
            writer,
        ));

        // persist now so it survives a crash/kill
        if let Some(path) = &db_path {
            save_store(path, &store, &[])
                .map_err(|e| format!("failed to persist new database: {}", e))?;
            info!("persisted new database '{}' to {}", name, path.display());
        }

        let entry = DatabaseEntry::new(store, db_path, false, false);
        self.databases
            .write_unpoisoned()
            .insert(name.to_string(), Arc::clone(&entry));
        info!("created database '{}'", name);
        Ok(entry)
    }

    /// Unloads a database from memory, saving it first. Unlike `drop_database` the `.tvdb` stays;
    /// `load_database` brings it back. Sessions still holding it can't commit any more.
    pub fn unload_database(&self, name: &str, current_db: &str) -> Result<(), String> {
        if name == current_db {
            return Err(format!(
                "cannot unload the currently active database '{}'",
                name
            ));
        }
        let _admin = self.admin.lock_unpoisoned();
        self.check_movable(name, "unload")?;
        let entry = self
            .databases
            .write_unpoisoned()
            .remove(name)
            .ok_or_else(|| format!("database '{}' does not exist", name))?;

        // the save and the close under one pause: every commit is in the file, or fails
        let pause = entry.store.pause_commits();
        if !entry.read_only
            && let Some(path) = entry.db_path()
        {
            let saved = pause
                .checkpoint()
                .and_then(|checkpoint| install_checkpoint(&path, &entry.store, &checkpoint, &[]));
            if let Err(e) = saved {
                drop(pause);
                self.databases
                    .write_unpoisoned()
                    .insert(name.to_string(), entry);
                return Err(format!("failed to persist before unload: {}", e));
            }
            info!("persisted database '{}' to {}", name, path.display());
        }
        pause.close();

        info!("unloaded database '{}'", name);
        free_database_entry(entry);
        Ok(())
    }

    /// Drops a database and deletes its files: the `.tvdb`, its Studio sidecar and its redo log.
    /// `Err` if it doesn't exist, is the default or the caller's current database, or an alias
    /// points at it. Sessions still holding it can't commit any more.
    pub fn drop_database(&self, name: &str, current_db: &str) -> Result<(), String> {
        if name == current_db {
            return Err(format!(
                "cannot drop the currently active database '{}'",
                name
            ));
        }
        let _admin = self.admin.lock_unpoisoned();
        self.check_movable(name, "drop")?;
        let entry = self
            .databases
            .write_unpoisoned()
            .remove(name)
            .ok_or_else(|| format!("database '{}' does not exist", name))?;
        // waits for a write in progress; sessions still holding it can't commit from here on, so
        // its log doesn't grow while it's deleted
        entry.store.pause_commits().close();

        if let Some(path) = entry.db_path() {
            let remove = |p: &Path| match std::fs::remove_file(p) {
                Ok(()) => info!("removed {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("failed to remove {}: {e}", p.display()),
            };
            remove(&path);
            remove(&traverse_core::storage::wal::wal_path(&path));
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                remove(&path.with_file_name(format!("{}.studio.json", stem)));
            }
            if let Err(e) = rlog::segments_of(&path).and_then(|s| rlog::remove_segments(&s)) {
                tracing::warn!("failed to remove the redo log of {}: {e}", path.display());
            }
        }

        info!("dropped database '{}'", name);
        free_database_entry(entry);
        Ok(())
    }

    /// Renames a database with its files (`.tvdb`, redo log, Studio sidecar). `Err` if the old
    /// name doesn't exist, is the default database or an alias target, or the new one is taken.
    pub fn rename_database(&self, old_name: &str, new_name: &str) -> Result<(), String> {
        if old_name == new_name {
            return Ok(());
        }
        if new_name.is_empty() {
            return Err("database name cannot be empty".into());
        }
        let _admin = self.admin.lock_unpoisoned();
        self.check_movable(old_name, "rename")?;
        self.check_free(new_name)?;
        let entry = self
            .databases
            .read_unpoisoned()
            .get(old_name)
            .cloned()
            .ok_or_else(|| format!("database '{}' does not exist", old_name))?;

        if let Some(old_path) = entry.db_path() {
            let new_path = old_path.with_file_name(format!("{}.tvdb", new_name));
            if new_path.exists() {
                return Err(format!("{} already exists", file_label(&new_path)));
            }
            move_database_files(&entry.store, &old_path, &new_path)?;
            info!("moved {} -> {}", old_path.display(), new_path.display());
            if let Some(stem) = old_path.file_stem().and_then(|s| s.to_str()) {
                let old_studio = old_path.with_file_name(format!("{}.studio.json", stem));
                let new_studio = old_path.with_file_name(format!("{}.studio.json", new_name));
                if old_studio.exists()
                    && let Err(e) = std::fs::rename(&old_studio, &new_studio)
                {
                    tracing::warn!("couldn't move {}: {e}", old_studio.display());
                }
            }
            *entry.db_path.write_unpoisoned() = Some(new_path);
        }

        let mut dbs = self.databases.write_unpoisoned();
        dbs.remove(old_name);
        dbs.insert(new_name.to_string(), entry);
        drop(dbs);
        info!("renamed database '{}' -> '{}'", old_name, new_name);
        Ok(())
    }

    /// Every database with its default flag, read-only flag and aliases.
    pub fn list_databases(&self) -> Vec<(String, bool, bool, Vec<String>)> {
        let dbs: Vec<(String, bool)> = self
            .databases
            .read_unpoisoned()
            .iter()
            .map(|(name, entry)| (name.clone(), entry.read_only))
            .collect();
        let aliases = self.aliases.read_unpoisoned();
        let mut list: Vec<(String, bool, bool, Vec<String>)> = dbs
            .into_iter()
            .map(|(name, read_only)| {
                let mut db_aliases: Vec<String> = aliases
                    .iter()
                    .filter(|(_, target)| **target == name)
                    .map(|(alias, _)| alias.clone())
                    .collect();
                db_aliases.sort();
                let is_default = name == self.default_db;
                (name, is_default, read_only, db_aliases)
            })
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    /// Every database with its entry, sorted by name.
    fn entries(&self) -> Vec<(String, Arc<DatabaseEntry>)> {
        let mut entries: Vec<(String, Arc<DatabaseEntry>)> = self
            .databases
            .read_unpoisoned()
            .iter()
            .map(|(name, entry)| (name.clone(), Arc::clone(entry)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Per-database summary: (name, node_count, edge_count, is_default).
    pub fn database_summary(&self) -> Vec<(String, usize, usize, bool)> {
        self.entries()
            .into_iter()
            .filter_map(|(name, entry)| {
                let tx = entry.store.begin_read().ok()?;
                let nodes = tx.node_count();
                let edges = tx.edge_count();
                let is_default = name == self.default_db;
                Some((name, nodes, edges, is_default))
            })
            .collect()
    }

    // -- aliases ------------------------------------------------

    /// Alias target if `name` is an alias, else `name`.
    pub fn resolve_alias(&self, name: &str) -> String {
        self.aliases
            .read_unpoisoned()
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// Creates an alias for an existing database.
    pub fn create_alias(&self, alias: &str, target: &str) -> Result<(), String> {
        let _admin = self.admin.lock_unpoisoned();
        {
            let dbs = self.databases.read_unpoisoned();
            if dbs.contains_key(alias) {
                return Err(format!("'{}' is already a database name", alias));
            }
            if !dbs.contains_key(target) {
                return Err(format!("target database '{}' does not exist", target));
            }
        }
        let mut aliases = self.aliases.read_unpoisoned().clone();
        if aliases.contains_key(alias) {
            return Err(format!("alias '{}' already exists", alias));
        }
        aliases.insert(alias.to_string(), target.to_string());
        self.replace_aliases(aliases)?;
        info!("created alias '{}' -> '{}'", alias, target);
        Ok(())
    }

    /// Removes an alias.
    pub fn drop_alias(&self, alias: &str) -> Result<(), String> {
        let _admin = self.admin.lock_unpoisoned();
        let mut aliases = self.aliases.read_unpoisoned().clone();
        if aliases.remove(alias).is_none() {
            return Err(format!("alias '{}' does not exist", alias));
        }
        self.replace_aliases(aliases)?;
        info!("dropped alias '{}'", alias);
        Ok(())
    }

    /// Retargets an alias atomically.
    pub fn alter_alias(&self, alias: &str, new_target: &str) -> Result<(), String> {
        let _admin = self.admin.lock_unpoisoned();
        if !self.databases.read_unpoisoned().contains_key(new_target) {
            return Err(format!("target database '{}' does not exist", new_target));
        }
        let mut aliases = self.aliases.read_unpoisoned().clone();
        if !aliases.contains_key(alias) {
            return Err(format!("alias '{}' does not exist", alias));
        }
        aliases.insert(alias.to_string(), new_target.to_string());
        self.replace_aliases(aliases)?;
        info!("altered alias '{}' -> '{}'", alias, new_target);
        Ok(())
    }

    /// Sorted `(alias, target)` pairs.
    pub fn list_aliases(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = self
            .aliases
            .read_unpoisoned()
            .iter()
            .map(|(a, t)| (a.clone(), t.clone()))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    /// Aliases pointing at `db_name`.
    pub fn aliases_targeting(&self, db_name: &str) -> Vec<String> {
        let mut result: Vec<String> = self
            .aliases
            .read_unpoisoned()
            .iter()
            .filter(|(_, target)| *target == db_name)
            .map(|(alias, _)| alias.clone())
            .collect();
        result.sort();
        result
    }

    /// Saves `aliases` to `aliases.json` in the data directory (crash-safe), then makes them the
    /// live set: an alias change that can't be saved isn't made. Under the admin lock.
    fn replace_aliases(&self, aliases: HashMap<String, String>) -> Result<(), String> {
        if let Some(dir) = &self.data_dir {
            let path = dir.join("aliases.json");
            let saved = if aliases.is_empty() {
                match std::fs::remove_file(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    other => other,
                }
            } else {
                let sorted: std::collections::BTreeMap<&str, &str> = aliases
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let json = serde_json::to_vec_pretty(&sorted).map_err(std::io::Error::other);
                json.and_then(|json| traverse_core::storage::write_durable(&path, &json))
            };
            saved.map_err(|e| format!("couldn't save aliases.json: {e}"))?;
        }
        *self.aliases.write_unpoisoned() = aliases;
        Ok(())
    }

    /// Aliases from `aliases.json` in `dir`.
    fn load_aliases(dir: &Path) -> HashMap<String, String> {
        let path = dir.join("aliases.json");
        if !path.exists() {
            return HashMap::new();
        }
        match std::fs::read_to_string(&path) {
            Ok(json) => match serde_json::from_str::<HashMap<String, String>>(&json) {
                Ok(aliases) => aliases,
                Err(e) => {
                    tracing::warn!("failed to parse aliases.json: {}", e);
                    HashMap::new()
                }
            },
            Err(e) => {
                tracing::warn!("failed to read aliases.json: {}", e);
                HashMap::new()
            }
        }
    }

    /// Data directory, if configured.
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    /// Loads a `.tvdb` from the data directory as a read-only database. `file` is a relative
    /// filename (e.g. `"snap.tvdb"`); absolute paths and `..` are rejected.
    pub fn load_database(&self, name: &str, file: &str) -> Result<Arc<DatabaseEntry>, String> {
        let data_dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| "no data directory configured".to_string())?;

        // path safety: no absolute paths, no `..`
        let file_path = Path::new(file);
        if file_path.is_absolute() {
            return Err("absolute paths are not allowed".into());
        }
        for component in file_path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err("path traversal ('..') is not allowed".into());
            }
        }

        let resolved = data_dir.join(file);
        // canonicalize against symlink escapes
        {
            let canon_dir = data_dir
                .canonicalize()
                .map_err(|e| format!("cannot resolve data directory: {}", e))?;
            let canon_file = resolved
                .canonicalize()
                .map_err(|e| format!("cannot resolve file '{}': {}", file, e))?;
            if !canon_file.starts_with(&canon_dir) {
                return Err("file path escapes data directory".into());
            }
        }

        // held through the open: another open of the same file would attach a second log writer
        let _admin = self.admin.lock_unpoisoned();
        self.check_free(name)?;
        self.check_not_open(&resolved)?;

        info!("loading database '{}' from {}", name, resolved.display());

        let file_size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        traverse_core::memory::check_memory_preflight(file_size)
            .map_err(|e| format!("failed to open '{}': {}", file, e))?;

        let db = Database::open_with_durability(&resolved, wal_durability_enabled())
            .map_err(|e| format!("failed to open '{}': {}", file, e))?;

        let (graph, db_path, _meta, log_writer, recovery_floor) = db.into_parts();
        info!(
            "loaded database '{}': {} nodes, {} edges (read-only)",
            name,
            graph.node_count(),
            graph.edge_count()
        );
        let store = Arc::new(install_writer_with_floor(
            MvccStore::with_budget(graph, self.memory_budget),
            log_writer,
            recovery_floor,
        ));
        let entry = DatabaseEntry::new(store, Some(db_path), true, false);
        self.databases
            .write_unpoisoned()
            .insert(name.to_string(), Arc::clone(&entry));

        // purge mimalloc caches, returning deserialization temporaries to the OS
        purge_allocator();

        Ok(entry)
    }

    /// Loads a `.tvdb` from any path into memory only: for servers without a data directory. It
    /// has no `db_path`, isn't saved, and doesn't log its commits next to the source file.
    pub fn load_database_from_file(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<Arc<DatabaseEntry>, String> {
        let _admin = self.admin.lock_unpoisoned();
        self.check_free(name)?;
        self.check_not_open(path)?;

        info!(
            "loading database '{}' from {} (in-memory only)",
            name,
            path.display()
        );

        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        traverse_core::memory::check_memory_preflight(file_size)
            .map_err(|e| format!("failed to open '{}': {}", file_label(path), e))?;

        let db = Database::open_with_durability(path, false)
            .map_err(|e| format!("failed to open '{}': {}", file_label(path), e))?;

        let (graph, _db_path, _meta, _log_writer, _recovery_floor) = db.into_parts();
        info!(
            "loaded database '{}': {} nodes, {} edges (in-memory)",
            name,
            graph.node_count(),
            graph.edge_count()
        );
        let store = Arc::new(MvccStore::with_budget(graph, self.memory_budget));
        let entry = DatabaseEntry::new(store, None, false, false);
        self.databases
            .write_unpoisoned()
            .insert(name.to_string(), Arc::clone(&entry));

        // purge mimalloc caches, returning deserialization temporaries to the OS
        purge_allocator();

        Ok(entry)
    }

    /// Saves every modified database that has a file, the default one with the user catalog. A
    /// failed save doesn't stop the others; the error lists every failure.
    pub fn persist_all(&self) -> Result<(), Box<dyn std::error::Error>> {
        let _admin = self.admin.lock_unpoisoned();
        let mut failures = Vec::new();
        for (name, entry) in self.entries() {
            if let Err(e) = self.save_if_dirty(&name, &entry) {
                tracing::error!("saving database '{name}' failed: {e}");
                failures.push(format!("'{name}': {e}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("couldn't save {}", failures.join("; ")).into())
        }
    }

    /// Saves one database (aliases resolved) if it's modified and has a file: before handing its
    /// file out.
    pub fn persist(&self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let _admin = self.admin.lock_unpoisoned();
        self.persist_locked(name)
    }

    /// A file of the data directory to stream out as of now: `database`, when given and loaded,
    /// saved first; then the file opened and its header and length taken, all under the admin
    /// lock. After that a database file only gets pages appended past that length and its header
    /// rewritten (user changes, `Database::save_metadata`), or is replaced whole (saves rename a
    /// new file over it; the open handle keeps the old one). So the returned head followed by the
    /// file up to the length is a consistent copy.
    pub fn open_for_download(
        &self,
        database: Option<&str>,
        path: &Path,
    ) -> Result<FileSnapshot, String> {
        use std::io::Read as _;
        let _admin = self.admin.lock_unpoisoned();
        if let Some(name) = database
            && self.get(name).is_some()
        {
            self.persist_locked(name)
                .map_err(|e| format!("couldn't save '{name}' before download: {e}"))?;
        }
        let mut file =
            std::fs::File::open(path).map_err(|e| format!("failed to open file: {e}"))?;
        let len = file
            .metadata()
            .map_err(|e| format!("failed to read file metadata: {e}"))?
            .len();
        let mut head = vec![0u8; len.min(traverse_core::storage::HEADER_SIZE as u64) as usize];
        file.read_exact(&mut head)
            .map_err(|e| format!("failed to read file: {e}"))?;
        Ok(FileSnapshot { file, head, len })
    }

    /// [`persist`](Self::persist) under the admin lock.
    fn persist_locked(&self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let resolved = self.resolve_alias(name);
        let entry = self
            .databases
            .read_unpoisoned()
            .get(&resolved)
            .cloned()
            .ok_or_else(|| format!("database '{name}' does not exist"))?;
        self.save_if_dirty(&resolved, &entry)?;
        Ok(())
    }

    /// Saves `entry` if it's modified and persists. The dirty flag is cleared before the snapshot,
    /// so a commit landing during the save marks it again; a failed save leaves it dirty. Under
    /// the admin lock.
    fn save_if_dirty(&self, name: &str, entry: &DatabaseEntry) -> traverse_core::error::Result<()> {
        // read-only databases are loaded from files that aren't ours to rewrite
        if entry.read_only {
            return Ok(());
        }
        let Some(path) = entry.db_path() else {
            return Ok(());
        };
        if !entry.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        info!("saving database '{}' to {}", name, path.display());
        let metadata = if name == self.default_db {
            self.metadata()
        } else {
            Vec::new()
        };
        match save_store(&path, &entry.store, &metadata) {
            Ok((nodes, edges)) => {
                info!("saved database '{name}': {nodes} nodes, {edges} edges");
                Ok(())
            }
            Err(e) => {
                entry.dirty.store(true, Ordering::Release);
                Err(e)
            }
        }
    }

    /// Applies a user change, saving the catalog into the default database's file before anyone
    /// sees it: an acknowledged change survives a crash, and one that can't be saved isn't made.
    /// `users` is the live catalog; every change to it goes through here.
    pub fn change_users(
        &self,
        users: &RwLock<AuthCatalog>,
        change: &UserChange,
    ) -> Result<(), UserError> {
        let _admin = self.admin.lock_unpoisoned();
        let mut next = users.read_unpoisoned().clone();
        next.apply(change)?;
        let bytes = next.serialize();
        self.write_metadata(&bytes).map_err(UserError::Save)?;
        *users.write_unpoisoned() = next;
        *self.metadata.lock_unpoisoned() = bytes;
        Ok(())
    }

    /// Writes the default database's metadata page; a database never saved yet is saved whole.
    /// Nothing to write for a database without a file. Under the admin lock.
    fn write_metadata(&self, metadata: &[u8]) -> Result<(), String> {
        let entry = self.default_entry();
        let Some(path) = entry.db_path() else {
            return Ok(());
        };
        if path.exists() {
            Database::save_metadata(&path, metadata).map_err(|e| e.to_string())
        } else {
            save_store(&path, &entry.store, metadata)
                .map(drop)
                .map_err(|e| e.to_string())
        }
    }
}

/// A file as [`DatabaseRegistry::open_for_download`] found it: its first bytes (the header of a
/// `.tvdb`) read already, the handle positioned after them, and its length then.
pub struct FileSnapshot {
    /// The open file, positioned after `head`.
    pub file: std::fs::File,
    /// The file's first bytes.
    pub head: Vec<u8>,
    /// The file's length when opened: stream up to it.
    pub len: u64,
}

/// Drains mimalloc caches on all threads and returns freed pages to the OS. Graph data is allocated
/// on rayon workers during deserialization but freed on HTTP/Bolt threads (or when the last `Arc`
/// drops), and mimalloc keeps cross-thread frees in per-thread delayed-free lists:
/// `mi_collect(true)` here reclaims this thread's heap, `rayon::broadcast` drains every worker's
/// list so their segments can be decommitted.
#[cfg(feature = "native-alloc")]
pub fn purge_allocator() {
    collect_allocator_caches();
    info!("purged allocator caches (all threads)");
}

/// No-op without the native allocator (no cross-thread cache to drain).
#[cfg(not(feature = "native-alloc"))]
pub fn purge_allocator() {}

/// [`purge_allocator`] without the log line, for the per-query path after large results.
#[cfg(feature = "native-alloc")]
pub fn collect_allocator_caches() {
    // SAFETY: mi_collect has no preconditions; it only walks the calling thread's heap
    unsafe { libmimalloc_sys::mi_collect(true) };
    // SAFETY: as above, once on each rayon worker
    rayon::broadcast(|_| unsafe { libmimalloc_sys::mi_collect(true) });
}

/// No-op without the native allocator.
#[cfg(not(feature = "native-alloc"))]
pub fn collect_allocator_caches() {}

/// Frees a database entry and returns its memory to the OS.
fn free_database_entry(entry: Arc<DatabaseEntry>) {
    drop(entry);
    purge_allocator();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use traverse_core::graph::property::PropertyValue;
    use traverse_core::storage::rlog::{LogWriter, MIN_SEGMENT_SIZE, WriterOptions};
    use traverse_core::tx::GraphView;

    fn segments(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".rlog.")
            })
            .count()
    }

    /// A checkpoint drops the log segments its snapshot covers; reopening rebuilds the snapshot
    /// plus every later commit from the remaining log.
    #[test]
    fn save_truncates_covered_segments_and_reopen_replays_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.tvdb");
        let writer = Arc::new(Mutex::new(
            LogWriter::open(WriterOptions {
                segment_size: MIN_SEGMENT_SIZE,
                stem: "t.tvdb".to_string(),
                dir: dir.path().to_path_buf(),
            })
            .unwrap(),
        ));
        let store = MvccStore::new(Graph::new()).with_log_writer(Arc::clone(&writer));
        // log-backed but not synced per commit: this is about checkpoint truncation, not latency
        store.set_sync_on_commit(false);
        let commit = |i: i64| {
            let mut w = store.begin_write();
            let mut props = BTreeMap::new();
            props.insert("i".to_string(), PropertyValue::Int(i));
            GraphView::create_node(&mut w, &["N"], props).unwrap();
            w.commit().unwrap();
        };
        for i in 0..2000 {
            commit(i);
        }
        let before = segments(dir.path());
        assert!(before > 2, "the workload should span several segments");
        let (nodes, _) = save_store(&path, &store, &[]).unwrap();
        assert_eq!(nodes, 2000);
        assert_eq!(segments(dir.path()), 1, "only the active segment is left");
        for i in 2000..2010 {
            commit(i);
        }
        drop(store);
        drop(writer);

        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert_eq!(g.node_count(), 2010, "snapshot + the log tail after it");
    }

    /// Commits made while a checkpoint is written land in the log past its position: the file
    /// holds the store as of the checkpoint, and reopening replays the rest, each commit once.
    #[test]
    fn commits_during_a_checkpoint_replay_after_it_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.tvdb");
        let writer = Arc::new(Mutex::new(
            LogWriter::open(WriterOptions {
                segment_size: MIN_SEGMENT_SIZE,
                stem: "t.tvdb".to_string(),
                dir: dir.path().to_path_buf(),
            })
            .unwrap(),
        ));
        let store = MvccStore::new(Graph::new()).with_log_writer(Arc::clone(&writer));
        store.set_sync_on_commit(false);
        let commit = |i: i64| {
            let mut w = store.begin_write();
            let mut props = BTreeMap::new();
            props.insert("i".to_string(), PropertyValue::Int(i));
            GraphView::create_node(&mut w, &["N"], props).unwrap();
            w.commit().unwrap();
        };
        for i in 0..100 {
            commit(i);
        }
        let checkpoint = store.checkpoint().unwrap();
        for i in 100..150 {
            commit(i);
        }
        let (nodes, _) = install_checkpoint(&path, &store, &checkpoint, &[]).unwrap();
        assert_eq!(nodes, 100, "the file holds the store as of the checkpoint");
        drop(checkpoint);
        for i in 150..160 {
            commit(i);
        }
        drop(store);
        drop(writer);

        let db = Database::open(&path).unwrap();
        let g = db.graph();
        let key = traverse_core::graph::types::PropertyKeyId(g.strings().get("i").unwrap());
        let mut seen: Vec<i64> = g
            .nodes_vec()
            .into_iter()
            .map(|id| match g.get_node(id).unwrap().properties.get(key) {
                Some(PropertyValue::Int(i)) => i,
                other => panic!("{other:?}"),
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..160).collect::<Vec<_>>());
    }

    fn reopened_metadata(path: &Path) -> Vec<u8> {
        Database::open(path).unwrap().into_parts().2
    }

    fn users_with(names: &[&str]) -> AuthCatalog {
        let mut users = AuthCatalog::new();
        for name in names {
            users
                .apply(&UserChange::Add {
                    username: name.to_string(),
                    hash: format!("hash-of-{name}"),
                    role: crate::auth::Role::Reader,
                })
                .unwrap();
        }
        users
    }

    fn add(name: &str) -> UserChange {
        UserChange::Add {
            username: name.into(),
            hash: "h".into(),
            role: crate::auth::Role::Admin,
        }
    }

    fn node(store: &MvccStore) {
        let mut w = store.begin_write();
        GraphView::create_node(&mut w, &["N"], BTreeMap::new()).unwrap();
        w.commit().unwrap();
    }

    /// Commits the log replays at startup aren't in the file: the database opens unsaved, so a
    /// download or a checkpoint saves them first.
    #[test]
    fn commits_replayed_at_startup_count_as_unsaved() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("other.tvdb");
        {
            let writer = open_writer_for(&path).unwrap();
            let store = MvccStore::new(Graph::new()).with_log_writer(writer);
            save_store(&path, &store, &[]).unwrap();
            node(&store);
            // gone without a save: the commit is only in the log
        }
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        let entry = registry.get("other").unwrap();
        assert!(entry.dirty.load(Ordering::Acquire));
        assert_eq!(entry.store.begin_read().unwrap().node_count(), 1);
        registry.persist("other").unwrap();
        drop(registry);
        assert_eq!(Database::open(&path).unwrap().graph().node_count(), 1);
    }

    /// A download taken while users change is the file as of its start: a user change appends
    /// pages and rewrites the header, and the download streams the header it read first and the
    /// file up to its length then.
    #[test]
    fn a_download_is_the_file_as_of_its_start() {
        use std::io::Read as _;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("traverse.tvdb");
        let store = Arc::new(MvccStore::new(Graph::new()));
        node(&store);
        save_store(&path, &store, b"before").unwrap();
        let registry =
            DatabaseRegistry::single("traverse".into(), Arc::clone(&store), Some(path.clone()))
                .with_metadata(b"before".to_vec());
        let snapshot = registry.open_for_download(Some("traverse"), &path).unwrap();

        // a user list larger than a page lands mid-download
        let users: Vec<u8> = (0..40_000u64)
            .map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8)
            .collect();
        Database::save_metadata(&path, &users).unwrap();

        let mut copy = snapshot.head.clone();
        (&snapshot.file)
            .take(snapshot.len - snapshot.head.len() as u64)
            .read_to_end(&mut copy)
            .unwrap();
        let copy_path = dir.path().join("copy.tvdb");
        std::fs::write(&copy_path, &copy).unwrap();
        assert_eq!(reopened_metadata(&copy_path), b"before");
        assert_eq!(Database::open(&copy_path).unwrap().graph().node_count(), 1);
        assert_eq!(reopened_metadata(&path), users);
    }

    /// The user catalog lives in the default database's metadata page; saves of that database
    /// (checkpoints, shutdown) keep it.
    #[test]
    fn saves_keep_the_user_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("traverse.tvdb");
        let store = Arc::new(MvccStore::new(Graph::new()));
        save_store(&path, &store, &[]).unwrap();
        Database::save_metadata(&path, b"users").unwrap();
        let registry =
            DatabaseRegistry::single("traverse".into(), Arc::clone(&store), Some(path.clone()))
                .with_metadata(reopened_metadata(&path));
        node(&store);
        registry.persist_all().unwrap();
        assert_eq!(reopened_metadata(&path), b"users");
        assert_eq!(Database::open(&path).unwrap().graph().node_count(), 1);
    }

    /// A user change is on disk when it returns, whether or not the database has been saved yet.
    #[test]
    fn user_changes_are_saved_as_they_happen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("traverse.tvdb");
        let registry = DatabaseRegistry::single(
            "traverse".into(),
            Arc::new(MvccStore::new(Graph::new())),
            Some(path.clone()),
        );
        let users = RwLock::new(AuthCatalog::new());
        // no file yet: the database is saved whole, with the catalog
        registry.change_users(&users, &add("alice")).unwrap();
        let saved = AuthCatalog::deserialize(&reopened_metadata(&path)).unwrap();
        assert_eq!(saved.list_users().len(), 1);
        // then just the metadata page
        registry.change_users(&users, &add("bob")).unwrap();
        let saved = AuthCatalog::deserialize(&reopened_metadata(&path)).unwrap();
        assert_eq!(saved.list_users().len(), 2);
        assert_eq!(registry.metadata(), saved.serialize());
        // a rejected change touches nothing
        assert_eq!(
            registry.change_users(&users, &add("bob")),
            Err(UserError::Exists("bob".into()))
        );
        assert_eq!(users.read().unwrap().list_users().len(), 2);
    }

    /// A change that can't be saved isn't made.
    #[test]
    fn a_user_change_that_cannot_be_saved_is_not_made() {
        let dir = tempfile::TempDir::new().unwrap();
        // the database "file" is a directory: nothing can be written there
        let path = dir.path().join("traverse.tvdb");
        std::fs::create_dir(&path).unwrap();
        let registry = DatabaseRegistry::single(
            "traverse".into(),
            Arc::new(MvccStore::new(Graph::new())),
            Some(path),
        );
        let users = RwLock::new(users_with(&["alice"]));
        let before = users.read().unwrap().generation();
        assert!(matches!(
            registry.change_users(&users, &add("bob")),
            Err(UserError::Save(_))
        ));
        let users = users.read().unwrap();
        assert_eq!(users.list_users().len(), 1);
        assert_eq!(users.generation(), before);
    }

    /// Directory mode reads the user catalog from the default database's file.
    #[test]
    fn directory_mode_loads_the_user_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("traverse.tvdb");
        Database::save(&path, &Graph::new(), &users_with(&["alice"]).serialize()).unwrap();
        Database::save(&dir.path().join("other.tvdb"), &Graph::new(), &[]).unwrap();
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        let users = AuthCatalog::deserialize(&registry.metadata()).unwrap();
        assert_eq!(users.list_users()[0].0, "alice");
    }

    /// A default database file that can't be opened stops startup instead of being replaced by an
    /// empty one at the next save.
    #[test]
    fn an_unreadable_default_database_stops_startup() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("traverse.tvdb"), b"not a database").unwrap();
        std::fs::write(dir.path().join("broken.tvdb"), b"not a database").unwrap();
        let err =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .err()
                .expect("startup fails");
        assert!(
            err.to_string().contains("default database 'traverse'"),
            "{err}"
        );
        // any other database is skipped
        std::fs::remove_file(dir.path().join("traverse.tvdb")).unwrap();
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        assert!(registry.get("broken").is_none());
        // and its file isn't overwritten by a new database of that name
        assert!(registry.create_database("broken").is_err());
        assert_eq!(
            std::fs::read(dir.path().join("broken.tvdb")).unwrap(),
            b"not a database"
        );
    }

    #[test]
    fn the_default_database_stays() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        registry.create_database("other").unwrap();
        for result in [
            registry.drop_database("traverse", "other"),
            registry.unload_database("traverse", "other"),
            registry.rename_database("traverse", "renamed"),
        ] {
            assert!(result.unwrap_err().contains("default database"));
        }
        registry.default_entry();
    }

    /// Commits made before a rename survive a crash after it (WAL mode): the log moves with the
    /// database, and the old name leaves nothing behind.
    #[test]
    fn a_renamed_database_keeps_its_logged_commits() {
        let dir = tempfile::TempDir::new().unwrap();
        let old_path = dir.path().join("old.tvdb");
        let new_path = dir.path().join("new.tvdb");
        let store =
            MvccStore::new(Graph::new()).with_log_writer(open_writer_for(&old_path).unwrap());
        save_store(&old_path, &store, &[]).unwrap();
        node(&store);
        move_database_files(&store, &old_path, &new_path).unwrap();
        node(&store);
        // a crash: nothing saved after the move
        drop(store);
        assert!(!old_path.exists());
        assert!(rlog::segments_of(&old_path).unwrap().is_empty());
        assert_eq!(Database::open(&new_path).unwrap().graph().node_count(), 2);
    }

    /// Without a log, a rename is a file rename.
    #[test]
    fn a_database_without_a_log_is_renamed_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        let entry = registry.create_database("old").unwrap();
        node(&entry.store);
        entry.mark_dirty();
        registry.rename_database("old", "new").unwrap();
        // sessions holding the entry keep the same database: same dirty flag, new file
        assert_eq!(entry.db_path(), Some(dir.path().join("new.tvdb")));
        assert!(Arc::ptr_eq(&registry.get("new").unwrap(), &entry));
        registry.persist_all().unwrap();
        assert!(!dir.path().join("old.tvdb").exists());
        assert_eq!(
            Database::open(dir.path().join("new.tvdb"))
                .unwrap()
                .graph()
                .node_count(),
            1
        );
    }

    /// A dropped database takes its log along, so a new database by that name starts empty, and
    /// sessions still holding the dropped one can't commit.
    #[test]
    fn a_dropped_database_leaves_no_log_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("gone.tvdb");
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        // what a WAL-mode database looks like: snapshot, then logged commits
        let store =
            Arc::new(MvccStore::new(Graph::new()).with_log_writer(open_writer_for(&path).unwrap()));
        save_store(&path, &store, &[]).unwrap();
        node(&store);
        registry.databases.write().unwrap().insert(
            "gone".into(),
            DatabaseEntry::new(Arc::clone(&store), Some(path.clone()), false, false),
        );
        registry.drop_database("gone", "traverse").unwrap();
        assert!(!path.exists());
        assert!(rlog::segments_of(&path).unwrap().is_empty());
        let mut w = store.begin_write();
        GraphView::create_node(&mut w, &["N"], BTreeMap::new()).unwrap();
        assert!(matches!(
            w.commit(),
            Err(traverse_core::error::Error::DatabaseClosed)
        ));

        registry.create_database("gone").unwrap();
        drop(store);
        assert_eq!(Database::open(&path).unwrap().graph().node_count(), 0);
    }

    #[test]
    fn aliases_are_saved_before_they_count() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        registry.create_alias("main", "traverse").unwrap();
        let reloaded =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        assert_eq!(
            reloaded.list_aliases(),
            vec![("main".to_string(), "traverse".to_string())]
        );
        registry.drop_alias("main").unwrap();
        assert!(!dir.path().join("aliases.json").exists());
    }

    /// One file is only ever open once: a second store on it would log and save over the first.
    #[test]
    fn a_file_is_loaded_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry =
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap();
        registry.create_database("a").unwrap();
        let err = registry
            .load_database("b", "a.tvdb")
            .err()
            .expect("refused");
        assert!(err.contains("already open as database 'a'"), "{err}");
    }

    /// A panic while a map lock is held doesn't take the registry down for every later request.
    #[test]
    fn the_registry_outlives_a_panic_under_its_locks() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Arc::new(
            DatabaseRegistry::from_directory(dir.path().to_path_buf(), "traverse".into(), None)
                .unwrap(),
        );
        let r = Arc::clone(&registry);
        let _ = std::thread::spawn(move || {
            let _dbs = r.databases.write().unwrap();
            let _aliases = r.aliases.write().unwrap();
            let _admin = r.admin.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(registry.databases.is_poisoned());
        registry.create_database("after").unwrap();
        registry.create_alias("a", "after").unwrap();
        assert!(registry.get("a").is_some());
        registry.persist_all().unwrap();
    }
}
