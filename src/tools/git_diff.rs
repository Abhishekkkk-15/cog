use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

#[derive(Deserialize)]
struct GitDiffParams {
    path: Option<String>,
    #[serde(default)]
    staged: bool,
}

pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show the current git diff (working tree, or staged with `staged: true`), optionally restricted to a path."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Optional path to restrict the diff to."},
                "staged": {"type": "boolean", "description": "If true, show 'git diff --staged' instead of the working tree diff."}
            }
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: GitDiffParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let mut cmd_args = vec!["diff".to_string()];
        if params.staged {
            cmd_args.push("--staged".to_string());
        }
        if let Some(path) = &params.path {
            cmd_args.push("--".to_string());
            cmd_args.push(path.clone());
        }

        let output = tokio::process::Command::new("git").args(&cmd_args).current_dir(&ctx.cwd).output().await?;
        if !output.status.success() {
            return Err(ToolError::Execution(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(if text.is_empty() { "no changes".to_string() } else { text })
    }
}
