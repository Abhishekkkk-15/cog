use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq)]
pub enum TaskStatus {
    Pending,
    Active,
    Verifying,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub description: String,
    pub dependencies: Vec<String>,
    pub status: TaskStatus,
    pub retry_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MilestoneStatus {
    Pending,
    Active,
    Completed,
}

#[derive(Debug, Clone)]
pub struct Milestone {
    pub id: String,
    pub description: String,
    pub status: MilestoneStatus,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub milestones: Vec<Milestone>,
}

#[derive(Debug, Clone, Default)]
pub struct Goal {
    pub original_prompt: String,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub open_files: Vec<String>,
    pub retrieved_snippets: Vec<String>,
    pub recent_errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentState {
    pub run_id: String,
    pub goal: Goal,
    pub plan: Plan,
    pub context: Context,
    pub conversation: crate::message::Conversation,
}

impl AgentState {
    pub fn new() -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self {
            run_id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        }))
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    GoalReceived(String),
    PlanCreated,
    MilestoneStarted(String),
    TaskStarted(String),
    ContextReady(String), // task_id
    AssistantStreaming(String), // text delta
    AssistantStreamingDone,
    ActionProposed { call: crate::message::ToolCall },
    ActionApproved(String), // tool_call_id
    ActionRejected(String), // tool_call_id
    ActionFinished { id: String, result: String, success: bool },
    TurnComplete,
    ExecutionFinished(String), // task_id
    VerificationPassed,
    VerificationFailed { tid: String, error: String },
    ReflectionGenerated { tid: String, reflection: String },
    TaskCompleted(String),
    /// Whole goal finished (not just one task) — `true` if the plan ran to
    /// completion, `false` if recovery exhausted its retries and gave up.
    RunFinished(bool),
}
