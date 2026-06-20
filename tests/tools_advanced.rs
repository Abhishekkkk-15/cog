use serde_json::json;

use cog::memory::MemoryManager;
use cog::tools::{ToolContext, ToolRegistry};

fn ctx(cwd: std::path::PathBuf) -> ToolContext {
    ToolContext { cwd, ui_tx: None, memory: None, snapshots: None }
}

struct SimpleMockEmbedder;

#[async_trait::async_trait]
impl cog::memory::Embedder for SimpleMockEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, cog::memory::EmbedError> {
        Ok(vec![vec![0.1; 1024]; texts.len()])
    }

    fn dimensions(&self) -> usize {
        1024
    }
}

#[tokio::test]
async fn memory_tools_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    
    // Create a MemoryManager with a SimpleMockEmbedder
    let db_path = tmp.path().join("mem.db");
    let embedder = std::sync::Arc::new(SimpleMockEmbedder);
    let manager = std::sync::Arc::new(tokio::sync::Mutex::new(
        MemoryManager::open(&db_path, embedder).unwrap()
    ));

    let mut context = ctx(tmp.path().to_path_buf());
    context.memory = Some(manager);

    let registry = ToolRegistry::new();
    let save_tool = registry.get("save_memory").unwrap();
    let recall_tool = registry.get("recall_memory").unwrap();

    // 1. Save a fact
    let save_res = save_tool.execute(json!({
        "key": "user_name",
        "value": "Alice",
    }), &context).await.unwrap();
    assert!(save_res.contains("saved fact 'user_name'"));

    // 2. Recall the fact
    let recall_res = recall_tool.execute(json!({
        "query": "Alice",
    }), &context).await.unwrap();
    assert!(recall_res.contains("user_name: Alice"));
}

#[tokio::test]
async fn semantic_replace_exact_match_success() {
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("test.rs");
    
    std::fs::write(&file_path, "fn old_func() {\n    println!(\"old\");\n}\n").unwrap();

    let context = ctx(tmp.path().to_path_buf());
    let registry = ToolRegistry::new();
    let replace_tool = registry.get("semantic_replace").unwrap();

    let result = replace_tool.execute(json!({
        "path": "test.rs",
        "symbol_name": "old_func",
        "old_signature": "fn old_func() {\n    println!(\"old\");\n}",
        "new_text": "fn new_func() {\n    println!(\"new\");\n}"
    }), &context).await.unwrap();

    assert!(result.contains("replaced symbol 'old_func'"));
    
    let updated = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(updated, "fn new_func() {\n    println!(\"new\");\n}\n");
}

#[tokio::test]
async fn semantic_replace_near_miss_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("test.rs");
    
    std::fs::write(&file_path, "fn target() {\n    let x = 1;\n}\n").unwrap();

    let context = ctx(tmp.path().to_path_buf());
    let registry = ToolRegistry::new();
    let replace_tool = registry.get("semantic_replace").unwrap();

    let err = replace_tool.execute(json!({
        "path": "test.rs",
        "symbol_name": "target",
        "old_signature": "fn target() {\n    let x = 2;\n}", // near miss
        "new_text": "fn target() {\n    let x = 3;\n}"
    }), &context).await.unwrap_err();

    assert!(err.to_string().contains("no symbol named 'target' with exact matching source text found"));
}

#[tokio::test]
async fn parse_compiler_errors_regex() {
    let output = "
error[E0425]: cannot find value `x` in this scope
 --> src/main.rs:2:5
  |
2 |     x + 1
  |     ^ not found in this scope
";
    // We export the parser specifically to unit test it
    use cog::tools::run_test_suite::parse_compiler_errors;
    let errors = parse_compiler_errors(output);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code.as_deref(), Some("E0425"));
    assert_eq!(errors[0].file.as_deref(), Some("src/main.rs"));
    assert_eq!(errors[0].line, Some(2));
    assert_eq!(errors[0].column, Some(5));
}
