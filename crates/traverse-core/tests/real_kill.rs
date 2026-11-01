//! Real-process kill tests: a child commits, saves and compacts while the parent kills it
//! (`TerminateProcess` on Windows, `SIGKILL` elsewhere: no unwinding, nothing flushed on the way
//! out), then the parent looks at what it left.
//!
//! - Redo log: a child commits through `WriteTx -> log -> fsync`. Every commit it acknowledged must
//!   be in the segments, in order, and a new writer appends after them.
//! - Database: a child runs a fixed history of multi-step transactions (plain, optimistic and
//!   bulk-load commits, records bigger than a page, deletes, an index) while a second thread saves
//!   in place and compacts without pause. It's killed and reopened over and over, at random
//!   points: each open must hold exactly a prefix of the history, at least as long as every commit
//!   or save the child reported durable. Once with the redo log, once with saves only.
//!
//! Custom harness (`harness = false`): a child must turn victim before libtest parses argv.
//! `TRAVERSE_KILL_VICTIM` set -> run as that victim, no tests. An argument filters the tests by
//! name; `TRAVERSE_KILL_SEED` replays a run.

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    native::dispatch();
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fmt::Debug;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use traverse_core::graph::Graph;
    use traverse_core::graph::property::{IndexKey, PropertyMap, PropertyValue};
    use traverse_core::graph::types::NodeId;
    use traverse_core::storage::Database;
    use traverse_core::storage::rlog::{LogWriter, Op as RedoOp, WriterOptions, scan_segments};
    use traverse_core::tx::{GraphView, MvccStore};

    pub(super) fn dispatch() {
        match std::env::var("TRAVERSE_KILL_VICTIM").as_deref() {
            Ok("rlog") => return rlog_victim(),
            Ok("db") => return db_victim(),
            Ok(other) => panic!("unknown victim {other}"),
            Err(_) => {}
        }
        let tests: [(&str, fn()); 5] = [
            (
                "rlog_keeps_every_acknowledged_commit",
                rlog_keeps_every_acknowledged_commit,
            ),
            (
                "rlog_appends_after_a_killed_writer",
                rlog_appends_after_a_killed_writer,
            ),
            (
                "rlog_survives_kills_at_any_commit",
                rlog_survives_kills_at_any_commit,
            ),
            (
                "a_logged_database_survives_kills_mid_commit_save_and_compaction",
                a_logged_database_survives_kills,
            ),
            (
                "a_database_saving_snapshots_survives_kills_mid_save_and_compaction",
                a_database_saving_snapshots_survives_kills,
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

    // -- victims --

    fn var(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| die("missing", name))
    }

    /// Ends a victim: what went wrong to stderr, where the parent looks.
    fn die(what: &str, e: impl Debug) -> ! {
        eprintln!("victim: {what}: {e:?}");
        std::process::exit(3)
    }

    /// Commits `TRAVERSE_KILL_TOTAL` nodes with a 1 KiB payload each, printing `COMMITTED i` as
    /// each commit returns, then waits to be killed.
    fn rlog_victim() {
        let dir = PathBuf::from(var("TRAVERSE_KILL_DIR"));
        let total: i64 = var("TRAVERSE_KILL_TOTAL").parse().unwrap();
        let writer = LogWriter::open(rlog_options(&dir, &var("TRAVERSE_KILL_STEM")))
            .unwrap_or_else(|e| die("open the log", e));
        let store = MvccStore::new(Graph::new()).with_log_writer(Arc::new(Mutex::new(writer)));
        for i in 0..total {
            let mut tx = store.begin_write();
            let mut props = PropertyMap::new();
            props.insert("i".into(), PropertyValue::Int(i));
            props.insert("data".into(), PropertyValue::String("x".repeat(1024)));
            GraphView::create_node(&mut tx, &["N"], props)
                .unwrap_or_else(|e| die(&format!("create {i}"), e));
            tx.commit()
                .unwrap_or_else(|e| die(&format!("commit {i}"), e));
            println!("COMMITTED {i}");
        }
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Opens `TRAVERSE_KILL_DB` (with the redo log when `TRAVERSE_KILL_LOGGED` is 1), checks it
    /// holds a prefix of the history and prints `OPENED n` (its length), then carries the history
    /// on while a second thread saves and compacts, at most [`AHEAD`] commits past the last save.
    /// Prints `DURABLE n` whenever the first `n` commits are sure to survive: after a logged or
    /// bulk-load commit, and after a save.
    fn db_victim() {
        let path = PathBuf::from(var("TRAVERSE_KILL_DB"));
        let logged = var("TRAVERSE_KILL_LOGGED") == "1";
        let db = Arc::new(
            Database::open_with_durability(&path, logged).unwrap_or_else(|e| die("open", e)),
        );
        let (n, mut keys) = recovered(&db).unwrap_or_else(|e| die("reopened", e));
        println!("OPENED {n}");

        let (done, saved) = (Arc::new(AtomicI64::new(n)), Arc::new(AtomicI64::new(n)));
        {
            let (db, done, saved) = (Arc::clone(&db), Arc::clone(&done), Arc::clone(&saved));
            std::thread::spawn(move || {
                for round in 0u64.. {
                    let covered = done.load(Ordering::Acquire);
                    let result = if round % 5 == 4 {
                        db.compact()
                    } else {
                        db.flush()
                    };
                    if let Err(e) = result {
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
            let durable =
                commit(&db, i, &mut keys).unwrap_or_else(|e| die(&format!("commit {i}"), e));
            done.store(i + 1, Ordering::Release);
            if logged || durable {
                println!("DURABLE {}", i + 1);
            }
        }
        std::thread::sleep(Duration::from_secs(60));
    }

    /// How far the victim commits past its last save: keeps the file, and each open, small.
    const AHEAD: i64 = 64;

    // -- the database history --

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

    /// Commit `i` of the history, on `tx`: node `k = i`, linked to node `i - 1`; node `i / 2`
    /// updated to `v = i`; every fourth commit, node `i - 7` deleted with its relationships. The
    /// first one also indexes `k`. `keys` maps the live nodes' `k` to their ids.
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

    /// Commits step `i`: most as plain write transactions, some optimistic, some as bulk loads.
    /// `true`: durable on return without the redo log (a bulk load saves as it commits).
    fn commit(
        db: &Arc<Database>,
        i: i64,
        keys: &mut HashMap<i64, NodeId>,
    ) -> traverse_core::error::Result<bool> {
        if i % 40 == 39 {
            let mut tx = db.begin_bulk_load(None)?;
            step(i, &mut tx, keys)?;
            tx.commit()?;
            Ok(true)
        } else if i % 7 == 3 {
            let mut tx = db.begin_optimistic_owned(false)?;
            step(i, &mut tx, keys)?;
            tx.commit()?;
            Ok(false)
        } else {
            let mut tx = db.begin_write_owned();
            step(i, &mut tx, keys)?;
            tx.commit()?;
            Ok(false)
        }
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

    /// How many commits of the history `db` holds, and its live nodes' ids by `k`; `Err` when it
    /// isn't a state the history goes through.
    fn recovered(db: &Arc<Database>) -> Result<(i64, HashMap<i64, NodeId>), String> {
        let e = |e: traverse_core::error::Error| e.to_string();
        let tx = db.begin_read_owned().map_err(e)?;
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
            let (Some(&from), Some(&to)) = (by_id.get(&edge.source), by_id.get(&edge.target))
            else {
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
                "not a prefix of the history: n={n}, first wrong node {node:?} (has {:?}, want \
                 {:?}), first wrong relationship {edge:?}",
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

    /// A child running as a victim, its stdout lines arriving on a channel.
    struct Victim {
        child: Child,
        lines: Receiver<String>,
        reader: Option<JoinHandle<()>>,
    }

    impl Victim {
        fn spawn(victim: &str, env: &[(&str, String)]) -> Self {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env("TRAVERSE_KILL_VICTIM", victim)
                .envs(env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().unwrap();
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

        /// The next line; `None` once the child closed its stdout (it exited). Fails the test
        /// when the child stays silent for `timeout`: a hang.
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

    // -- redo-log tests --

    fn rlog_options(dir: &Path, stem: &str) -> WriterOptions {
        WriterOptions {
            segment_size: traverse_core::storage::rlog::MIN_SEGMENT_SIZE,
            stem: stem.to_string(),
            dir: dir.to_path_buf(),
        }
    }

    /// Runs the rlog victim until it acknowledged `kill_after` commits, kills it, and returns the
    /// highest commit it acknowledged.
    fn kill_rlog_victim(dir: &Path, stem: &str, total: i64, kill_after: i64) -> i64 {
        let mut victim = Victim::spawn(
            "rlog",
            &[
                ("TRAVERSE_KILL_DIR", dir.display().to_string()),
                ("TRAVERSE_KILL_STEM", stem.to_string()),
                ("TRAVERSE_KILL_TOTAL", total.to_string()),
            ],
        );
        let mut acked = -1;
        while acked < kill_after - 1 {
            let Some(line) = victim.next_line(Duration::from_secs(30)) else {
                break;
            };
            acked = parse(&line).1;
        }
        let (rest, stderr) = victim.kill();
        for line in rest {
            acked = acked.max(parse(&line).1);
        }
        assert!(
            acked >= kill_after - 1,
            "the victim stopped after {acked}; stderr: {stderr}"
        );
        acked
    }

    /// The `i` of each logged transaction, in log order.
    fn logged(dir: &Path, stem: &str) -> Vec<i64> {
        let report = scan_segments(dir, stem, 0).unwrap();
        let mut last_ts = 0;
        report
            .transactions
            .iter()
            .map(|tx| {
                assert!(tx.commit_ts >= last_ts, "commits out of order");
                last_ts = tx.commit_ts;
                match &tx.ops[0] {
                    RedoOp::CreateNode { properties, .. } => {
                        match properties.iter().find(|(k, _)| k == "i") {
                            Some((_, PropertyValue::Int(i))) => *i,
                            other => panic!("i: {other:?}"),
                        }
                    }
                    other => panic!("unexpected op {other:?}"),
                }
            })
            .collect()
    }

    fn rlog_keeps_every_acknowledged_commit() {
        let tmp = TempDir::new().unwrap();
        let acked = kill_rlog_victim(tmp.path(), "kill1.tvdb", 1000, 50);
        let found = logged(tmp.path(), "kill1.tvdb");
        assert!(
            found.len() as i64 > acked,
            "lost commits: acked {acked}, found {}",
            found.len()
        );
        assert!(
            found.iter().copied().eq(0..found.len() as i64),
            "commits out of order"
        );
    }

    fn rlog_appends_after_a_killed_writer() {
        let tmp = TempDir::new().unwrap();
        let acked = kill_rlog_victim(tmp.path(), "kill2.tvdb", 100, 5);
        let writer = LogWriter::open(rlog_options(tmp.path(), "kill2.tvdb")).unwrap();
        let store = MvccStore::new(Graph::new()).with_log_writer(Arc::new(Mutex::new(writer)));
        for i in 1000..1005 {
            let mut tx = store.begin_write();
            let mut props = PropertyMap::new();
            props.insert("i".into(), PropertyValue::Int(i));
            GraphView::create_node(&mut tx, &["N"], props).unwrap();
            tx.commit().unwrap();
        }
        drop(store);

        let found = logged(tmp.path(), "kill2.tvdb");
        let (old, new): (Vec<i64>, Vec<i64>) = found.iter().partition(|&&i| i < 1000);
        assert!(
            old.len() as i64 > acked,
            "lost commits: acked {acked}, found {}",
            old.len()
        );
        assert_eq!(new, (1000..1005).collect::<Vec<_>>());
        assert!(found.ends_with(&new), "new commits not after the old ones");
    }

    fn rlog_survives_kills_at_any_commit() {
        let mut rng = Rng::new();
        for round in 0..8 {
            let tmp = TempDir::new().unwrap();
            let stem = format!("kill_rand_{round}.tvdb");
            let acked = kill_rlog_victim(tmp.path(), &stem, 300, 1 + rng.below(120) as i64);
            let found = logged(tmp.path(), &stem);
            assert!(
                found.len() as i64 > acked,
                "round {round}: lost commits: acked {acked}, found {}",
                found.len()
            );
            assert!(
                found.iter().copied().eq(0..found.len() as i64),
                "commits out of order"
            );
        }
    }

    // -- database tests --

    fn a_logged_database_survives_kills() {
        database_survives_kills(true, 40);
    }

    fn a_database_saving_snapshots_survives_kills() {
        database_survives_kills(false, 40);
    }

    /// Kills the db victim `kills` times at random points (some while it opens), checking every
    /// open against what it reported durable before; then opens the database here once more.
    fn database_survives_kills(logged: bool, kills: u32) {
        let mut rng = Rng::new();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("victim.tvdb");
        let env = [
            ("TRAVERSE_KILL_DB", path.display().to_string()),
            ("TRAVERSE_KILL_LOGGED", u8::from(logged).to_string()),
        ];
        // longest prefix of the history reported durable so far
        let mut durable = 0;
        let mut opens = 0;
        for kill in 0..kills {
            let mut victim = Victim::spawn("db", &env);
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

        let db = Arc::new(Database::open_with_durability(&path, logged).unwrap());
        let (n, _) = recovered(&db).unwrap_or_else(|e| panic!("last open: {e}"));
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
}
