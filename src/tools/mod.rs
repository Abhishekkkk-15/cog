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

/// Maps an absolute file path to its content from *before* the current run
/// first touched it (`None` = the file didn't exist yet). Populated by
/// `snapshot_before_write` the first time each path is written, so
/// `RecoveryNode` can restore everything if a task ultimately fails after
/// exhausting its retries.
pub type FileSnapshots = Arc<std::sync::Mutex<HashMap<PathBuf, Option<Vec<u8>>>>>;

/// Shared context every tool gets. `ui_tx` is only used by ask_user, which
/// round-trips through the TUI for an interactive answer.
pub struct ToolContext {
    pub cwd: PathBuf,
    pub ui_tx: Option<mpsc::UnboundedSender<AgentToUi>>,
    pub memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
    pub snapshots: Option<FileSnapshots>,
}

/// Records `path`'s current bytes (or `None` if it doesn't exist yet) the
/// *first* time it's touched in a run — a no-op if `ctx.snapshots` isn't
/// wired, or if this exact path was already snapshotted earlier in the
/// same run (a later snapshot would capture an already-modified state,
/// defeating the point of rolling back to the original).
fn snapshot_before_write(ctx: &ToolContext, path: &std::path::Path) {
    let Some(snapshots) = &ctx.snapshots else { return };
    snapshots.lock().unwrap().entry(path.to_path_buf()).or_insert_with(|| std::fs::read(path).ok());
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
    /// Optional short, model-facing usage tip for this specific tool,
    /// appended to the system prompt when the tool is registered. Keep it
    /// narrow — this tool's own failure modes and how to avoid them, not
    /// general engineering advice (that belongs in the base system prompt,
    /// not duplicated per tool).
    fn prompt_guidelines(&self) -> Option<&str> {
        None
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

    /// Assembles every registered tool's `prompt_guidelines()` into one
    /// block, sorted by tool name for a deterministic order regardless of
    /// `HashMap` iteration — empty if no registered tool has any.
    pub fn prompt_guidelines(&self) -> String {
        let mut entries: Vec<(&str, &str)> = self.tools.values().filter_map(|t| t.prompt_guidelines().map(|g| (t.name(), g))).collect();
        entries.sort_by_key(|(name, _)| *name);
        entries.into_iter().map(|(_, g)| g).collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_guidelines_includes_known_tools_in_deterministic_sorted_order() {
        let registry = ToolRegistry::new();
        let guidelines = registry.prompt_guidelines();

        assert!(guidelines.contains("edit_file:"));
        assert!(guidelines.contains("write_file:"));
        assert!(guidelines.contains("run_command:"));

        let edit_pos = guidelines.find("edit_file:").unwrap();
        let run_pos = guidelines.find("run_command:").unwrap();
        let write_pos = guidelines.find("write_file:").unwrap();
        assert!(edit_pos < run_pos && run_pos < write_pos, "expected alphabetical order by tool name");
    }

    #[test]
    fn prompt_guidelines_is_empty_for_a_registry_with_no_guideline_tools() {
        let mut registry = ToolRegistry { tools: HashMap::new() };
        registry.register(Box::new(super::search_regex::SearchRegexTool));
        assert_eq!(registry.prompt_guidelines(), "");
    }
}
