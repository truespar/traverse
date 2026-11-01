use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::sync::LockUnpoisoned as _;

/// Completed/failed tasks are cleaned up after an hour.
const MAX_TASK_AGE: Duration = Duration::from_secs(3600);

/// Task id (e.g. `tsk_a1b2c3d4e5f67890`).
pub type TaskId = String;

/// Async task status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
}

/// Async task info.
#[derive(Debug, Clone, Serialize)]
pub struct TaskInfo {
    pub id: TaskId,
    pub operation: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Creation instant (not serialized; for cleanup).
    #[serde(skip)]
    pub created_instant: Instant,
}

/// Async tasks with lazy cleanup.
pub struct TaskManager {
    tasks: Mutex<HashMap<TaskId, TaskInfo>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Creates a running task and returns its id; also lazily cleans up old finished tasks.
    pub fn create(&self, operation: &str) -> TaskId {
        let id = generate_task_id();
        let now = chrono::Utc::now();
        let info = TaskInfo {
            id: id.clone(),
            operation: operation.to_string(),
            status: TaskStatus::Running,
            result: None,
            error: None,
            created_at: now.to_rfc3339(),
            finished_at: None,
            created_instant: Instant::now(),
        };

        let mut tasks = self.tasks.lock_unpoisoned();
        // lazy cleanup before inserting
        cleanup_old_tasks(&mut tasks, MAX_TASK_AGE);
        tasks.insert(id.clone(), info);
        id
    }

    /// Marks a task completed with a result.
    pub fn complete(&self, id: &str, result: Value) {
        let mut tasks = self.tasks.lock_unpoisoned();
        if let Some(task) = tasks.get_mut(id) {
            task.status = TaskStatus::Completed;
            task.result = Some(result);
            task.finished_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    /// Marks a task failed with an error message.
    pub fn fail(&self, id: &str, error: String) {
        let mut tasks = self.tasks.lock_unpoisoned();
        if let Some(task) = tasks.get_mut(id) {
            task.status = TaskStatus::Failed;
            task.error = Some(error);
            task.finished_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    /// Runs `work` on the blocking pool as a task; its result completes or fails the task, and so
    /// does a panic, so no task stays running forever (`/api/quiesce` waits for them).
    pub fn spawn_blocking(
        self: &std::sync::Arc<Self>,
        operation: &str,
        work: impl FnOnce() -> Result<Value, String> + Send + 'static,
    ) -> TaskId {
        let id = self.create(operation);
        let tasks = std::sync::Arc::clone(self);
        let task = id.clone();
        let operation = operation.to_string();
        tokio::task::spawn_blocking(move || {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
                Ok(Ok(result)) => tasks.complete(&task, result),
                Ok(Err(e)) => tasks.fail(&task, e),
                Err(panic) => {
                    let why = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    tasks.fail(&task, format!("{operation} failed: internal error {why}"));
                }
            }
        });
        id
    }

    /// Snapshot of a task's info, if it exists.
    pub fn get(&self, id: &str) -> Option<TaskInfo> {
        let tasks = self.tasks.lock_unpoisoned();
        tasks.get(id).cloned()
    }

    /// Tasks still running. `/api/quiesce` waits for zero before persisting, so an in-flight async
    /// save/load can't race a host-driven stop.
    pub fn running_count(&self) -> usize {
        let tasks = self.tasks.lock_unpoisoned();
        tasks
            .values()
            .filter(|t| matches!(t.status, TaskStatus::Running))
            .count()
    }
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes completed/failed tasks older than `max_age`.
fn cleanup_old_tasks(tasks: &mut HashMap<TaskId, TaskInfo>, max_age: Duration) {
    let now = Instant::now();
    tasks.retain(|_, task| {
        if task.status == TaskStatus::Running {
            return true;
        }
        now.duration_since(task.created_instant) < max_age
    });
}

/// `tsk_` + 16 random hex chars.
fn generate_task_id() -> TaskId {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("getrandom failed");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("tsk_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_create_and_complete() {
        let mgr = TaskManager::new();
        let id = mgr.create("load");

        // running at first
        let task = mgr.get(&id).unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.operation, "load");
        assert!(task.result.is_none());
        assert!(task.error.is_none());
        assert!(task.finished_at.is_none());

        // complete it
        mgr.complete(&id, serde_json::json!({"ok": true}));
        let task = mgr.get(&id).unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result, Some(serde_json::json!({"ok": true})));
        assert!(task.finished_at.is_some());
    }

    #[test]
    fn task_create_and_fail() {
        let mgr = TaskManager::new();
        let id = mgr.create("save");

        mgr.fail(&id, "disk full".to_string());
        let task = mgr.get(&id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error, Some("disk full".to_string()));
        assert!(task.finished_at.is_some());
    }

    #[test]
    fn task_not_found() {
        let mgr = TaskManager::new();
        assert!(mgr.get("tsk_nonexistent").is_none());
    }

    #[test]
    fn cleanup_removes_old_completed_preserves_running() {
        let mgr = TaskManager::new();

        // two tasks
        let old_id = mgr.create("load");
        let running_id = mgr.create("save");

        // complete the first
        mgr.complete(&old_id, serde_json::json!({"ok": true}));

        // cleanup with zero max_age (everything finished is "old")
        {
            let mut tasks = mgr.tasks.lock().unwrap();
            cleanup_old_tasks(&mut tasks, Duration::from_secs(0));
        }

        // the completed task is gone, the running one stays
        assert!(mgr.get(&old_id).is_none());
        assert!(mgr.get(&running_id).is_some());
        assert_eq!(mgr.get(&running_id).unwrap().status, TaskStatus::Running);
    }

    #[test]
    fn task_id_format() {
        let mgr = TaskManager::new();
        let id = mgr.create("test");
        assert!(id.starts_with("tsk_"));
        assert_eq!(id.len(), 4 + 16); // "tsk_" + 16 hex chars
    }
}
