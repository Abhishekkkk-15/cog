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

/// Shared by `AgentState::new()` (the conversation's real budget — what
/// `MemoryManager::compress_if_needed` checks against) and the TUI's status
/// bar (what gets displayed). These used to be two separately-defined
/// constants; `AgentState::default()`'s derived `Conversation::default()`
/// silently used `token_budget: 0`, which made `compress_if_needed`'s
/// "over 80% of budget" check always true the moment there was any
/// conversation at all — `0 * 0.8 == 0`, and any positive token count is
/// `> 0`. That triggered a real (LLM-call-making) summarization attempt on
/// *every single round*, not just when actually near the limit — dormant
/// since nothing called `compress_if_needed` until it was wired into the
/// live executor loop.
pub const DEFAULT_TOKEN_BUDGET: usize = 128_000;

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
            conversation: crate::message::Conversation::new(DEFAULT_TOKEN_BUDGET),
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
    /// `made_tool_calls` lets `VerifierNode` short-circuit straight to a
    /// failure when the model never acted at all — the same shape as the
    /// k8s-tui bug, where "Verification passed" fired despite zero files
    /// being created, because nothing else checked for that case.
    ExecutionFinished { tid: String, made_tool_calls: bool },
    VerificationPassed,
    VerificationFailed { tid: String, error: String },
    ReflectionGenerated { tid: String, reflection: String },
    TaskCompleted(String),
    /// Whole goal finished (not just one task) — `true` if the plan ran to
    /// completion, `false` if recovery exhausted its retries and gave up.
    RunFinished(bool),
}
