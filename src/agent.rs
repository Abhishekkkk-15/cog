use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::memory::MemoryManager;
use crate::message::Message;
use crate::provider::{Provider, ProviderError};
use crate::tools::ToolRegistry;
use crate::tui::{AgentToUi, UiToAgent};

const MAX_ROUNDS: usize = 25;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("exceeded {MAX_ROUNDS} tool-call rounds without a final answer")]
    MaxRoundsExceeded,
}

use crate::bus::EventBus;
use crate::state::AgentState;

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    model: String,
    cwd: PathBuf,
    auto_approve: bool,
    memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
    ui_tx: Option<mpsc::UnboundedSender<AgentToUi>>,
    ui_rx: Option<mpsc::UnboundedReceiver<UiToAgent>>,

    // New architecture fields
    pub bus: EventBus,
    pub state: Arc<tokio::sync::RwLock<AgentState>>,
    /// Messages typed while a task is already executing, queued here
    /// instead of going through `GoalReceived` — `ExecutorNode` drains this
    /// into `state.conversation` at the top of each round (a safe boundary
    /// between rounds, never mid-tool-call), so the model sees the steer on
    /// its next turn without a whole new plan/milestone being spun up for it.
    pub steering: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>, tools: ToolRegistry, model: impl Into<String>) -> Self {
        Agent {
            provider: Arc::from(provider),
            tools: Arc::new(tools),
            model: model.into(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            auto_approve: false,
            memory: None,
            ui_tx: None,
            ui_rx: None,
            bus: EventBus::new(100),
            state: AgentState::new(),
            steering: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Auto-approve confirmation-gated tool calls (headless `cog run --yes`).
    pub fn with_auto_approve(mut self, auto_approve: bool) -> Self {
        self.auto_approve = auto_approve;
        self
    }

    /// Appends every registered tool's `prompt_guidelines()` after the given
    /// base prompt — narrow, tool-specific usage tips assembled from the
    /// actual registered `ToolRegistry`, rather than duplicated by hand into
    /// one large prompt string. `self.tools` is already set by `new()` by
    /// the time this builder method runs, so this needs no extra argument.
    pub fn with_system_prompt(self, prompt: impl Into<String>) -> Self {
        let mut full = prompt.into();
        let guidelines = self.tools.prompt_guidelines();
        if !guidelines.is_empty() {
            full.push_str("\n\n");
            full.push_str(&guidelines);
        }
        // Need to use blocking write here since we're in a synchronous builder method
        // Or we can just use a sync Mutex/RwLock, but we have tokio RwLock.
        // For builder, we can use try_write or block_on.
        // Actually, we can use `try_write` since it's uncontended during builder phase.
        if let Ok(mut st) = self.state.try_write() {
            st.conversation.push(Message::system(full));
        }
        self
    }

    /// Wires the agent to a TUI: confirmations and ask_user round-trip
    /// through `ui_tx`/`ui_rx` instead of stdin (used by `run_interactive`,
    /// built out in Phase 4).
    pub fn with_ui_channels(mut self, ui_tx: mpsc::UnboundedSender<AgentToUi>, ui_rx: mpsc::UnboundedReceiver<UiToAgent>) -> Self {
        self.ui_tx = Some(ui_tx);
        self.ui_rx = Some(ui_rx);
        self
    }

    /// Spawns the planner/context/executor/verifier/reflector/recovery
    /// nodes. Each node's `bus.subscribe()` happens here, synchronously,
    /// *before* its task is spawned — not inside the task itself. On a
    /// multi-threaded runtime there's no guarantee a freshly spawned task
    /// has run at all by the time `spawn_nodes()` returns, so subscribing
    /// inside `start()` would race the caller's next `bus.publish()` (e.g.
    /// the initial `GoalReceived`) and could silently drop it forever.
    pub async fn spawn_nodes(&self) {
        use crate::nodes::{planner::PlannerNode, context::ContextNode, executor::ExecutorNode, verifier::VerifierNode, reflector::ReflectorNode, recovery::RecoveryNode, Node};

        // Session-only "always allow" trust set, recreated fresh each time
        // spawn_nodes() runs (once per process invocation) — never persisted.
        let trusted = Arc::new(std::sync::Mutex::new(HashSet::new()));

        // Shared between ExecutorNode (records) and RecoveryNode (restores
        // and drains on a give-up) — see `crate::tools::FileSnapshots`.
        let snapshots: crate::tools::FileSnapshots = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        if let Some(memory) = &self.memory {
            let run_id = self.state.read().await.run_id.clone();
            let mem = memory.lock().await;
            let _ = mem.create_session(&run_id, &self.cwd.to_string_lossy()).await;
        }

        let nodes: Vec<Box<dyn Node>> = vec![
            Box::new(PlannerNode::new(self.provider.clone(), self.model.clone())),
            Box::new(ContextNode::new(self.memory.clone())),
            Box::new(ExecutorNode::new(
                self.provider.clone(),
                self.tools.clone(),
                self.model.clone(),
                self.cwd.clone(),
                self.ui_tx.clone(),
                self.auto_approve,
                self.memory.clone(),
                trusted,
                snapshots.clone(),
                self.steering.clone(),
            )),
            Box::new(VerifierNode::new(self.cwd.clone(), self.provider.clone(), self.model.clone())),
            Box::new(ReflectorNode::new(self.provider.clone(), self.model.clone())),
            Box::new(RecoveryNode::new(snapshots)),
        ];

        for node in nodes {
            let bus = self.bus.clone();
            let rx = bus.subscribe();
            let state = self.state.clone();
            tokio::spawn(async move {
                node.start(bus, rx, state).await;
            });
        }
    }

    /// Wires a memory manager into the agent so tools can persist/query
    /// facts and code chunks, and the loop can auto-compress context.
    pub fn with_memory(mut self, memory: Arc<tokio::sync::Mutex<MemoryManager>>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn dispatch(&self, event: crate::state::Event) {
        if let Err(e) = self.bus.publish(event) {
            tracing::error!("Failed to dispatch event: {e}");
        }
    }
}
