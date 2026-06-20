use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

use super::{Tool, ToolContext, ToolError};

const MAX_DIFF_CHARS: usize = 4000;

#[derive(Deserialize)]
struct EditFileParams {
    path: String,
    old_string: String,
    new_string: String,
}

pub struct EditFileTool;

/// Finds `old_string` in `content`, requiring exactly one match. Returns the
/// byte offset of that match, or an error telling the model how to fix its
/// call — re-read the file (stale content) or add more surrounding context
/// (ambiguous match) — rather than guessing which occurrence was meant.
fn find_unique_match<'a>(content: &'a str, old_string: &str) -> Result<usize, ToolError> {
    let mut matches = content.match_indices(old_string);
    let Some((first, _)) = matches.next() else {
        return Err(ToolError::InvalidArgs(
            "old_string not found in file — it must match the file's current content exactly. Re-read the file and try again.".to_string(),
        ));
    };
    if matches.next().is_some() {
        return Err(ToolError::InvalidArgs(
            "old_string matches multiple locations in the file — add more surrounding context so it matches exactly once.".to_string(),
        ));
    }
    Ok(first)
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a file with new text. `old_string` must match the file's current content exactly and occur exactly once — if it doesn't match or matches more than once, the call fails with an error instead of guessing."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to modify, relative to the working directory."},
                "old_string": {"type": "string", "description": "Exact text to replace. Must match the file's current content exactly and occur exactly once; include enough surrounding context to make it unique."},
                "new_string": {"type": "string", "description": "Text to replace old_string with."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn confirmation_description(&self, args: &Value, ctx: &ToolContext) -> String {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let old_string = args.get("old_string").and_then(Value::as_str).unwrap_or("");
        let new_string = args.get("new_string").and_then(Value::as_str).unwrap_or("");

        let Ok(original) = std::fs::read_to_string(ctx.cwd.join(path)) else {
            return format!("edit_file {path}: (file not readable)");
        };
        let Ok(offset) = find_unique_match(&original, old_string) else {
            return format!("edit_file {path}: (old_string not found or ambiguous)");
        };
        let updated = format!("{}{}{}", &original[..offset], new_string, &original[offset + old_string.len()..]);

        let diff = TextDiff::from_lines(&original, &updated);
        let mut out = format!("edit_file {path}:\n");
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
        let params: EditFileParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let full_path = ctx.cwd.join(&params.path);
        super::snapshot_before_write(ctx, &full_path);

        let original = std::fs::read_to_string(&full_path)?;
        let uses_crlf = original.contains("\r\n");
        let normalized = original.replace("\r\n", "\n");
        let old_string = params.old_string.replace("\r\n", "\n");
        let new_string = params.new_string.replace("\r\n", "\n");

        let offset = find_unique_match(&normalized, &old_string)?;
        let patched = format!("{}{}{}", &normalized[..offset], new_string, &normalized[offset + old_string.len()..]);

        let final_content = if uses_crlf { patched.replace('\n', "\r\n") } else { patched };
        std::fs::write(&full_path, &final_content)?;

        Ok(format!("edited {} ({} -> {} lines)", params.path, original.lines().count(), final_content.lines().count()))
    }
}
