use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::WalkDir;

use super::{Tool, ToolContext, ToolError};

const MAX_ENTRIES: usize = 500;

fn default_path() -> String {
    ".".to_string()
}

#[derive(Deserialize)]
struct ListDirParams {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    recursive: bool,
}

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List directory contents, optionally recursively. Directories are suffixed with '/'."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path, relative to the working directory. Defaults to '.'."},
                "recursive": {"type": "boolean", "description": "If true, recursively list subdirectories."}
            }
        })
    }

    // See read_file's execute() for why this runs via spawn_blocking rather
    // than inline: directory walking has no await point of its own, so it
    // wouldn't actually overlap with other tool calls in the same round
    // without being moved onto a real OS thread.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: ListDirParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let cwd = ctx.cwd.clone();
        tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
            let root = cwd.join(&params.path);
            let mut lines = Vec::new();

            if params.recursive {
                for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                    if lines.len() >= MAX_ENTRIES {
                        break;
                    }
                    let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                    if rel.as_os_str().is_empty() {
                        continue;
                    }
                    let marker = if entry.file_type().is_dir() { "/" } else { "" };
                    lines.push(format!("{}{marker}", rel.display()));
                }
            } else {
                let mut entries: Vec<_> = std::fs::read_dir(&root)?.filter_map(|e| e.ok()).collect();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    if lines.len() >= MAX_ENTRIES {
                        break;
                    }
                    let marker = if entry.path().is_dir() { "/" } else { "" };
                    lines.push(format!("{}{marker}", entry.file_name().to_string_lossy()));
                }
            }

            let truncated = lines.len() >= MAX_ENTRIES;
            let mut out = lines.join("\n");
            if truncated {
                out.push_str("\n... [truncated]");
            }
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
    }
}
