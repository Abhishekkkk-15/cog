use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

use super::{Tool, ToolContext, ToolError};

const MAX_DIFF_CHARS: usize = 4000;

#[derive(Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Overwrite a file with new content, creating it (and any parent directories) if it doesn't exist."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file, relative to the working directory."},
                "content": {"type": "string", "description": "The full new contents of the file."}
            },
            "required": ["path", "content"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn confirmation_description(&self, args: &Value, ctx: &ToolContext) -> String {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let new_content = args.get("content").and_then(Value::as_str).unwrap_or("");
        let old_content = std::fs::read_to_string(ctx.cwd.join(path)).unwrap_or_default();

        if old_content == new_content {
            return format!("write_file {path}: no changes");
        }

        let diff = TextDiff::from_lines(&old_content, new_content);
        let mut out = format!("write_file {path}:\n");
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            out.push_str(sign);
            out.push_str(change.as_str().unwrap_or(""));
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        if out.len() > MAX_DIFF_CHARS {
            out.truncate(MAX_DIFF_CHARS);
            out.push_str("\n... [diff truncated]");
        }
        out
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: WriteFileParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let full_path = ctx.cwd.join(&params.path);
        let old_lines = std::fs::read_to_string(&full_path).ok().map(|s| s.lines().count());

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, &params.content)?;

        let new_lines = params.content.lines().count();
        Ok(match old_lines {
            Some(old) => format!("wrote {} ({old} -> {new_lines} lines)", params.path),
            None => format!("created {} ({new_lines} lines)", params.path),
        })
    }
}
