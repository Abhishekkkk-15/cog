use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

#[derive(Deserialize)]
struct EditFileParams {
    path: String,
    /// Unified diff text (as produced by `diff -u`, including `---`/`+++`
    /// hunk headers) describing the change to apply.
    diff: String,
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Apply a unified diff (as produced by `diff -u`) to a file. The diff's file headers are ignored; only the hunks are applied to the given path."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to modify, relative to the working directory."},
                "diff": {"type": "string", "description": "Unified diff text (diff -u format) with the hunks to apply."}
            },
            "required": ["path", "diff"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn confirmation_description(&self, args: &Value, _ctx: &ToolContext) -> String {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let diff = args.get("diff").and_then(Value::as_str).unwrap_or("");
        format!("edit_file {path}:\n{diff}")
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: EditFileParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let full_path = ctx.cwd.join(&params.path);
        super::snapshot_before_write(ctx, &full_path);

        let original = std::fs::read_to_string(&full_path)?;
        let uses_crlf = original.contains("\r\n");
        let normalized = original.replace("\r\n", "\n");
        let diff_text = params.diff.replace("\r\n", "\n");

        let patch = diffy::Patch::from_str(&diff_text).map_err(|e| ToolError::InvalidArgs(format!("invalid unified diff: {e}")))?;
        let patched = diffy::apply(&normalized, &patch).map_err(|e| ToolError::Execution(format!("diff did not apply cleanly: {e}")))?;

        let final_content = if uses_crlf { patched.replace('\n', "\r\n") } else { patched };
        std::fs::write(&full_path, &final_content)?;

        Ok(format!("applied diff to {} ({} -> {} lines)", params.path, original.lines().count(), final_content.lines().count()))
    }
}
