//! `apoc.periodic.{submit,repeat,list,cancel,countdown}`: the background job
//! scheduler.
//!
//! Jobs run on a pluggable BACKGROUND EXECUTOR the server installs at startup
//! (it runs a statement against a database like a client would, on its own
//! thread). Embedded without one, `submit` / `countdown` run their statement
//! INLINE once and register as done, and `repeat` runs one inline iteration:
//! same observable result, minus the asynchrony.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// A registered job's observable state (the YIELD row of every scheduler procedure).
#[derive(Clone)]
pub struct JobInfo {
    /// Job name (registry key).
    pub name: String,
    /// Initial delay in seconds (countdown); 0 for submit/repeat.
    pub delay: i64,
    /// Repeat rate in seconds; 0 for one-shot jobs.
    pub rate: i64,
    /// Completed (one-shot jobs, after their run).
    pub done: Arc<AtomicBool>,
    /// Cancelled via apoc.periodic.cancel.
    pub cancelled: Arc<AtomicBool>,
}

fn registry() -> &'static RwLock<Vec<JobInfo>> {
    static REG: OnceLock<RwLock<Vec<JobInfo>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(Vec::new()))
}

/// Statement runner installed by the server: runs `statement` against the default database on the
/// caller's thread; an error string on failure (logged, job marked done).
pub type BackgroundExecutor = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

static EXECUTOR: OnceLock<BackgroundExecutor> = OnceLock::new();

/// Installs the background executor (server startup; first call wins).
pub fn install_executor(exec: BackgroundExecutor) {
    let _ = EXECUTOR.set(exec);
}

/// Is a real background executor installed?
pub fn has_executor() -> bool {
    EXECUTOR.get().is_some()
}

/// Registers a job, replacing (and cancelling) any job with the same name.
pub fn register(name: &str, delay: i64, rate: i64) -> JobInfo {
    let job = JobInfo {
        name: name.to_string(),
        delay,
        rate,
        done: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let mut reg = registry().write().unwrap_or_else(|p| p.into_inner());
    if let Some(old) = reg.iter().position(|j| j.name == name) {
        reg[old].cancelled.store(true, Ordering::SeqCst);
        reg.remove(old);
    }
    reg.push(job.clone());
    job
}

/// All jobs, sorted by name.
pub fn list() -> Vec<JobInfo> {
    let mut v = registry().read().map(|r| r.clone()).unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Cancels and removes a job, returning its final state.
pub fn cancel(name: &str) -> Option<JobInfo> {
    let mut reg = registry().write().ok()?;
    let i = reg.iter().position(|j| j.name == name)?;
    reg[i].cancelled.store(true, Ordering::SeqCst);
    Some(reg.remove(i))
}

/// Spawns (or runs inline) a job: first run after `delay` seconds; `rate` > 0 repeats until
/// cancelled; one-shot jobs mark themselves done.
pub fn spawn(job: JobInfo, statement: String) {
    let Some(exec) = EXECUTOR.get() else {
        // embedded fallback: one inline run, then done
        let _ = exec_inline(&statement);
        job.done.store(true, Ordering::SeqCst);
        return;
    };
    let _ = exec; // fetched via EXECUTOR.get() inside the thread
    std::thread::Builder::new()
        .name(format!("apoc-periodic-{}", job.name))
        .spawn(move || {
            let exec = EXECUTOR.get().expect("checked above");
            if job.delay > 0 {
                sleep_cancellable(job.delay, &job.cancelled);
            }
            if job.rate > 0 {
                // fixed-delay repetition: first run after one interval
                loop {
                    sleep_cancellable(job.rate, &job.cancelled);
                    if job.cancelled.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Err(e) = exec(&statement) {
                        eprintln!("apoc.periodic job `{}` failed: {e}", job.name);
                        break;
                    }
                }
            } else if !job.cancelled.load(Ordering::SeqCst)
                && let Err(e) = exec(&statement)
            {
                eprintln!("apoc.periodic job `{}` failed: {e}", job.name);
            }
            job.done.store(true, Ordering::SeqCst);
        })
        .ok();
}

fn sleep_cancellable(secs: i64, cancelled: &AtomicBool) {
    let mut remaining_ms = secs.max(0) * 1000;
    while remaining_ms > 0 && !cancelled.load(Ordering::SeqCst) {
        let step = remaining_ms.min(100);
        std::thread::sleep(std::time::Duration::from_millis(step as u64));
        remaining_ms -= step;
    }
}

thread_local! {
    /// Inline-run outcome for the embedded (no-executor) fallback, set per query by the procedure
    /// arm so the statement runs against the SAME graph borrow.
    static INLINE_RESULT: std::cell::RefCell<Option<Result<(), String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Embedded fallback: the procedure arm runs the statement itself (it holds `&mut graph`) and
/// records the outcome here before calling [`spawn`], whose fallback path consumes it.
pub fn set_inline_result(r: Result<(), String>) {
    INLINE_RESULT.with(|c| *c.borrow_mut() = Some(r));
}

fn exec_inline(_statement: &str) -> Result<(), String> {
    INLINE_RESULT.with(|c| c.borrow_mut().take().unwrap_or(Ok(())))
}
