use std::sync::Arc;

use async_trait::async_trait;
use tempfile::tempdir;

use cog::memory::{EmbedError, Embedder, MemoryManager};
use cog::message::{Conversation, Message, Role};
use cog::provider::{ChatResponse, DummyProvider, FinishReason};

/// Deterministic, no-network embedder for tests — returns a fixed vector
/// per input string (hashed into a small set of basis directions) so
/// `MockEmbedder::embed` is reproducible across runs without touching the
/// real Mistral API.
struct MockEmbedder {
    dims: usize,
    overrides: std::collections::HashMap<String, Vec<f32>>,
}

impl MockEmbedder {
    fn new() -> Self {
        MockEmbedder { dims: 1024, overrides: std::collections::HashMap::new() }
    }

    fn with_override(mut self, text: &str, mut vector: Vec<f32>) -> Self {
        vector.resize(self.dims, 0.0);
        self.overrides.insert(text.to_string(), vector);
        self
    }

    fn hash_vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        let idx = text.len() % self.dims;
        v[idx] = 1.0;
        v
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| self.overrides.get(t).cloned().unwrap_or_else(|| self.hash_vector(t))).collect())
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

async fn open_test_manager() -> (tempfile::TempDir, MemoryManager) {
    let dir = tempdir().unwrap();
    let manager = MemoryManager::open(&dir.path().join("test.db"), Arc::new(MockEmbedder::new())).unwrap();
    (dir, manager)
}

#[tokio::test]
async fn open_is_idempotent_against_the_same_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    MemoryManager::open(&path, Arc::new(MockEmbedder::new())).unwrap();
    MemoryManager::open(&path, Arc::new(MockEmbedder::new())).unwrap();
}

#[tokio::test]
async fn save_message_and_load_recent_round_trip_in_order() {
    let (_dir, manager) = open_test_manager().await;
    manager.create_session("s1", "/project").await.unwrap();

    manager.save_message("s1", &Message::user("first")).await.unwrap();
    manager.save_message("s1", &Message::user("second")).await.unwrap();
    manager.save_message("s1", &Message::user("third")).await.unwrap();

    let recent = manager.load_recent("s1", 2).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].content.as_deref(), Some("second"));
    assert_eq!(recent[1].content.as_deref(), Some("third"));
}

#[tokio::test]
async fn list_sessions_returns_created_sessions() {
    let (_dir, manager) = open_test_manager().await;
    manager.create_session("a", "/proj-a").await.unwrap();
    manager.create_session("b", "/proj-b").await.unwrap();

    let sessions = manager.list_sessions().await.unwrap();
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"b"));
}

#[tokio::test]
async fn remember_and_recall_via_substring_match() {
    let (_dir, manager) = open_test_manager().await;
    manager.remember("lang", "this project uses Rust and tabs").await.unwrap();
    manager.remember("editor", "prefers vim keybindings").await.unwrap();

    let results = manager.recall("Rust", 5).await.unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "lang", "substring match should be ranked first");
    assert_eq!(results[1].0, "editor", "semantic match fills the rest of the limit");
}

#[tokio::test]
async fn recall_ranks_by_cosine_similarity_when_no_substring_match() {
    let dir = tempdir().unwrap();

    // remember() embeds the *value*, recall() embeds the *query* — one
    // embedder instance with all three overrides registered so both calls
    // see the vectors this test cares about.
    let embedder = MockEmbedder::new()
        .with_override("close fact", vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        .with_override("far fact", vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0])
        .with_override("query text", vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

    let manager = MemoryManager::open(&dir.path().join("test.db"), Arc::new(embedder)).unwrap();
    manager.remember("near", "close fact").await.unwrap();
    manager.remember("distant", "far fact").await.unwrap();

    let results = manager.recall("query text", 1).await.unwrap();
    assert_eq!(results.first().map(|(k, _)| k.as_str()), Some("near"));
}

#[tokio::test]
async fn delete_fact_removes_it_from_recall() {
    let (_dir, manager) = open_test_manager().await;
    manager.remember("temp", "a fact about Rust").await.unwrap();
    assert!(manager.recall("Rust", 5).await.unwrap().iter().any(|(k, _)| k == "temp"));

    let deleted = manager.delete_fact("temp").await.unwrap();
    assert!(deleted);
    assert!(!manager.recall("Rust", 5).await.unwrap().iter().any(|(k, _)| k == "temp"));
}

#[tokio::test]
async fn index_file_chunks_rust_source_and_semantic_search_finds_the_right_chunk() {
    let dir = tempdir().unwrap();
    let mut embedder = MockEmbedder::new()
        .with_override("fn alpha_function() {}", vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        .with_override("fn beta_function() {}", vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        .with_override("alpha query", vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

    let manager = MemoryManager::open(&dir.path().join("test.db"), Arc::new(embedder)).unwrap();

    let src_path = dir.path().join("lib.rs");
    std::fs::write(&src_path, "fn alpha_function() {}\nfn beta_function() {}\n").unwrap();

    let chunk_count = manager.index_file(&src_path).await.unwrap();
    assert_eq!(chunk_count, 2);

    let results = manager.semantic_search("alpha query", 1).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].chunk_text.contains("alpha_function"));
}

#[tokio::test]
async fn compress_if_needed_is_a_noop_below_threshold() {
    let (_dir, manager) = open_test_manager().await;
    let mut conversation = Conversation::new(100_000);
    conversation.push(Message::system("system prompt"));
    conversation.push(Message::user("hello"));

    let provider = DummyProvider::echo();
    let compressed = manager.compress_if_needed(&mut conversation, &provider, "test-model").await.unwrap();

    assert!(!compressed);
    assert_eq!(conversation.messages.len(), 2);
}

#[tokio::test]
async fn compress_if_needed_summarizes_the_middle_and_keeps_system_and_last_five() {
    let (_dir, manager) = open_test_manager().await;

    // A small budget makes it trivial to exceed 80% with a handful of
    // messages, without needing huge strings.
    let mut conversation = Conversation::new(10);
    conversation.push(Message::system("system prompt"));
    for i in 0..10 {
        conversation.push(Message::user(format!("message number {i} with enough text to add up tokens")));
    }

    let scripted = DummyProvider::scripted(vec![ChatResponse {
        message: Message { role: Role::Assistant, content: Some("condensed summary".into()), tool_calls: vec![], tool_call_id: None, name: None },
        finish_reason: FinishReason::Stop,
        usage: None,
    }]);

    let before_len = conversation.messages.len();
    let compressed = manager.compress_if_needed(&mut conversation, &scripted, "test-model").await.unwrap();

    assert!(compressed);
    assert!(conversation.messages.len() < before_len);
    assert_eq!(conversation.messages[0].role, Role::System);
    assert!(conversation.messages[0].content.as_deref().unwrap_or("").contains("system prompt"));

    let last_five: Vec<&str> = conversation.messages[conversation.messages.len() - 5..].iter().map(|m| m.content.as_deref().unwrap_or("")).collect();
    assert!(last_five.iter().any(|c| c.contains("message number 9")));

    let has_summary = conversation.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("condensed summary"));
    assert!(has_summary);
}
