use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};
use crate::tui::AgentToUi;

fn default_path() -> String {
    ".".to_string()
}

#[derive(Deserialize)]
struct SemanticIndexParams {
    #[serde(default = "default_path")]
    path: String,
}

pub struct SemanticIndexCurrentTool;

#[async_trait]
impl Tool for SemanticIndexCurrentTool {
    fn name(&self) -> &str {
        "semantic_index_current"
    }

    fn description(&self) -> &str {
        "Index source files in the project directory for semantic code search. \
         Walks the directory tree respecting .gitignore, parses supported source \
         files (Rust, Python, JavaScript) into AST chunks, embeds them, and stores \
         them for later semantic_search queries via the memory system."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to index, relative to the working directory. Defaults to '.'."}
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: SemanticIndexParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let memory = ctx
            .memory
            .as_ref()
            .ok_or_else(|| ToolError::Execution("memory is not configured".into()))?;

        let root = ctx.cwd.join(&params.path);

        // Collect candidate files using ignore::WalkBuilder for real .gitignore semantics
        let candidates: Vec<std::path::PathBuf> = ignore::WalkBuilder::new(&root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
            .filter(|entry| crate::tools::lang_spec_for(entry.path()).is_some())
            .map(|entry| entry.into_path())
            .collect();

        let total = candidates.len();
        if total == 0 {
            return Ok("no indexable source files found".to_string());
        }

        let mut total_chunks = 0usize;
        let mut errors = 0usize;

        for (i, path) in candidates.iter().enumerate() {
            // Send progress event to the TUI
            if let Some(ui_tx) = &ctx.ui_tx {
                let _ = ui_tx.send(AgentToUi::IndexingProgress {
                    current: i + 1,
                    total,
                });
            }

            let mem = memory.lock().await;
            match mem.index_file(path).await {
                Ok(n) => total_chunks += n,
                Err(e) => {
                    tracing::warn!("failed to index {}: {e}", path.display());
                    errors += 1;
                }
            }
        }

        let mut result = format!("indexed {total} files, {total_chunks} code chunks");
        if errors > 0 {
            result.push_str(&format!(" ({errors} files had errors)"));
        }
        Ok(result)
    }
}
