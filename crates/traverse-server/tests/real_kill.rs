//! Real-process kill test of the server's saves: a child opens a data directory as the server does,
//! commits a fixed history of multi-step transactions (records bigger than a page, deletes, an
//! index) while a second thread saves it the server's way (a checkpoint written to a new file
//! renamed over the old one, then the redo log it covers dropped), and is killed at random points
//! (`TerminateProcess` on Windows, `SIGKILL` elsewhere), over and over. Each open must hold exactly
//! a prefix of the history, at least as long as every commit or save the child reported durable.
//! Once in WAL mode, once saving snapshots only.
//!
//! Custom harness (`harness = false`): a child must turn victim before libtest parses argv.
//! `TRAVERSE_KILL_DIR` set -> run as the victim, no tests. An argument filters the tests by name;
//! `TRAVERSE_KILL_SEED` replays a run.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use traverse_core::graph::property::{IndexKey, PropertyMap, PropertyValue};
use traverse_core::graph::types::NodeId;
use traverse_core::storage::Database;
use traverse_core::tx::GraphView;
use traverse_server::registry::DatabaseRegistry;

fn main() {
    if std::env::var("TRAVERSE_KILL_DIR").is_ok() {
        return victim();
    }
    let tests: [(&str, fn()); 2] = [
        (
            "a_wal_database_survives_kills_mid_commit_and_save",
            a_wal_database_survives_kills,
        ),
        (
            "a_snapshot_database_survives_kills_mid_save",
            a_snapshot_database_survives_kills,
        ),
    ];
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let tests: Vec<_> = tests
        .into_iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|f| name.contains(f)))
        .collect();
    println!("running {} tests", tests.len());
    let mut failed = 0usize;
    for (name, test) in tests {
        print!("test {name} ... ");
        std::io::stdout().flush().unwrap();
        let start = Instant::now();
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("ok ({:.1?})", start.elapsed()),
            Err(_) => {
                println!("FAILED");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!("{failed} test(s) failed");
        std::process::exit(1);
    }
}

// -- the victim --

/// Ends the victim: what went wrong to stderr, where the parent looks.
fn die(what: &str, e: impl Debug) -> ! {
    eprintln!("victim: {what}: {e:?}");
    std::process::exit(3)
}

/// Opens the data directory `TRAVERSE_KILL_DIR` (durability from `TRAVERSE_DURABILITY`), checks
/// its `victim` database holds a prefix of the history and prints `OPENED n` (its length), then
/// carries the history on while a second thread saves, at most [`AHEAD`] commits past the last
/// save. Prints `DURABLE n` whenever the first `n` commits are sure to survive: after each commit
/// in WAL mode, and after a save.
fn victim() {
    let dir = PathBuf::from(std::env::var("TRAVERSE_KILL_DIR").unwrap());
    let wal = std::env::var("TRAVERSE_DURABILITY").is_ok_and(|d| d == "wal");
    let registry = Arc::new(
        DatabaseRegistry::from_directory(dir, "victim".into(), None)
            .unwrap_or_else(|e| die("open", e)),
    );
    let entry = registry.default_entry();
    let (n, mut keys) = {
        let tx = entry
            .store
            .begin_read()
            .unwrap_or_else(|e| die("begin_read", e));
        recovered(&tx).unwrap_or_else(|e| die("reopened", e))
    };
    println!("OPENED {n}");

    let (done, saved) = (Arc::new(AtomicI64::new(n)), Arc::new(AtomicI64::new(n)));
    {
        let (registry, done, saved) =
            (Arc::clone(&registry), Arc::clone(&done), Arc::clone(&saved));
        std::thread::spawn(move || {
            for round in 0u64.. {
                let covered = done.load(Ordering::Acquire);
                if let Err(e) = registry.persist("victim") {
                    die("save", e);
                }
                saved.store(covered, Ordering::Release);
                println!("DURABLE {covered}");
                std::thread::sleep(Duration::from_millis(round % 3));
            }
        });
    }
    for i in n..n + 100_000 {
        while i - saved.load(Ordering::Acquire) > AHEAD {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut tx = entry.store.begin_write();
        step(i, &mut tx, &mut keys).unwrap_or_else(|e| die(&format!("step {i}"), e));
        tx.commit()
            .unwrap_or_else(|e| die(&format!("commit {i}"), e));
        entry.mark_dirty();
        done.store(i + 1, Ordering::Release);
        if wal {
            println!("DURABLE {}", i + 1);
        }
    }
    std::thread::sleep(Duration::from_secs(60));
}

/// How far the victim commits past its last save: keeps the file, and each open, small.
const AHEAD: i64 = 64;

// -- the history --

/// Every this many nodes carries a record bigger than a page.
const BLOB_EVERY: i64 = 10;

fn blob(k: i64) -> PropertyValue {
    PropertyValue::String(format!("{k:08}").repeat(2_500))
}

fn node_props(k: i64, v: i64) -> PropertyMap {
    let mut props = PropertyMap::new();
    props.insert("k".into(), PropertyValue::Int(k));
    props.insert("v".into(), PropertyValue::Int(v));
    if k % BLOB_EVERY == 0 {
        props.insert("blob".into(), blob(k));
    }
    props
}

/// Commit `i` of the history, on `tx`: node `k = i`, linked to node `i - 1`; node `i / 2` updated
/// to `v = i`; every fourth commit, node `i - 7` deleted with its relationships. The first one also
/// indexes `k`. `keys` maps the live nodes' `k` to their ids.
fn step(
    i: i64,
    tx: &mut dyn GraphView,
    keys: &mut HashMap<i64, NodeId>,
) -> traverse_core::error::Result<()> {
    if i == 0 {
        tx.create_index("n_k", "N", "k")?;
    }
    let id = tx.create_node(&["N"], node_props(i, 0))?;
    if let Some(&prev) = keys.get(&(i - 1)) {
        let mut props = PropertyMap::new();
        props.insert("i".into(), PropertyValue::Int(i));
        tx.create_edge(id, prev, "NEXT", props)?;
    }
    keys.insert(i, id);
    let j = i / 2;
    if j != i
        && let Some(&node) = keys.get(&j)
    {
        tx.update_node_properties(node, node_props(j, i))?;
    }
    if i >= 7
        && i % 4 == 0
        && let Some(gone) = keys.remove(&(i - 7))
    {
        tx.delete_node_cascading(gone)?;
    }
    Ok(())
}

/// Nodes (`k -> v`) and relationships (`from k -> to k`).
#[derive(Debug, Default, PartialEq, Eq)]
struct State {
    nodes: BTreeMap<i64, i64>,
    edges: BTreeSet<(i64, i64)>,
}

/// The database after the first `n` commits of the history.
fn history(n: i64) -> State {
    let mut s = State::default();
    for i in 0..n {
        s.nodes.insert(i, 0);
        if s.nodes.contains_key(&(i - 1)) {
            s.edges.insert((i, i - 1));
        }
        let j = i / 2;
        if j != i
            && let Some(v) = s.nodes.get_mut(&j)
        {
            *v = i;
        }
        if i >= 7 && i % 4 == 0 && s.nodes.remove(&(i - 7)).is_some() {
            s.edges.remove(&(i - 6, i - 7));
            s.edges.remove(&(i - 7, i - 8));
        }
    }
    s
}

fn int(v: Option<PropertyValue>, what: &str) -> Result<i64, String> {
    match v {
        Some(PropertyValue::Int(i)) => Ok(i),
        other => Err(format!("{what}: {other:?}")),
    }
}

/// How many commits of the history `tx` sees, and the live nodes' ids by `k`; `Err` when it isn't
/// a state the history goes through.
fn recovered(tx: &dyn GraphView) -> Result<(i64, HashMap<i64, NodeId>), String> {
    let e = |e: traverse_core::error::Error| e.to_string();
    let mut state = State::default();
    let mut keys = HashMap::new();
    let mut by_id = HashMap::new();
    for id in tx.nodes_vec() {
        let k = int(tx.get_node_property(id, "k").map_err(e)?, "k")?;
        let v = int(tx.get_node_property(id, "v").map_err(e)?, "v")?;
        let big = tx.get_node_property(id, "blob").map_err(e)?;
        if big != (k % BLOB_EVERY == 0).then(|| blob(k)) {
            return Err(format!("node {k}: blob {big:.40?}"));
        }
        if state.nodes.insert(k, v).is_some() {
            return Err(format!("two nodes {k}"));
        }
        keys.insert(k, id);
        by_id.insert(id, k);
    }
    for id in tx.all_edges_vec() {
        let edge = tx.get_edge(id).map_err(e)?;
        let (Some(&from), Some(&to)) = (by_id.get(&edge.source), by_id.get(&edge.target)) else {
            return Err(format!("relationship {id:?} between unknown nodes"));
        };
        if int(tx.get_edge_property(id, "i").map_err(e)?, "i")? != from {
            return Err(format!("relationship {from} -> {to}: wrong i"));
        }
        if !state.edges.insert((from, to)) {
            return Err(format!("two relationships {from} -> {to}"));
        }
    }
    if tx.nodes_by_label("N").len() != state.nodes.len()
        || tx.edges_by_type("NEXT").len() != state.edges.len()
    {
        return Err("labels or types lost".into());
    }

    let n = state.nodes.keys().next_back().map_or(0, |k| k + 1);
    let want = history(n);
    if state != want {
        let node = (0..n).find(|k| state.nodes.get(k) != want.nodes.get(k));
        let edge = want.edges.symmetric_difference(&state.edges).next();
        return Err(format!(
            "not a prefix of the history: n={n}, first wrong node {node:?} (has {:?}, want {:?}), \
             first wrong relationship {edge:?}",
            node.and_then(|k| state.nodes.get(&k)),
            node.and_then(|k| want.nodes.get(&k)),
        ));
    }
    if n > 0 {
        if !tx.index_catalog().has_index_by_name("n_k") {
            return Err("index n_k lost".into());
        }
        for k in 0..n {
            let found = tx
                .index_lookup("N", "k", &IndexKey::Int(k))
                .unwrap_or_default();
            let want: Vec<NodeId> = keys.get(&k).copied().into_iter().collect();
            if found != want {
                return Err(format!("index n_k: {k} -> {found:?}, want {want:?}"));
            }
        }
    }
    Ok((n, keys))
}

// -- parent side --

/// The child running as the victim, its stdout lines arriving on a channel.
struct Victim {
    child: Child,
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
}

impl Victim {
    fn spawn(dir: &Path, durability: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .env("TRAVERSE_KILL_DIR", dir)
            .env("TRAVERSE_DURABILITY", durability)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            reader: Some(reader),
        }
    }

    /// The next line; `None` once the child closed its stdout (it exited). Fails the test when
    /// the child stays silent for `timeout`: a hang.
    fn next_line(&mut self, timeout: Duration) -> Option<String> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Some(line),
            Err(RecvTimeoutError::Disconnected) => None,
            Err(RecvTimeoutError::Timeout) => {
                let (_, stderr) = self.kill();
                panic!("victim silent for {timeout:?}; stderr: {stderr}")
            }
        }
    }

    /// The lines that arrive before `deadline`, or until the child exits.
    fn lines_until(&mut self, deadline: Instant) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(line) => lines.push(line),
                Err(_) => break,
            }
        }
        lines
    }

    /// Kills the child: the lines it printed that weren't read yet, and its stderr.
    fn kill(&mut self) -> (Vec<String>, String) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let rest = self.lines.try_iter().collect();
        let mut stderr = String::new();
        if let Some(mut s) = self.child.stderr.take() {
            let _ = s.read_to_string(&mut stderr);
        }
        (rest, stderr)
    }
}

impl Drop for Victim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `WORD n` -> `("WORD", n)`.
fn parse(line: &str) -> (&str, i64) {
    let (word, n) = line.split_once(' ').unwrap_or_else(|| unexpected(line));
    (word, n.parse().unwrap_or_else(|_| unexpected(line)))
}

fn unexpected(line: &str) -> ! {
    panic!("victim said {line:?}")
}

/// xorshift64*: kill points that differ run to run, replayed from the printed seed.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = std::env::var("TRAVERSE_KILL_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64
                    | 1
            });
        eprint!("[TRAVERSE_KILL_SEED={seed}] ");
        Self(seed)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

// -- tests --

fn a_wal_database_survives_kills() {
    survives_kills("wal", 40);
}

fn a_snapshot_database_survives_kills() {
    survives_kills("snapshot", 40);
}

/// Kills the victim `kills` times at random points (some while it opens), checking every open
/// against what it reported durable before; then opens the database here once more.
fn survives_kills(durability: &str, kills: u32) {
    let mut rng = Rng::new();
    let tmp = tempfile::TempDir::new().unwrap();
    // longest prefix of the history reported durable so far
    let mut durable = 0;
    let mut opens = 0;
    for kill in 0..kills {
        let mut victim = Victim::spawn(tmp.path(), durability);
        let mut lines = Vec::new();
        if rng.below(4) == 0 {
            // anywhere, the open and its recovery included
            let at = Instant::now() + Duration::from_millis(rng.below(80));
            lines = victim.lines_until(at);
        } else {
            let more = 1 + rng.below(40) as i64;
            let mut target = None;
            while let Some(line) = victim.next_line(Duration::from_secs(60)) {
                let (word, n) = parse(&line);
                let enough = match word {
                    "OPENED" => {
                        target = Some(n + more);
                        false
                    }
                    _ => target.is_some_and(|t| n >= t),
                };
                lines.push(line);
                if enough {
                    break;
                }
            }
            std::thread::sleep(Duration::from_micros(rng.below(3_000)));
        }
        let (rest, stderr) = victim.kill();
        lines.extend(rest);
        assert!(
            stderr.is_empty(),
            "kill {kill}: the victim failed: {stderr}"
        );

        let mut reported = durable;
        for line in &lines {
            match parse(line) {
                ("OPENED", n) => {
                    assert!(
                        n >= durable,
                        "kill {kill}: reopened with {n} commits, {durable} were durable"
                    );
                    opens += 1;
                }
                ("DURABLE", n) => reported = reported.max(n),
                _ => unexpected(line),
            }
        }
        durable = reported;
    }

    let path = tmp.path().join("victim.tvdb");
    let db = Database::open_with_durability(&path, durability == "wal").unwrap();
    let tx = db.begin_read().unwrap();
    let (n, _) = recovered(&tx).unwrap_or_else(|e| panic!("last open: {e}"));
    assert!(
        n >= durable,
        "last open: {n} commits, {durable} were durable"
    );
    assert!(
        !path.with_extension("tvdb.tmp").exists(),
        "a temp file was left"
    );
    eprint!("[{kills} kills, {opens} reopens, {durable} durable, {n} recovered] ");
}
