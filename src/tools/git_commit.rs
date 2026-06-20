use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

#[derive(Deserialize)]
struct GitCommitParams {
    message: String,
    #[serde(default)]
    paths: Vec<String>,
}

pub struct GitCommitTool;

#[async_trait]
impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }

    fn description(&self) -> &str {
        "Stage and commit changes with a message."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string", "description": "The commit message."},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Specific paths to stage. Defaults to all changes ('.') if omitted."}
            },
            "required": ["message"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn confirmation_description(&self, args: &Value, _ctx: &ToolContext) -> String {
        let message = args.get("message").and_then(Value::as_str).unwrap_or("?");
        format!("git commit -m \"{message}\"")
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: GitCommitParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let add_targets: Vec<String> = if params.paths.is_empty() { vec![".".to_string()] } else { params.paths };

        let add_output = tokio::process::Command::new("git").arg("add").args(&add_targets).current_dir(&ctx.cwd).output().await?;
        if !add_output.status.success() {
            return Err(ToolError::Execution(format!("git add failed: {}", String::from_utf8_lossy(&add_output.stderr))));
        }

        let commit_output = tokio::process::Command::new("git").args(["commit", "-m", &params.message]).current_dir(&ctx.cwd).output().await?;
        if !commit_output.status.success() {
            return Err(ToolError::Execution(format!("git commit failed: {}", String::from_utf8_lossy(&commit_output.stderr))));
        }
        Ok(String::from_utf8_lossy(&commit_output.stdout).into_owned())
    }
}
