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

/// Maps visually-confusable Unicode characters an LLM might substitute when
/// copying text (smart quotes, en/em dashes, non-breaking/thin spaces) to
/// their plain-ASCII equivalent, one scalar value at a time. The mapping
/// never merges or drops characters, so a character *position* found in the
/// normalized string is always the same character position in the original
/// — only the byte length can differ, since some of these are multi-byte.
fn normalize_lookalikes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201F}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00A0}' | '\u{2009}' | '\u{202F}' => ' ',
            other => other,
        })
        .collect()
}

fn char_index_to_byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// Finds `old_string`'s unique occurrence in `content` and returns the byte
/// span (in `content`'s own coordinates) to replace. Tries an exact match
/// first; if that finds nothing, falls back to a Unicode-lookalike
/// normalized match — LLMs reliably copy text verbatim but sometimes
/// substitute a smart quote or em-dash for its plain-ASCII look-alike.
/// Ambiguous matches (any count > 1) are never retried via fuzzy matching,
/// since normalizing more characters can only make ambiguity worse.
fn find_match(content: &str, old_string: &str) -> Result<(usize, usize), ToolError> {
    let exact: Vec<usize> = content.match_indices(old_string).map(|(i, _)| i).collect();
    match exact.len() {
        1 => return Ok((exact[0], exact[0] + old_string.len())),
        n if n > 1 => {
            return Err(ToolError::InvalidArgs(format!(
                "old_string matches {n} locations in the file — add more surrounding context so it matches exactly once."
            )));
        }
        _ => {}
    }

    let normalized_content = normalize_lookalikes(content);
    let normalized_old = normalize_lookalikes(old_string);
    let fuzzy: Vec<usize> = normalized_content.match_indices(&normalized_old).map(|(i, _)| i).collect();
    match fuzzy.len() {
        1 => {
            let start_char = normalized_content[..fuzzy[0]].chars().count();
            let end_char = start_char + normalized_old.chars().count();
            Ok((char_index_to_byte_offset(content, start_char), char_index_to_byte_offset(content, end_char)))
        }
        n if n > 1 => Err(ToolError::InvalidArgs(format!(
            "old_string matches {n} locations in the file after normalizing look-alike Unicode characters (smart quotes/dashes) — add more surrounding context so it matches exactly once."
        ))),
        _ => Err(ToolError::InvalidArgs(
            "old_string not found in file — it must match the file's current content exactly. Re-read the file and try again.".to_string(),
        )),
    }
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

    fn prompt_guidelines(&self) -> Option<&str> {
        Some(
            "edit_file: keep old_string as short as possible while still unique — a single distinguishing line beats a large block. \
If a call fails as \"not found\", re-read the file first; don't guess or retry with the same text. If it fails as \"ambiguous\", add a \
line of real surrounding context rather than repeating the same snippet.",
        )
    }

    fn confirmation_description(&self, args: &Value, ctx: &ToolContext) -> String {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let old_string = args.get("old_string").and_then(Value::as_str).unwrap_or("");
        let new_string = args.get("new_string").and_then(Value::as_str).unwrap_or("");

        let Ok(original) = std::fs::read_to_string(ctx.cwd.join(path)) else {
            return format!("edit_file {path}: (file not readable)");
        };
        let Ok((start, end)) = find_match(&original, old_string) else {
            return format!("edit_file {path}: (old_string not found or ambiguous)");
        };
        let updated = format!("{}{}{}", &original[..start], new_string, &original[end..]);

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

        let (start, end) = find_match(&normalized, &old_string)?;
        let patched = format!("{}{}{}", &normalized[..start], new_string, &normalized[end..]);

        let final_content = if uses_crlf { patched.replace('\n', "\r\n") } else { patched };
        std::fs::write(&full_path, &final_content)?;

        Ok(format!("edited {} ({} -> {} lines)", params.path, original.lines().count(), final_content.lines().count()))
    }
}
