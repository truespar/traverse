//! Training jobs. `train` queues one and returns its id at once; at most [`set_max_running`] jobs
//! (default 1) train at a time and the rest wait their turn, in order, so concurrent requests
//! can't oversubscribe the CPU or a GPU's memory. `trainStatus` and `trainJobs` report them;
//! `trainCancel` stops one: a queued job never starts, a running one stops at its next epoch or
//! mini-batch. Finished jobs stay listed until [`KEEP_FINISHED`] newer ones have finished.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use traverse_ml::cancel::CancellationToken;

use crate::ml::registry::TaskType;
use crate::query::{Interrupt, QueryContext, QueryOptions};

/// Finished jobs kept for `trainStatus` / `trainJobs`.
pub const KEEP_FINISHED: usize = 100;

/// What a job trains.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub model_name: String,
    pub pipeline: String,
    pub task: TaskType,
    /// Where it trains: `cpu`, `cuda:0`, ...
    pub device: String,
}

/// Where a job is in its life.
#[derive(Debug, Clone)]
pub enum JobState {
    Queued,
    Running,
    /// Trained and registered: wall time, and model info and selection stats as JSON.
    Completed {
        train_millis: i64,
        model_info_json: String,
        selection_stats_json: String,
        /// Why the model couldn't be saved to disk, if it couldn't (it's still registered).
        warning: Option<String>,
    },
    Failed(String),
    Cancelled,
}

impl JobState {
    /// `queued`, `running`, `completed`, `failed` or `cancelled`.
    pub fn name(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Completed { .. } => "completed",
            JobState::Failed(_) => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    fn finished(&self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }
}

/// A job as reported.
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: String,
    pub info: JobInfo,
    pub state: JobState,
    /// Unix seconds.
    pub submitted_at: u64,
    /// Seconds spent training so far; 0 while queued.
    pub elapsed_seconds: u64,
    /// `(completed, planned)` model-selection trials.
    pub progress: (usize, usize),
    /// Jobs ahead of it while it's queued.
    pub queue_position: Option<usize>,
}

/// Handed to a job's work: its cancel token, and progress reporting.
pub struct JobContext {
    id: String,
    pub cancel: CancellationToken,
}

impl JobContext {
    /// Records `(completed, planned)` trials.
    pub fn progress(&self, done: usize, total: usize) {
        if let Some(job) = jobs().all.get_mut(&self.id) {
            job.progress = (done, total);
        }
    }
}

type Work = Box<dyn FnOnce(&JobContext) -> JobState + Send>;

struct Job {
    info: JobInfo,
    state: JobState,
    submitted_at: u64,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    progress: (usize, usize),
    cancel: CancellationToken,
    /// The job runs as a query of its own, so algorithm steps inside it stop on a cancel too.
    query: Arc<QueryContext>,
    work: Option<Work>,
}

struct Jobs {
    all: BTreeMap<String, Job>,
    queue: VecDeque<String>,
    finished: VecDeque<String>,
    /// Worker threads alive; each takes queued jobs until there are none.
    workers: usize,
    max_running: usize,
    seq: u64,
}

static JOBS: Mutex<Jobs> = Mutex::new(Jobs {
    all: BTreeMap::new(),
    queue: VecDeque::new(),
    finished: VecDeque::new(),
    workers: 0,
    max_running: 1,
    seq: 0,
});
/// Signalled whenever a job finishes.
static FINISHED: Condvar = Condvar::new();

// every update is a few field writes, so the state stays consistent even if a holder panicked
fn jobs() -> MutexGuard<'static, Jobs> {
    JOBS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How many jobs may train at once (at least 1); the server sets it at startup.
pub fn set_max_running(n: usize) {
    let mut j = jobs();
    j.max_running = n.max(1);
    start_workers(&mut j);
}

/// Queues `work` and returns the job's id.
pub fn submit(
    info: JobInfo,
    work: impl FnOnce(&JobContext) -> JobState + Send + 'static,
) -> String {
    let mut j = jobs();
    j.seq += 1;
    let id = format!("job-{:x}-{:x}", now_unix(), j.seq);
    j.all.insert(
        id.clone(),
        Job {
            info,
            state: JobState::Queued,
            submitted_at: now_unix(),
            started_at: None,
            finished_at: None,
            progress: (0, 0),
            cancel: CancellationToken::new(),
            query: QueryContext::new(QueryOptions::default()),
            work: Some(Box::new(work)),
        },
    );
    j.queue.push_back(id.clone());
    start_workers(&mut j);
    id
}

/// Starts workers while fewer than `max_running` are alive and there are more queued jobs than
/// workers between jobs (which take queued ones as they come back).
fn start_workers(j: &mut Jobs) {
    while j.workers < j.max_running && j.queue.len() > j.workers.saturating_sub(running(j)) {
        j.workers += 1;
        let spawned = std::thread::Builder::new()
            .name("traverse-ml-train".into())
            .spawn(worker);
        if let Err(e) = spawned {
            j.workers -= 1;
            // nothing will run the queue's head: fail it rather than leave it waiting forever
            if let Some(id) = j.queue.pop_front() {
                finish(
                    j,
                    &id,
                    JobState::Failed(format!("couldn't start a training thread: {e}")),
                );
            }
        }
    }
}

/// Jobs a worker is training right now.
fn running(j: &Jobs) -> usize {
    j.all
        .values()
        .filter(|job| matches!(job.state, JobState::Running))
        .count()
}

fn worker() {
    loop {
        let (id, work, ctx, query) = {
            let mut j = jobs();
            let Some(id) = j.queue.pop_front() else {
                j.workers -= 1;
                return;
            };
            let job = j.all.get_mut(&id).expect("queued jobs are recorded");
            job.state = JobState::Running;
            job.started_at = Some(now_unix());
            let work = job.work.take().expect("a queued job has its work");
            let ctx = JobContext {
                id: id.clone(),
                cancel: job.cancel.clone(),
            };
            (id, work, ctx, Arc::clone(&job.query))
        };
        let scope = query.enter();
        let state = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&ctx))) {
            Ok(state) => state,
            Err(panic) => JobState::Failed(format!(
                "training panicked: {}",
                panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown cause")
            )),
        };
        drop(scope);
        // a job that stopped because it was asked to is cancelled, not failed
        let state = match state {
            JobState::Completed { .. } => state,
            _ if ctx.cancel.is_cancelled() => JobState::Cancelled,
            other => other,
        };
        finish(&mut jobs(), &id, state);
    }
}

/// Moves `id` to a finished state, drops the oldest finished jobs beyond [`KEEP_FINISHED`] and
/// wakes waiters.
fn finish(j: &mut Jobs, id: &str, state: JobState) {
    if let Some(job) = j.all.get_mut(id) {
        job.state = state;
        job.finished_at = Some(now_unix());
        job.work = None;
    }
    j.finished.push_back(id.to_string());
    while j.finished.len() > KEEP_FINISHED {
        if let Some(old) = j.finished.pop_front() {
            j.all.remove(&old);
        }
    }
    FINISHED.notify_all();
}

/// Blocks until job `id` has finished (returns at once for an unknown id).
pub fn wait(id: &str) {
    let mut j = jobs();
    while j.all.get(id).is_some_and(|job| !job.state.finished()) {
        j = FINISHED.wait(j).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Asks job `id` to stop: a queued job is cancelled at once, a running one stops at its next
/// epoch or mini-batch, a finished one is left alone. Returns the job afterwards and whether a
/// cancel was requested.
pub fn cancel(id: &str) -> Option<(JobView, bool)> {
    let mut j = jobs();
    let requested = match j.all.get(id).map(|job| job.state.clone())? {
        JobState::Queued => {
            j.queue.retain(|q| q != id);
            finish(&mut j, id, JobState::Cancelled);
            true
        }
        JobState::Running => {
            let job = &j.all[id];
            job.cancel.cancel();
            job.query.cancel(Interrupt::Cancelled);
            true
        }
        _ => false,
    };
    view(&j, id).map(|v| (v, requested))
}

fn view(j: &Jobs, id: &str) -> Option<JobView> {
    let job = j.all.get(id)?;
    let elapsed = match (job.started_at, job.finished_at) {
        (Some(s), Some(f)) => f.saturating_sub(s),
        (Some(s), None) => now_unix().saturating_sub(s),
        _ => 0,
    };
    Some(JobView {
        id: id.to_string(),
        info: job.info.clone(),
        state: job.state.clone(),
        submitted_at: job.submitted_at,
        elapsed_seconds: elapsed,
        progress: job.progress,
        queue_position: j.queue.iter().position(|q| q == id),
    })
}

/// Job `id`, if it's known.
pub fn get(id: &str) -> Option<JobView> {
    view(&jobs(), id)
}

/// Every known job, newest first.
pub fn list() -> Vec<JobView> {
    let j = jobs();
    let mut out: Vec<JobView> = j.all.keys().filter_map(|id| view(&j, id)).collect();
    out.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at).then(b.id.cmp(&a.id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn info(name: &str) -> JobInfo {
        JobInfo {
            model_name: name.into(),
            pipeline: "p".into(),
            task: TaskType::NodeClassification,
            device: "cpu".into(),
        }
    }

    fn done() -> JobState {
        JobState::Completed {
            train_millis: 1,
            model_info_json: "{}".into(),
            selection_stats_json: "{}".into(),
            warning: None,
        }
    }

    // the queue is process-global: these tests run one at a time
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn jobs_run_one_at_a_time_in_order_and_can_be_cancelled() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let (started_tx, started) = mpsc::channel::<&'static str>();
        let (release_tx, release) = mpsc::channel::<()>();
        let tx = started_tx.clone();
        let first = submit(info("a"), move |_| {
            tx.send("a").unwrap();
            release.recv().unwrap();
            done()
        });
        let tx = started_tx.clone();
        let second = submit(info("b"), move |_| {
            tx.send("b").unwrap();
            done()
        });
        let third = submit(info("c"), move |ctx| {
            // stops when asked
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            JobState::Failed("training cancelled".into())
        });
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok("a"));
        // one at a time: b waits behind a
        let b = get(&second).unwrap();
        assert_eq!(b.state.name(), "queued");
        assert!(b.queue_position.is_some());
        // cancelling a queued job takes it out of line
        let (b, requested) = cancel(&second).unwrap();
        assert!(requested);
        assert_eq!(b.state.name(), "cancelled");
        release_tx.send(()).unwrap();
        wait(&first);
        assert_eq!(get(&first).unwrap().state.name(), "completed");
        // c runs next (b never does) and stops when cancelled
        while get(&third).unwrap().state.name() != "running" {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(cancel(&third).unwrap().1);
        wait(&third);
        assert_eq!(get(&third).unwrap().state.name(), "cancelled");
        assert!(started.try_recv().is_err(), "b must not have started");
        // a finished job can't be cancelled
        assert!(!cancel(&first).unwrap().1);
    }

    #[test]
    fn a_panicking_job_fails_and_the_queue_moves_on() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let bad = submit(info("bad"), |_| panic!("boom"));
        let good = submit(info("good"), |ctx| {
            ctx.progress(3, 4);
            done()
        });
        wait(&bad);
        wait(&good);
        match get(&bad).unwrap().state {
            JobState::Failed(msg) => assert!(msg.contains("boom"), "{msg}"),
            other => panic!("{other:?}"),
        }
        let g = get(&good).unwrap();
        assert_eq!(g.state.name(), "completed");
        assert_eq!(g.progress, (3, 4));
        assert!(list().iter().any(|v| v.id == good));
    }
}
