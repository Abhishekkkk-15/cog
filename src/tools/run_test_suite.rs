use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_CHARS: usize = 12000;

#[derive(Deserialize)]
struct RunTestSuiteParams {
    command: Option<String>,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}

pub struct RunTestSuiteTool;

#[async_trait]
impl Tool for RunTestSuiteTool {
    fn name(&self) -> &str {
        "run_test_suite"
    }

    fn description(&self) -> &str {
        "Run the project's test suite (defaults to `cargo test`). Captures output and \
         parses compiler errors into structured JSON for easier debugging. Returns both \
         the raw output and any parsed errors."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Test command to run. Defaults to 'cargo test'."},
                "cwd": {"type": "string", "description": "Optional working directory, relative to the agent's working directory."},
                "timeout_secs": {"type": "integer", "description": "Optional timeout in seconds (default 120)."}
            },
            "required": []
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn confirmation_description(&self, args: &Value, _ctx: &ToolContext) -> String {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("cargo test");
        format!("run_test_suite: {command}")
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: RunTestSuiteParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let command = params.command.as_deref().unwrap_or("cargo test");
        let work_dir = match &params.cwd {
            Some(c) => ctx.cwd.join(c),
            None => ctx.cwd.clone(),
        };
        let timeout = Duration::from_secs(params.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut cmd = build_shell_command(command);
        cmd.current_dir(&work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn()?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(ToolError::Execution(e.to_string())),
            Err(_) => {
                return Err(ToolError::Execution(format!(
                    "test suite timed out after {}s",
                    timeout.as_secs()
                )))
            }
        };

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        let errors = parse_compiler_errors(&combined);
        let exit_status = output.status;

        let mut result = String::new();

        if !errors.is_empty() {
            let errors_json = serde_json::to_string_pretty(&errors).unwrap_or_default();
            result.push_str(&format!(
                "## Compiler Errors ({} found)\n```json\n{}\n```\n\n",
                errors.len(),
                errors_json
            ));
        }

        result.push_str(&format!("## Raw Output\n```\n"));
        if combined.len() > MAX_OUTPUT_CHARS {
            result.push_str(&combined[..MAX_OUTPUT_CHARS]);
            result.push_str("\n... [truncated]");
        } else {
            result.push_str(&combined);
        }
        result.push_str(&format!("\n```\n\n--- exit status: {exit_status} ---"));

        Ok(result)
    }
}

#[derive(Debug, serde::Serialize)]
pub struct CompilerError {
    pub code: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Parses cargo's error output for `error[E0XXX]: message` lines followed
/// by `--> file:line:col` location lines.
pub fn parse_compiler_errors(output: &str) -> Vec<CompilerError> {
    let error_re = Regex::new(r"error(?:\[(E\d+)\])?: (.+)").unwrap();
    let location_re = Regex::new(r"^\s*--> (.+):(\d+):(\d+)").unwrap();

    let lines: Vec<&str> = output.lines().collect();
    let mut errors = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = error_re.captures(line) {
            let code = caps.get(1).map(|m| m.as_str().to_string());
            let message = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();

            let mut file = None;
            let mut line_num = None;
            let mut column = None;

            // Look at the next few lines for a location marker
            for j in (i + 1)..lines.len().min(i + 5) {
                if let Some(loc) = location_re.captures(lines[j]) {
                    file = loc.get(1).map(|m| m.as_str().to_string());
                    line_num = loc.get(2).and_then(|m| m.as_str().parse().ok());
                    column = loc.get(3).and_then(|m| m.as_str().parse().ok());
                    break;
                }
            }

            errors.push(CompilerError {
                code,
                message,
                file,
                line: line_num,
                column,
            });
        }
    }

    errors
}

#[cfg(windows)]
pub(crate) fn build_shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(windows))]
pub(crate) fn build_shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}
