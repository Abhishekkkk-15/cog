use std::sync::Arc;
use serde::Deserialize;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::message::Message;
use crate::nodes::{recv_lossy, Node};
use crate::provider::{ChatRequest, Provider};
use crate::state::{AgentState, Event, Milestone, MilestoneStatus, Task, TaskStatus};

pub struct PlannerNode {
    provider: Arc<dyn Provider>,
    model: String,
}

impl PlannerNode {
    pub fn new(provider: Arc<dyn Provider>, model: String) -> Self {
        Self { provider, model }
    }

    /// Asks the LLM to break `goal` into an ordered list of concrete steps.
    /// Falls back to treating the whole goal as one step on any failure
    /// (provider error, empty response, unparseable JSON, empty list) —
    /// decomposition is a quality improvement, not something that should
    /// be able to fail the run outright.
    async fn decompose(&self, goal: &str) -> Vec<String> {
        let fallback = || vec![goal.to_string()];

        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(
                    "You are a planning assistant. Break the user's goal into the FEWEST steps a coding agent \
                     genuinely needs to accomplish it — most goals need only 1-3 steps. Each step should be a \
                     substantial, independent chunk of work; do not split a single coherent activity into \
                     separate steps just because it involves multiple actions (e.g. \"read the project's files \
                     and summarize its purpose\" is one step, not one step per file). Respond with ONLY a JSON \
                     object of the form {\"tasks\": [\"step one\", \"step two\"]} and nothing else — no \
                     markdown, no commentary. If the goal is already a single simple step, return a \
                     single-element list.",
                ),
                Message::user(goal.to_string()),
            ],
            tools: None,
            tool_choice: None,
            stream: false,
            temperature: None,
            max_tokens: None,
        };

        let Ok(resp) = self.provider.chat(&req).await else { return fallback() };
        let Some(content) = resp.message.content else { return fallback() };

        #[derive(Deserialize)]
        struct DecomposedPlan {
            tasks: Vec<String>,
        }

        match serde_json::from_str::<DecomposedPlan>(strip_code_fence(&content)) {
            Ok(plan) if !plan.tasks.is_empty() => plan.tasks,
            _ => fallback(),
        }
    }
}

/// LLMs often wrap JSON in a ```json fenced block despite being told not
/// to — strip it rather than failing the parse and falling back needlessly.
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let trimmed = trimmed.strip_prefix("```json").or_else(|| trimmed.strip_prefix("```")).unwrap_or(trimmed);
    trimmed.strip_suffix("```").unwrap_or(trimmed).trim()
}

#[async_trait::async_trait]
impl Node for PlannerNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        while let Some(event) = recv_lossy(&mut rx, "PlannerNode").await {
            match event {
                Event::GoalReceived(prompt) => {
                    tracing::info!("PlannerNode: Goal received");
                    {
                        let mut st = state.write().await;
                        st.goal.original_prompt = prompt.clone();
                        st.conversation.push(Message::user(prompt.clone()));
                    }

                    let steps = self.decompose(&prompt).await;
                    {
                        let mut st = state.write().await;
                        let milestone = build_milestone(&st.plan.milestones, &prompt, steps);
                        st.plan.milestones.push(milestone);
                    }
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

/// Builds a new milestone with an id that can't collide with any already
/// in `existing` — a long-lived TUI session runs `GoalReceived` more than
/// once, and every milestone/task used to be hardcoded to "m1"/"t1", so a
/// second goal's tasks silently aliased the first goal's already-completed
/// ones. `next_pending_task`/`mark_task_completed` scan *all* milestones
/// by id, so a collision meant the wrong milestone's task could get
/// activated or marked complete — exactly the "task N finishes, then task
/// N-1 'starts' again" confusion this fixes. Task ids are namespaced under
/// the milestone id for the same reason.
fn build_milestone(existing: &[Milestone], goal: &str, steps: Vec<String>) -> Milestone {
    let milestone_id = format!("m{}", existing.len() + 1);
    Milestone {
        id: milestone_id.clone(),
        description: format!("Execute: {goal}"),
        status: MilestoneStatus::Pending,
        tasks: steps
            .into_iter()
            .enumerate()
            .map(|(i, description)| Task { id: format!("{milestone_id}-t{}", i + 1), description, dependencies: vec![], status: TaskStatus::Pending, retry_count: 0 })
            .collect(),
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
    use crate::message::Role;
    use crate::provider::{ChatResponse, DummyProvider, FinishReason};

    #[test]
    fn strip_code_fence_removes_json_fenced_block() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
    }

    fn scripted_response(content: &str) -> ChatResponse {
        ChatResponse { message: Message { role: Role::Assistant, content: Some(content.to_string()), ..Default::default() }, finish_reason: FinishReason::Stop, usage: None }
    }

    #[tokio::test]
    async fn decompose_parses_a_valid_json_task_list() {
        let provider = Arc::new(DummyProvider::scripted(vec![scripted_response(r#"{"tasks": ["step one", "step two"]}"#)]));
        let node = PlannerNode::new(provider, "test-model".into());
        assert_eq!(node.decompose("do a thing").await, vec!["step one".to_string(), "step two".to_string()]);
    }

    #[tokio::test]
    async fn decompose_handles_a_markdown_fenced_response() {
        let provider = Arc::new(DummyProvider::scripted(vec![scripted_response("```json\n{\"tasks\": [\"only step\"]}\n```")]));
        let node = PlannerNode::new(provider, "test-model".into());
        assert_eq!(node.decompose("do a thing").await, vec!["only step".to_string()]);
    }

    #[tokio::test]
    async fn decompose_falls_back_to_a_single_task_on_unparseable_response() {
        let provider = Arc::new(DummyProvider::scripted(vec![scripted_response("sure, I'll get right on that!")]));
        let node = PlannerNode::new(provider, "test-model".into());
        assert_eq!(node.decompose("do a thing").await, vec!["do a thing".to_string()]);
    }

    #[tokio::test]
    async fn decompose_falls_back_to_a_single_task_on_empty_task_list() {
        let provider = Arc::new(DummyProvider::scripted(vec![scripted_response(r#"{"tasks": []}"#)]));
        let node = PlannerNode::new(provider, "test-model".into());
        assert_eq!(node.decompose("do a thing").await, vec!["do a thing".to_string()]);
    }

    fn task(id: &str, status: TaskStatus) -> Task {
        Task { id: id.into(), description: String::new(), dependencies: vec![], status, retry_count: 0 }
    }

    fn milestone(id: &str, status: MilestoneStatus, tasks: Vec<Task>) -> Milestone {
        Milestone { id: id.into(), description: String::new(), status, tasks }
    }

    #[test]
    fn build_milestone_ids_dont_collide_with_an_earlier_completed_milestone() {
        let existing = vec![milestone("m1", MilestoneStatus::Completed, vec![task("m1-t1", TaskStatus::Completed)])];
        let second = build_milestone(&existing, "do another thing", vec!["step a".to_string(), "step b".to_string()]);
        assert_eq!(second.id, "m2");
        assert_eq!(second.tasks[0].id, "m2-t1");
        assert_eq!(second.tasks[1].id, "m2-t2");
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
