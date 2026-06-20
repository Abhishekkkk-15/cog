use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::nodes::Node;
use crate::state::{AgentState, Event, Milestone, MilestoneStatus, Task, TaskStatus};

pub struct PlannerNode;

#[async_trait::async_trait]
impl Node for PlannerNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::GoalReceived(prompt) => {
                    tracing::info!("PlannerNode: Goal received");
                    let mut st = state.write().await;
                    st.goal.original_prompt = prompt.clone();
                    st.conversation.push(crate::message::Message::user(prompt.clone()));

                    // In a real implementation, we'd query the LLM to generate milestones.
                    // For now, we mock the strategic decomposition.
                    let milestone = Milestone {
                        id: "m1".into(),
                        description: format!("Execute: {}", prompt),
                        status: MilestoneStatus::Pending,
                        tasks: vec![Task {
                            id: "t1".into(),
                            description: prompt,
                            dependencies: vec![],
                            status: TaskStatus::Pending,
                            retry_count: 0,
                        }],
                    };

                    st.plan.milestones.push(milestone);
                    let _ = bus.publish(Event::PlanCreated);
                }
                Event::PlanCreated => {
                    let mut st = state.write().await;
                    advance_or_finish(&mut st, &bus);
                }
                Event::TaskCompleted(tid) => {
                    let mut st = state.write().await;
                    mark_task_completed(&mut st, &tid);
                    advance_or_finish(&mut st, &bus);
                }
                _ => {}
            }
        }
    }
}

/// Marks `tid` `Completed`, and its owning milestone too if that was the
/// milestone's last incomplete task.
fn mark_task_completed(state: &mut AgentState, tid: &str) {
    for m in &mut state.plan.milestones {
        let owns_task = m.tasks.iter().any(|t| t.id == tid);
        if !owns_task {
            continue;
        }
        if let Some(t) = m.tasks.iter_mut().find(|t| t.id == tid) {
            t.status = TaskStatus::Completed;
        }
        if m.tasks.iter().all(|t| t.status == TaskStatus::Completed) {
            m.status = MilestoneStatus::Completed;
        }
    }
}

/// Activates the next `Pending` milestone/task and returns the task id, or
/// `None` if nothing pending remains anywhere in the plan.
fn next_pending_task(state: &mut AgentState) -> Option<String> {
    for m in &mut state.plan.milestones {
        if m.status == MilestoneStatus::Pending {
            m.status = MilestoneStatus::Active;
        }
        if m.status == MilestoneStatus::Active {
            for t in &mut m.tasks {
                if t.status == TaskStatus::Pending {
                    t.status = TaskStatus::Active;
                    return Some(t.id.clone());
                }
            }
        }
    }
    None
}

fn advance_or_finish(state: &mut AgentState, bus: &EventBus) {
    match next_pending_task(state) {
        Some(tid) => {
            tracing::info!("PlannerNode: Emitting TaskStarted for {}", tid);
            let _ = bus.publish(Event::TaskStarted(tid));
        }
        None => {
            tracing::info!("PlannerNode: no pending tasks remain, run finished");
            let _ = bus.publish(Event::RunFinished(true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskStatus) -> Task {
        Task { id: id.into(), description: String::new(), dependencies: vec![], status, retry_count: 0 }
    }

    fn milestone(id: &str, status: MilestoneStatus, tasks: Vec<Task>) -> Milestone {
        Milestone { id: id.into(), description: String::new(), status, tasks }
    }

    #[test]
    fn mark_task_completed_flips_task_and_then_milestone_when_last_task() {
        let mut state = AgentState { plan: crate::state::Plan { milestones: vec![milestone("m1", MilestoneStatus::Active, vec![task("t1", TaskStatus::Active)])] }, ..Default::default() };
        mark_task_completed(&mut state, "t1");
        assert_eq!(state.plan.milestones[0].tasks[0].status, TaskStatus::Completed);
        assert_eq!(state.plan.milestones[0].status, MilestoneStatus::Completed);
    }

    #[test]
    fn mark_task_completed_leaves_milestone_active_if_a_sibling_task_remains_pending() {
        let mut state = AgentState {
            plan: crate::state::Plan { milestones: vec![milestone("m1", MilestoneStatus::Active, vec![task("t1", TaskStatus::Active), task("t2", TaskStatus::Pending)])] },
            ..Default::default()
        };
        mark_task_completed(&mut state, "t1");
        assert_eq!(state.plan.milestones[0].status, MilestoneStatus::Active);
    }

    #[test]
    fn next_pending_task_activates_a_pending_milestone_and_returns_its_first_task() {
        let mut state = AgentState { plan: crate::state::Plan { milestones: vec![milestone("m1", MilestoneStatus::Pending, vec![task("t1", TaskStatus::Pending)])] }, ..Default::default() };
        let next = next_pending_task(&mut state);
        assert_eq!(next, Some("t1".to_string()));
        assert_eq!(state.plan.milestones[0].status, MilestoneStatus::Active);
        assert_eq!(state.plan.milestones[0].tasks[0].status, TaskStatus::Active);
    }

    #[test]
    fn next_pending_task_returns_none_when_everything_is_completed() {
        let mut state = AgentState { plan: crate::state::Plan { milestones: vec![milestone("m1", MilestoneStatus::Completed, vec![task("t1", TaskStatus::Completed)])] }, ..Default::default() };
        assert_eq!(next_pending_task(&mut state), None);
    }
}
