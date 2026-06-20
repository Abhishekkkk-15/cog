use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tree_sitter::{Node, Parser};
use walkdir::WalkDir;

use super::{lang_spec_for, Tool, ToolContext, ToolError};

const MAX_RESULTS: usize = 100;
const IGNORED_DIR_NAMES: &[&str] = &["target", ".git", "node_modules"];

fn default_path() -> String {
    ".".to_string()
}

#[derive(Deserialize)]
struct SearchSemanticParams {
    query: String,
    #[serde(default = "default_path")]
    path: String,
}

pub struct SearchSemanticTool;

#[async_trait]
impl Tool for SearchSemanticTool {
    fn name(&self) -> &str {
        "search_semantic"
    }

    fn description(&self) -> &str {
        "Search Rust/Python/JavaScript source for function/class/struct definitions whose name contains the query. AST-based structural matching, not embeddings."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Substring to match against symbol names (functions, classes, structs, impls)."},
                "path": {"type": "string", "description": "Directory to search, relative to the working directory. Defaults to '.'."}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: SearchSemanticParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let root = ctx.cwd.join(&params.path);
        let mut results = Vec::new();

        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if results.len() >= MAX_RESULTS {
                break;
            }
            if !entry.file_type().is_file() || is_ignored(entry.path()) {
                continue;
            }
            let Some(lang) = lang_spec_for(entry.path()) else { continue };
            let Ok(source) = std::fs::read_to_string(entry.path()) else { continue };

            let mut parser = Parser::new();
            if parser.set_language(&lang.language).is_err() {
                continue;
            }
            let Some(tree) = parser.parse(&source, None) else { continue };

            let rel = entry.path().strip_prefix(&ctx.cwd).unwrap_or(entry.path()).to_path_buf();
            collect_symbols(tree.root_node(), &source, lang.symbol_kinds, &params.query, &rel, &mut results);
        }

        if results.is_empty() {
            return Ok("no matching symbols found".to_string());
        }
        Ok(results.join("\n"))
    }
}

fn is_ignored(path: &std::path::Path) -> bool {
    path.components().any(|c| c.as_os_str().to_str().map(|s| IGNORED_DIR_NAMES.contains(&s)).unwrap_or(false))
}

fn collect_symbols(node: Node, source: &str, kinds: &[&str], query: &str, rel_path: &PathBuf, out: &mut Vec<String>) {
    if out.len() >= MAX_RESULTS {
        return;
    }
    if kinds.contains(&node.kind()) {
        if let Some(name) = node.child_by_field_name("name").and_then(|n| source.get(n.byte_range())) {
            if name.contains(query) {
                let start = node.start_position().row + 1;
                let end = node.end_position().row + 1;
                out.push(format!("{}:{start}-{end} {} {name}", rel_path.display(), node.kind()));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(child, source, kinds, query, rel_path, out);
    }
}
