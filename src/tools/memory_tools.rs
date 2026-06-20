use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

// ── save_memory ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SaveMemoryParams {
    key: String,
    value: String,
}

pub struct SaveMemoryTool;

#[async_trait]
impl Tool for SaveMemoryTool {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        "Persist a key-value fact to long-term memory. Use this to remember user preferences, project conventions, or decisions for future sessions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {"type": "string", "description": "A short, unique identifier for this fact (e.g. 'preferred_lang', 'indent_style')."},
                "value": {"type": "string", "description": "The fact content to remember."}
            },
            "required": ["key", "value"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: SaveMemoryParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let memory = ctx
            .memory
            .as_ref()
            .ok_or_else(|| ToolError::Execution("memory is not configured".into()))?;

        let mem = memory.lock().await;
        mem.remember(&params.key, &params.value)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(format!("saved fact '{}' to memory", params.key))
    }
}

// ── recall_memory ────────────────────────────────────────────────────

fn default_limit() -> usize {
    5
}

#[derive(Deserialize)]
struct RecallMemoryParams {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

pub struct RecallMemoryTool;

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Search long-term memory for facts matching a query. Results are ranked by substring match first, then by semantic similarity."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query — matches against fact values by substring and semantic similarity."},
                "limit": {"type": "integer", "description": "Maximum number of results to return (default 5)."}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: RecallMemoryParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let memory = ctx
            .memory
            .as_ref()
            .ok_or_else(|| ToolError::Execution("memory is not configured".into()))?;

        let mem = memory.lock().await;
        let results = mem
            .recall(&params.query, params.limit)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        if results.is_empty() {
            return Ok("no matching facts found".to_string());
        }

        let formatted: Vec<String> = results
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect();
        Ok(formatted.join("\n"))
    }
}
