use std::path::Path;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::WalkDir;

use super::{Tool, ToolContext, ToolError};

const MAX_MATCHES: usize = 200;
const IGNORED_DIR_NAMES: &[&str] = &["target", ".git", "node_modules"];

fn default_path() -> String {
    ".".to_string()
}

#[derive(Deserialize)]
struct SearchRegexParams {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    file_glob: Option<String>,
}

pub struct SearchRegexTool;

#[async_trait]
impl Tool for SearchRegexTool {
    fn name(&self) -> &str {
        "search_regex"
    }

    fn description(&self) -> &str {
        "Search files under a path for a regex pattern, like grep -r. Returns matching `path:line: text` entries."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for."},
                "path": {"type": "string", "description": "Directory or file to search, relative to the working directory. Defaults to '.'."},
                "file_glob": {"type": "string", "description": "Optional glob (e.g. '*.rs') to restrict which files are searched."}
            },
            "required": ["pattern"]
        })
    }

    // See read_file's execute() for why this runs via spawn_blocking rather
    // than inline: walking a directory tree and reading each file has no
    // await point of its own, so it wouldn't actually overlap with other
    // tool calls in the same round without being moved onto a real OS
    // thread — and this is the one most likely to take real wall-clock
    // time on a large tree, where that matters most.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: SearchRegexParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let pattern = Regex::new(&params.pattern).map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
        let glob = params.file_glob.as_deref().map(glob_to_regex).transpose().map_err(|e| ToolError::InvalidArgs(format!("invalid file_glob: {e}")))?;
        let cwd = ctx.cwd.clone();

        tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
            let root = cwd.join(&params.path);
            let mut matches = Vec::new();
            'walk: for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() || is_ignored(entry.path()) {
                    continue;
                }
                if let Some(glob) = &glob {
                    if !glob.is_match(&entry.file_name().to_string_lossy()) {
                        continue;
                    }
                }
                let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
                let rel = entry.path().strip_prefix(&cwd).unwrap_or(entry.path()).to_path_buf();

                for (i, line) in content.lines().enumerate() {
                    if pattern.is_match(line) {
                        matches.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
                        if matches.len() >= MAX_MATCHES {
                            break 'walk;
                        }
                    }
                }
            }

            if matches.is_empty() {
                return Ok("no matches".to_string());
            }
            let truncated = matches.len() >= MAX_MATCHES;
            let mut out = matches.join("\n");
            if truncated {
                out.push_str("\n... [truncated]");
            }
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
    }
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str().to_str().map(|s| IGNORED_DIR_NAMES.contains(&s)).unwrap_or(false))
}

fn glob_to_regex(glob: &str) -> Result<Regex, regex::Error> {
    let mut pattern = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            c if "\\.+^$()[]{}|".contains(c) => {
                pattern.push('\\');
                pattern.push(c);
            }
            c => pattern.push(c),
        }
    }
    pattern.push('$');
    Regex::new(&pattern)
}
