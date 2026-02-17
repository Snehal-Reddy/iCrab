//! SubagentManager: task tracking, stable IDs, cancellation, bounded pruning.
//!
//! Single `Arc<SubagentManager>` shared between spawn tool and background tasks.
//! Interior mutability via `RwLock`; lock scopes kept short.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::llm::HttpProvider;
use crate::telegram::OutboundMsg;
use crate::tools::registry::ToolRegistry;

const MAX_COMPLETED_TASKS: usize = 50;

// ---------------------------------------------------------------------------
// Task types
// ---------------------------------------------------------------------------

/// Status of a subagent task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for SubagentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Completed => f.write_str("completed"),
            Self::Failed => f.write_str("failed"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// Public snapshot of a subagent task (no abort handle).
#[derive(Debug, Clone)]
pub struct SubagentTask {
    pub id: String,
    pub label: Option<String>,
    pub task: String,
    pub status: SubagentStatus,
    pub result: Option<String>,
    pub created_at: Instant,
}

/// Internal entry: task snapshot + abort handle.
struct TaskEntry {
    info: SubagentTask,
    abort_handle: Option<AbortHandle>,
}

/// Mutable state behind the RwLock.
struct ManagerState {
    tasks: HashMap<String, TaskEntry>,
}

// ---------------------------------------------------------------------------
// SubagentManager
// ---------------------------------------------------------------------------

/// Owns subagent config and task map.  Cheap to clone via `Arc`.
pub struct SubagentManager {
    llm: Arc<HttpProvider>,
    registry: Arc<ToolRegistry>,
    model: String,
    workspace: PathBuf,
    restrict_to_workspace: bool,
    max_iterations: u32,
    next_id: AtomicU64,
    state: RwLock<ManagerState>,
}

impl SubagentManager {
    pub fn new(
        llm: Arc<HttpProvider>,
        registry: Arc<ToolRegistry>,
        model: String,
        workspace: PathBuf,
        restrict_to_workspace: bool,
        max_iterations: u32,
    ) -> Self {
        Self {
            llm,
            registry,
            model,
            workspace,
            restrict_to_workspace,
            max_iterations,
            next_id: AtomicU64::new(1),
            state: RwLock::new(ManagerState {
                tasks: HashMap::new(),
            }),
        }
    }

    // -- config accessors (immutable after construction) --

    #[inline]
    pub fn llm(&self) -> &HttpProvider {
        &self.llm
    }

    #[inline]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    #[inline]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[inline]
    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    #[inline]
    pub fn restrict_to_workspace(&self) -> bool {
        self.restrict_to_workspace
    }

    #[inline]
    pub fn max_iterations(&self) -> u32 {
        self.max_iterations
    }

    // -- task operations --

    /// Spawn a subagent.  Returns the task ID immediately (does not block).
    /// The subagent runs in a `tokio::spawn` background task.
    pub fn spawn(
        self: &Arc<Self>,
        task: String,
        label: Option<String>,
        chat_id: i64,
        outbound_tx: Arc<mpsc::Sender<OutboundMsg>>,
        channel: String,
    ) -> String {
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task_id = format!("subagent-{}", id_num);

        // Insert task as Running (abort_handle filled after spawn).
        let entry = TaskEntry {
            info: SubagentTask {
                id: task_id.clone(),
                label: label.clone(),
                task: task.clone(),
                status: SubagentStatus::Running,
                result: None,
                created_at: Instant::now(),
            },
            abort_handle: None,
        };

        {
            let mut st = self.state.write().expect("subagent state lock");
            st.tasks.insert(task_id.clone(), entry);
        }

        // Spawn the async runner.
        let manager = Arc::clone(self);
        let tid = task_id.clone();
        let handle = tokio::spawn(async move {
            super::run_subagent(manager, tid, task, label, chat_id, outbound_tx, channel).await;
        });

        // Store abort handle so we can cancel later.
        {
            let mut st = self.state.write().expect("subagent state lock");
            if let Some(e) = st.tasks.get_mut(&task_id) {
                e.abort_handle = Some(handle.abort_handle());
            }
        }

        task_id
    }

    /// Mark a task as completed/failed.  Called from inside the spawned task
    /// when `run_subagent` finishes.  Idempotent: ignores if already terminal.
    pub fn complete_task(&self, task_id: &str, status: SubagentStatus, result: Option<String>) {
        let mut st = self.state.write().expect("subagent state lock");
        if let Some(e) = st.tasks.get_mut(task_id) {
            if e.info.status != SubagentStatus::Running {
                return; // already terminal — idempotent
            }
            e.info.status = status;
            e.info.result = result;
            e.abort_handle = None;
        }
        prune_completed(&mut st);
    }

    /// Cancel a running task.  Returns `true` if the task was running and is
    /// now cancelled; `false` if not found or already terminal.
    pub fn cancel(&self, task_id: &str) -> bool {
        let mut st = self.state.write().expect("subagent state lock");
        let Some(e) = st.tasks.get_mut(task_id) else {
            return false;
        };
        if e.info.status != SubagentStatus::Running {
            return false;
        }
        if let Some(h) = e.abort_handle.take() {
            h.abort();
        }
        e.info.status = SubagentStatus::Cancelled;
        e.info.result = Some("Cancelled".to_string());
        true
    }

    /// Snapshot of a single task (cheap clone).
    pub fn get_task(&self, task_id: &str) -> Option<SubagentTask> {
        let st = self.state.read().expect("subagent state lock");
        st.tasks.get(task_id).map(|e| e.info.clone())
    }

    /// Snapshot of all tasks.
    pub fn list_tasks(&self) -> Vec<SubagentTask> {
        let st = self.state.read().expect("subagent state lock");
        st.tasks.values().map(|e| e.info.clone()).collect()
    }
}

/// Drop completed/failed/cancelled tasks when count exceeds the cap,
/// keeping the most recent ones.  Running tasks are never pruned.
fn prune_completed(st: &mut ManagerState) {
    let mut non_running: Vec<(String, Instant)> = st
        .tasks
        .iter()
        .filter(|(_, e)| e.info.status != SubagentStatus::Running)
        .map(|(k, e)| (k.clone(), e.info.created_at))
        .collect();

    if non_running.len() <= MAX_COMPLETED_TASKS {
        return;
    }

    // Sort oldest first, remove the excess.
    non_running.sort_by_key(|(_, t)| *t);
    let to_remove = non_running.len() - MAX_COMPLETED_TASKS;
    for (k, _) in non_running.into_iter().take(to_remove) {
        st.tasks.remove(&k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_display() {
        assert_eq!(SubagentStatus::Running.to_string(), "running");
        assert_eq!(SubagentStatus::Completed.to_string(), "completed");
        assert_eq!(SubagentStatus::Failed.to_string(), "failed");
        assert_eq!(SubagentStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn prune_keeps_bounded() {
        let mut st = ManagerState {
            tasks: HashMap::new(),
        };
        // Insert MAX_COMPLETED_TASKS + 10 completed tasks.
        for i in 0..(MAX_COMPLETED_TASKS + 10) {
            let id = format!("subagent-{}", i);
            st.tasks.insert(
                id.clone(),
                TaskEntry {
                    info: SubagentTask {
                        id: id.clone(),
                        label: None,
                        task: "t".into(),
                        status: SubagentStatus::Completed,
                        result: Some("ok".into()),
                        created_at: Instant::now(),
                    },
                    abort_handle: None,
                },
            );
        }
        prune_completed(&mut st);
        assert!(st.tasks.len() <= MAX_COMPLETED_TASKS);
    }

    #[test]
    fn cancel_nonexistent_returns_false() {
        let mgr = SubagentManager::new(
            Arc::new(stub_provider()),
            Arc::new(crate::tools::registry::ToolRegistry::new()),
            "m".into(),
            std::path::PathBuf::from("/tmp"),
            true,
            5,
        );
        assert!(!mgr.cancel("subagent-999"));
    }

    #[test]
    fn complete_task_idempotent() {
        let mgr = SubagentManager::new(
            Arc::new(stub_provider()),
            Arc::new(crate::tools::registry::ToolRegistry::new()),
            "m".into(),
            std::path::PathBuf::from("/tmp"),
            true,
            5,
        );
        // Manually insert a running task.
        {
            let mut st = mgr.state.write().unwrap();
            st.tasks.insert(
                "subagent-1".into(),
                TaskEntry {
                    info: SubagentTask {
                        id: "subagent-1".into(),
                        label: None,
                        task: "t".into(),
                        status: SubagentStatus::Running,
                        result: None,
                        created_at: Instant::now(),
                    },
                    abort_handle: None,
                },
            );
        }
        mgr.complete_task("subagent-1", SubagentStatus::Completed, Some("a".into()));
        mgr.complete_task("subagent-1", SubagentStatus::Failed, Some("b".into()));
        let t = mgr.get_task("subagent-1").unwrap();
        assert_eq!(t.status, SubagentStatus::Completed);
        assert_eq!(t.result.as_deref(), Some("a"));
    }

    /// Minimal provider stub for tests that never call chat().
    fn stub_provider() -> HttpProvider {
        // HttpProvider::from_config requires a real config; we construct one
        // with dummy values.  The provider is never used in these unit tests.
        let cfg = crate::config::Config {
            workspace: Some("/tmp".into()),
            restrict_to_workspace: Some(true),
            telegram: None,
            llm: Some(crate::config::LlmConfig {
                provider: None,
                api_base: Some("http://localhost:1".into()),
                api_key: Some("test".into()),
                model: Some("test".into()),
            }),
            tools: None,
            heartbeat: None,
        };
        HttpProvider::from_config(&cfg).expect("stub provider")
    }
}
