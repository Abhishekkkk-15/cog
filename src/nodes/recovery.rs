use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::message::Message;
use crate::nodes::{recv_lossy, Node};
use crate::state::{AgentState, Event, TaskStatus};
use crate::tools::FileSnapshots;

/// Maximum automatic retry attempts per task before giving up and
/// surfacing the failure instead of looping forever.
const MAX_RETRIES: usize = 2;

#[derive(Debug, PartialEq, Eq)]
enum RecoveryDecision {
    Retry,
    GiveUp,
}

pub struct RecoveryNode {
    snapshots: FileSnapshots,
}

impl RecoveryNode {
    pub fn new(snapshots: FileSnapshots) -> Self {
        Self { snapshots }
    }
}

#[async_trait::async_trait]
impl Node for RecoveryNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        while let Some(event) = recv_lossy(&mut rx, "RecoveryNode").await {
            let Event::ReflectionGenerated { tid, reflection } = event else { continue };
            tracing::info!("RecoveryNode: handling reflection for task {tid}");

            let decision = {
                let mut st = state.write().await;
                record_attempt_and_decide(&mut st, &tid, &reflection)
            };

            match decision {
                RecoveryDecision::Retry => {
                    let _ = bus.publish(Event::TaskStarted(tid));
                }
                RecoveryDecision::GiveUp => {
                    tracing::warn!("RecoveryNode: giving up on task {tid} after exhausting retries, restoring touched files");
                    restore_snapshots(&self.snapshots);
                    let _ = bus.publish(Event::RunFinished(false));
                }
            }
        }
    }
}

/// Restores every file the failed task(s) touched back to its pre-run
/// state — `Some(bytes)` overwrites, `None` means the file didn't exist
/// before and gets removed if it does now. Drains the map either way, so
/// a restored (or not-yet-restored-because-of-an-IO-error) entry is never
/// reapplied against a later, unrelated run.
fn restore_snapshots(snapshots: &FileSnapshots) {
    let entries: Vec<_> = snapshots.lock().unwrap().drain().collect();
    for (path, prior) in entries {
        let result = match &prior {
            Some(bytes) => std::fs::write(&path, bytes),
            None => {
                if path.exists() {
                    std::fs::remove_file(&path)
                } else {
                    Ok(())
                }
            }
        };
        if let Err(e) = result {
            tracing::warn!("RecoveryNode: failed to roll back {}: {e}", path.display());
        }
    }
}

/// Bumps the task's retry count and decides whether to retry (pushing the
/// reflection into `conversation` as context for the next attempt) or give
/// up (marking the task `Failed`). A missing task id is treated as
/// give-up rather than retrying blindly against unknown state.
fn record_attempt_and_decide(state: &mut AgentState, tid: &str, reflection: &str) -> RecoveryDecision {
    let task = state.plan.milestones.iter_mut().flat_map(|m| m.tasks.iter_mut()).find(|t| t.id == tid);
    let Some(task) = task else {
        tracing::warn!("RecoveryNode: task {tid} not found in plan, giving up");
        return RecoveryDecision::GiveUp;
    };

    task.retry_count += 1;
    if task.retry_count <= MAX_RETRIES {
        let n = task.retry_count;
        state.conversation.push(Message::system(format!(
            "[Recovery attempt {n}/{MAX_RETRIES}] Previous attempt failed verification.\nReflection: {reflection}\nRetrying the task with this in mind."
        )));
        RecoveryDecision::Retry
    } else {
        task.status = TaskStatus::Failed;
        RecoveryDecision::GiveUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Milestone, MilestoneStatus, Plan, Task};

    fn state_with_task(retry_count: usize) -> AgentState {
        AgentState {
            plan: Plan {
                milestones: vec![Milestone {
                    id: "m1".into(),
                    description: String::new(),
                    status: MilestoneStatus::Active,
                    tasks: vec![Task { id: "t1".into(), description: String::new(), dependencies: vec![], status: TaskStatus::Active, retry_count }],
                }],
            },
            ..Default::default()
        }
    }

    #[test]
    fn retries_and_pushes_reflection_into_conversation_while_under_the_limit() {
        let mut state = state_with_task(0);
        let decision = record_attempt_and_decide(&mut state, "t1", "it broke");
        assert_eq!(decision, RecoveryDecision::Retry);
        assert_eq!(state.plan.milestones[0].tasks[0].retry_count, 1);
        assert!(state.conversation.messages[0].content.as_deref().unwrap_or("").contains("it broke"));
    }

    #[test]
    fn gives_up_and_marks_task_failed_once_retries_are_exhausted() {
        let mut state = state_with_task(MAX_RETRIES);
        let decision = record_attempt_and_decide(&mut state, "t1", "still broken");
        assert_eq!(decision, RecoveryDecision::GiveUp);
        assert_eq!(state.plan.milestones[0].tasks[0].status, TaskStatus::Failed);
    }

    #[test]
    fn gives_up_immediately_when_task_id_is_unknown() {
        let mut state = state_with_task(0);
        let decision = record_attempt_and_decide(&mut state, "missing", "irrelevant");
        assert_eq!(decision, RecoveryDecision::GiveUp);
    }

    fn snapshots_with(entries: Vec<(std::path::PathBuf, Option<Vec<u8>>)>) -> FileSnapshots {
        Arc::new(std::sync::Mutex::new(entries.into_iter().collect()))
    }

    #[test]
    fn restore_snapshots_overwrites_a_modified_file_back_to_its_prior_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "modified by the failed attempt").unwrap();

        let snapshots = snapshots_with(vec![(path.clone(), Some(b"original content".to_vec()))]);
        restore_snapshots(&snapshots);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original content");
        assert!(snapshots.lock().unwrap().is_empty(), "the map should be drained after restoring");
    }

    #[test]
    fn restore_snapshots_removes_a_file_that_did_not_exist_before_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_file.txt");
        std::fs::write(&path, "created by the failed attempt").unwrap();

        let snapshots = snapshots_with(vec![(path.clone(), None)]);
        restore_snapshots(&snapshots);

        assert!(!path.exists(), "a file that didn't exist before the run should be removed on rollback");
    }
}
