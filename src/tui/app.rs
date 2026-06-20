use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::sync::oneshot;

/// Agent (background tokio task) -> UI (render loop) events.
pub enum AgentToUi {
    AssistantTextDelta(String),
    AssistantTextDone,
    ToolCallStarted { id: String, name: String, args_preview: String },
    ToolCallFinished { id: String, result_preview: String, success: bool },
    ConfirmRequest { tool_name: String, description: String, respond_to: oneshot::Sender<ConfirmDecision> },
    AskUser { question: String, options: Vec<String>, respond_to: oneshot::Sender<String> },
    TokenUsageUpdate { prompt: u32, completion: u32, total: u32, budget: usize },
    ConnectionStatusChanged(ConnectionStatus),
    IndexingProgress { current: usize, total: usize },
    MemoryStatsUpdate { facts_count: usize, vectors_count: usize, tokens_used: usize, tokens_budget: usize },
    MemorySnapshotReady(MemorySnapshot),
    Info(String),
    Error(String),
    TurnComplete,
}

/// UI (input handling) -> Agent (background tokio task) events.
pub enum UiToAgent {
    UserPrompt(String),
    SlashCommand(SlashCommand),
    RequestMemorySnapshot,
    Quit,
}

pub enum SlashCommand {
    SwitchModel(String),
    SwitchProvider(String),
    OpenConfig,
    Search(String),
    MemoryStats,
    SessionResume(String),
    Forget(String),
    Auth { provider: String, key: String },
}

/// Snapshot of memory state, sent in response to `RequestMemorySnapshot`.
pub struct MemorySnapshot {
    pub facts: Vec<(String, String)>,
    pub sessions: Vec<(String, String, i64)>,  // (id, project_root, created_at)
    pub code_chunks_count: usize,
}

#[derive(Clone)]
pub enum ConnectionStatus {
    Idle,
    Connecting,
    Streaming,
    Error(String),
}

pub enum InputMode {
    Normal,
    Command,
    Confirm,
}

/// Which panel keyboard input is routed to. Tab toggles between them; the
/// file tree only takes Up/Down/Enter/Space, everything else goes to input.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Focus {
    Input,
    FileTree,
    MemoryInspector,
}

/// Controls which panel is shown on the right side of the layout.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum RightPanel {
    FileTree,
    MemoryInspector,
}

/// One rendered line in the chat panel. Kept separate from `message::Message`
/// since this is purely a display concern (streaming partial text, tool
/// lifecycle, system notes) rather than conversation/provider state.
pub enum ChatLine {
    User(String),
    Assistant(String),
    ToolCall { id: String, name: String, args_preview: String, result_preview: Option<String>, success: Option<bool> },
    SystemNote(String),
}

pub enum PendingPrompt {
    Confirm { tool_name: String, description: String, respond_to: oneshot::Sender<ConfirmDecision> },
    Ask { question: String, options: Vec<String>, respond_to: oneshot::Sender<String> },
}

/// A user's answer to a `ConfirmRequest`. `Always` additionally trusts the
/// tool for the rest of this run (session-only — never persisted), so
/// subsequent calls to the same tool skip the prompt entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmDecision {
    Once,
    Always,
    Deny,
}

pub struct FileTreeEntry {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
}

pub struct FileTreeState {
    pub root: PathBuf,
    pub expanded: HashSet<PathBuf>,
    pub modified: HashSet<PathBuf>,
    pub entries: Vec<FileTreeEntry>,
    pub selected: usize,
}

const IGNORED_DIR_NAMES: &[&str] = &["target", ".git", "node_modules"];

impl FileTreeState {
    pub fn new(root: PathBuf) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        let mut state = FileTreeState { root, expanded, modified: HashSet::new(), entries: Vec::new(), selected: 0 };
        state.refresh();
        state
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        let root = self.root.clone();
        self.collect(&root, 0);
    }

    fn collect(&mut self, dir: &Path, depth: usize) {
        let Ok(read_dir) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> = read_dir.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.file_name().cmp(&b.file_name())));

        for path in paths {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if IGNORED_DIR_NAMES.contains(&name) {
                continue;
            }
            let is_dir = path.is_dir();
            self.entries.push(FileTreeEntry { path: path.clone(), depth, is_dir });
            if is_dir && self.expanded.contains(&path) {
                self.collect(&path, depth + 1);
            }
        }
    }

    pub fn toggle_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else { return };
        if !entry.is_dir {
            return;
        }
        let path = entry.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.refresh();
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, len - 1) as usize;
    }
}

pub struct StatusState {
    pub model: String,
    pub provider: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub token_budget: usize,
    pub connection: ConnectionStatus,
    pub facts_count: usize,
    pub vectors_count: usize,
}

pub struct App {
    pub lines: Vec<ChatLine>,
    streaming_assistant_index: Option<usize>,
    pub scroll_offset: usize,
    pub file_tree: FileTreeState,
    pub status: StatusState,
    pub input: tui_input::Input,
    pub mode: InputMode,
    pub focus: Focus,
    pub right_panel: RightPanel,
    pub pending_prompt: Option<PendingPrompt>,
    pub indexing: Option<(usize, usize)>,
    pub memory_snapshot: Option<MemorySnapshot>,
    pub should_quit: bool,
    pub bus: crate::bus::EventBus,
    pub state: std::sync::Arc<tokio::sync::RwLock<crate::state::AgentState>>,
}

impl App {
    pub fn new(root: PathBuf, model: String, provider: String, token_budget: usize, bus: crate::bus::EventBus, state: std::sync::Arc<tokio::sync::RwLock<crate::state::AgentState>>) -> Self {
        App {
            lines: Vec::new(),
            streaming_assistant_index: None,
            scroll_offset: 0,
            file_tree: FileTreeState::new(root),
            status: StatusState { model, provider, prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, token_budget, connection: ConnectionStatus::Idle, facts_count: 0, vectors_count: 0 },
            input: tui_input::Input::default(),
            mode: InputMode::Normal,
            focus: Focus::Input,
            right_panel: RightPanel::FileTree,
            pending_prompt: None,
            indexing: None,
            memory_snapshot: None,
            should_quit: false,
            bus,
            state,
        }
    }

    pub fn push_user_line(&mut self, text: String) {
        self.lines.push(ChatLine::User(text));
        self.scroll_offset = 0;
    }

    /// `scroll_offset` counts lines up from the bottom (0 = pinned to the
    /// latest message); `chat_panel::render` clamps overscroll against the
    /// actual rendered height, so no upper bound is needed here.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
    }

    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    pub fn handle_agent_event(&mut self, event: crate::state::Event) {
        match event {
            crate::state::Event::AssistantStreaming(text) => {
                if let Some(idx) = self.streaming_assistant_index {
                    if let ChatLine::Assistant(ref mut current) = self.lines[idx] {
                        current.push_str(&text);
                    }
                } else {
                    self.streaming_assistant_index = Some(self.lines.len());
                    self.lines.push(ChatLine::Assistant(text));
                }
            }
            crate::state::Event::AssistantStreamingDone => {
                self.streaming_assistant_index = None;
            }
            crate::state::Event::ActionProposed { call } => {
                let args_preview = if call.arguments.len() > 60 { format!("{}...", &call.arguments[..57]) } else { call.arguments.clone() };
                self.lines.push(ChatLine::ToolCall { id: call.id.clone(), name: call.name.clone(), args_preview, result_preview: None, success: None });
            }
            crate::state::Event::ActionFinished { id, result, success } => {
                for line in self.lines.iter_mut().rev() {
                    if let &mut ChatLine::ToolCall { id: ref tool_id, ref mut result_preview, success: ref mut success_flag, .. } = line {
                        if tool_id == &id {
                            let preview = if result.len() > 60 { format!("{}...", &result[..57]) } else { result.clone() };
                            *result_preview = Some(preview);
                            *success_flag = Some(success);
                            break;
                        }
                    }
                }
            }
            crate::state::Event::TurnComplete => {
                self.streaming_assistant_index = None;
                self.status.connection = ConnectionStatus::Idle;
            }
            crate::state::Event::PlanCreated => {
                // Best-effort: PlannerNode already released the write lock
                // before publishing this event, so try_read should succeed;
                // if it doesn't, skipping the heads-up display isn't fatal.
                if let Ok(st) = self.state.try_read() {
                    if let Some(milestone) = st.plan.milestones.last() {
                        self.lines.push(ChatLine::SystemNote("Plan:".to_string()));
                        for (i, task) in milestone.tasks.iter().enumerate() {
                            self.lines.push(ChatLine::SystemNote(format!("  {}. {}", i + 1, task.description)));
                        }
                    }
                }
            }
            crate::state::Event::TaskStarted(tid) => {
                self.lines.push(ChatLine::SystemNote(format!("Starting task: {}", tid)));
            }
            crate::state::Event::VerificationPassed => {
                self.lines.push(ChatLine::SystemNote("Verification passed".into()));
            }
            crate::state::Event::VerificationFailed { error, .. } => {
                self.lines.push(ChatLine::SystemNote(format!("Verification failed: {}", error)));
            }
            crate::state::Event::ReflectionGenerated { reflection, .. } => {
                self.lines.push(ChatLine::SystemNote(format!("Reflection: {}", reflection)));
            }
            crate::state::Event::RunFinished(success) => {
                let msg = if success { "Run finished".to_string() } else { "Run failed after exhausting retries".to_string() };
                self.lines.push(ChatLine::SystemNote(msg));
                self.status.connection = ConnectionStatus::Idle;
            }
            _ => {}
        }
    }
}
