use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const MAX_OUTPUT_CHARS: usize = 8000;

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file, optionally restricted to a 1-indexed inclusive line range."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file, relative to the working directory."},
                "start_line": {"type": "integer", "description": "Optional 1-indexed start line (inclusive)."},
                "end_line": {"type": "integer", "description": "Optional 1-indexed end line (inclusive)."}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: ReadFileParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let full_path = ctx.cwd.join(&params.path);
        let content = std::fs::read_to_string(&full_path)?;

        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Ok(String::new());
        }
        let start = params.start_line.unwrap_or(1).max(1);
        let end = params.end_line.unwrap_or(lines.len()).min(lines.len());
        if start > end {
            return Ok(String::new());
        }

        let mut out = String::new();
        for (i, line) in lines[start - 1..end].iter().enumerate() {
            out.push_str(&format!("{:>6}: {}\n", start + i, line));
        }
        if out.len() > MAX_OUTPUT_CHARS {
            out.truncate(MAX_OUTPUT_CHARS);
            out.push_str("\n... [truncated]");
        }
        Ok(out)
    }
}
