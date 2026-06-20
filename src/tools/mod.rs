mod ask_user;
mod edit_file;
mod fetch_crate_docs;
mod git_commit;
mod git_diff;
mod lang_spec;
mod list_dir;
mod memory_tools;
mod read_file;
mod run_command;
pub mod run_test_suite;
mod search_regex;
mod search_semantic;
mod semantic_index;
mod semantic_replace;
mod web_fetch;
mod write_file;

pub(crate) use lang_spec::lang_spec_for;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::memory::MemoryManager;
use crate::provider::{FunctionSchema, ToolSchema};
use crate::tui::AgentToUi;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Shared context every tool gets. `ui_tx` is only used by ask_user, which
/// round-trips through the TUI for an interactive answer.
pub struct ToolContext {
    pub cwd: PathBuf,
    pub ui_tx: Option<mpsc::UnboundedSender<AgentToUi>>,
    pub memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Hand-written JSON Schema for the tool's parameters object — see plan
    /// rationale: hand-authored descriptions improve model tool-call accuracy
    /// more than schemas derived from Rust doc-comments would.
    fn parameters_schema(&self) -> Value;
    /// True for write_file/edit_file/run_command/git_commit — gates the
    /// agent loop into requesting UI (or stdin) confirmation before execute().
    fn requires_confirmation(&self) -> bool {
        false
    }
    /// Human-readable summary shown in the confirmation popup/stdin prompt.
    /// Default just dumps the raw arguments; tools whose args aren't
    /// naturally readable (a diff, a full file body) override this.
    fn confirmation_description(&self, args: &Value, _ctx: &ToolContext) -> String {
        args.to_string()
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut registry = ToolRegistry { tools: HashMap::new() };
        registry.register(Box::new(read_file::ReadFileTool));
        registry.register(Box::new(write_file::WriteFileTool));
        registry.register(Box::new(edit_file::EditFileTool));
        registry.register(Box::new(list_dir::ListDirTool));
        registry.register(Box::new(search_regex::SearchRegexTool));
        registry.register(Box::new(search_semantic::SearchSemanticTool));
        registry.register(Box::new(run_command::RunCommandTool));
        registry.register(Box::new(git_commit::GitCommitTool));
        registry.register(Box::new(git_diff::GitDiffTool));
        registry.register(Box::new(web_fetch::WebFetchTool));
        registry.register(Box::new(ask_user::AskUserTool));
        registry.register(Box::new(memory_tools::SaveMemoryTool));
        registry.register(Box::new(memory_tools::RecallMemoryTool));
        registry.register(Box::new(semantic_replace::SemanticReplaceTool));
        registry.register(Box::new(fetch_crate_docs::FetchCrateDocsTool));
        registry.register(Box::new(semantic_index::SemanticIndexCurrentTool));
        registry.register(Box::new(run_test_suite::RunTestSuiteTool));
        registry
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|t| ToolSchema {
                kind: "function",
                function: FunctionSchema {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.parameters_schema(),
                },
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn requires_confirmation(&self, name: &str) -> bool {
        self.tools.get(name).map(|t| t.requires_confirmation()).unwrap_or(false)
    }
}
