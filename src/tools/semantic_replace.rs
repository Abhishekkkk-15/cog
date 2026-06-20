
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tree_sitter::Parser;

use super::{lang_spec_for, Tool, ToolContext, ToolError};

#[derive(Deserialize)]
struct SemanticReplaceParams {
    path: String,
    symbol_name: String,
    old_signature: String,
    new_text: String,
}

pub struct SemanticReplaceTool;

#[async_trait]
impl Tool for SemanticReplaceTool {
    fn name(&self) -> &str {
        "semantic_replace"
    }

    fn description(&self) -> &str {
        "Replace a named symbol (function, struct, class, etc.) in a source file using AST-aware matching. \
         Finds the symbol by name, verifies its exact source text matches old_signature, then splices in new_text. \
         Requires confirmation before writing."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path relative to the working directory."},
                "symbol_name": {"type": "string", "description": "The name of the symbol to replace (e.g. function name, struct name)."},
                "old_signature": {"type": "string", "description": "The EXACT current source text of the symbol — must match byte-for-byte."},
                "new_text": {"type": "string", "description": "The replacement source text."}
            },
            "required": ["path", "symbol_name", "old_signature", "new_text"]
        })
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn confirmation_description(&self, args: &Value, _ctx: &ToolContext) -> String {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
        let symbol = args.get("symbol_name").and_then(Value::as_str).unwrap_or("?");
        format!("semantic_replace: replace symbol '{symbol}' in {path}")
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: SemanticReplaceParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let abs_path = ctx.cwd.join(&params.path);
        let source = std::fs::read_to_string(&abs_path)?;

        let Some(lang) = lang_spec_for(&abs_path) else {
            return Err(ToolError::Execution(format!(
                "unsupported file type: {}",
                abs_path.display()
            )));
        };

        let mut parser = Parser::new();
        parser
            .set_language(&lang.language)
            .map_err(|e| ToolError::Execution(format!("parser setup failed: {e}")))?;

        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| ToolError::Execution("failed to parse file".into()))?;

        let match_result = find_symbol(
            tree.root_node(),
            &source,
            lang.symbol_kinds,
            &params.symbol_name,
            &params.old_signature,
        );

        let Some((start_byte, end_byte)) = match_result else {
            return Err(ToolError::Execution(format!(
                "no symbol named '{}' with exact matching source text found in {}. \
                 Please re-read the file to get the exact current text and try again.",
                params.symbol_name,
                params.path
            )));
        };

        let mut new_source = String::with_capacity(source.len() - (end_byte - start_byte) + params.new_text.len());
        new_source.push_str(&source[..start_byte]);
        new_source.push_str(&params.new_text);
        new_source.push_str(&source[end_byte..]);

        std::fs::write(&abs_path, &new_source)?;

        Ok(format!(
            "replaced symbol '{}' in {} ({} bytes → {} bytes)",
            params.symbol_name,
            params.path,
            end_byte - start_byte,
            params.new_text.len()
        ))
    }
}

fn find_symbol(
    node: tree_sitter::Node,
    source: &str,
    kinds: &[&str],
    symbol_name: &str,
    old_signature: &str,
) -> Option<(usize, usize)> {
    if kinds.contains(&node.kind()) {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Some(name) = source.get(name_node.byte_range()) {
                if name == symbol_name {
                    let span_text = &source[node.byte_range()];
                    if span_text == old_signature {
                        return Some((node.start_byte(), node.end_byte()));
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(result) = find_symbol(child, source, kinds, symbol_name, old_signature) {
            return Some(result);
        }
    }
    None
}
