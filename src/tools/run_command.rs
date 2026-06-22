use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_CHARS: usize = 8000;

#[derive(Deserialize)]
struct RunCommandParams {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    env: HashMap<String, String>,
}

pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command with an optional working directory, environment variables, and timeout (default 30s)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command line to execute."},
                "cwd": {"type": "string", "description": "Optional working directory, relative to the agent's working directory."},
                "timeout_secs": {"type": "integer", "description": "Optional timeout in seconds (default 30)."},
                "env": {"type": "object", "description": "Optional extra environment variables.", "additionalProperties": {"type": "string"}}
            },
            "required": ["command"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn prompt_guidelines(&self) -> Option<&str> {
        Some("run_command: avoid long-running or interactive commands (servers, watchers, REPLs, anything waiting on stdin) — they will hit the timeout and waste a round instead of completing.")
    }

    fn confirmation_description(&self, args: &Value, _ctx: &ToolContext) -> String {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("?");
        match args.get("cwd").and_then(Value::as_str) {
            Some(cwd) => format!("run_command: {command}  (cwd: {cwd})"),
            None => format!("run_command: {command}"),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: RunCommandParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let work_dir = match &params.cwd {
            Some(c) => ctx.cwd.join(c),
            None => ctx.cwd.clone(),
        };

        let mut cmd = build_shell_command(&params.command);
        cmd.current_dir(&work_dir).envs(&params.env).stdout(Stdio::piped()).stderr(Stdio::piped());

        let timeout = Duration::from_secs(params.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let child = cmd.spawn()?;

        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(ToolError::Execution(e.to_string())),
            Err(_) => return Err(ToolError::Execution(format!("command timed out after {}s", timeout.as_secs()))),
        };

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            combined.push_str("\n--- stderr ---\n");
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        combined.push_str(&format!("\n--- exit status: {} ---", output.status));

        if combined.len() > MAX_OUTPUT_CHARS {
            combined.truncate(MAX_OUTPUT_CHARS);
            combined.push_str("\n... [truncated]");
        }
        Ok(combined)
    }
}

#[cfg(windows)]
fn build_shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(windows))]
fn build_shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}
