use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::memory::MemoryManager;
use crate::message::Message;
use crate::nodes::{recv_lossy, Node};
use crate::state::{AgentState, Event};

const MAX_CHUNK_CHARS: usize = 800;

pub struct ContextNode {
    memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
}

impl ContextNode {
    pub fn new(memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>) -> Self {
        Self { memory }
    }
}

#[async_trait::async_trait]
impl Node for ContextNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        while let Some(event) = recv_lossy(&mut rx, "ContextNode").await {
            let Event::TaskStarted(tid) = event else { continue };
            tracing::info!("ContextNode: gathering context for task {tid}");

            if let Some(memory) = &self.memory {
                let description = {
                    let st = state.read().await;
                    st.plan.milestones.iter().flat_map(|m| m.tasks.iter()).find(|t| t.id == tid).map(|t| t.description.clone())
                };

                if let Some(description) = description {
                    if let Some(summary) = gather_context(memory, &description).await {
                        let mut st = state.write().await;
                        st.conversation.push(Message::system(summary));
                    }
                }
            }

            let _ = bus.publish(Event::ContextReady(tid));
        }
    }
}

/// Pulls relevant facts and indexed code for `query` (the task description)
/// out of memory and formats them as one system message, or returns `None`
/// if both come back empty — no point injecting an empty "here's nothing
/// relevant" note into every single task.
async fn gather_context(memory: &Arc<tokio::sync::Mutex<MemoryManager>>, query: &str) -> Option<String> {
    let mem = memory.lock().await;
    let facts = mem.recall(query, 5).await.unwrap_or_default();
    let chunks = mem.semantic_search(query, 5).await.unwrap_or_default();
    drop(mem);

    if facts.is_empty() && chunks.is_empty() {
        return None;
    }

    let mut summary = String::from("Relevant context retrieved from memory for this task:\n");
    if !facts.is_empty() {
        summary.push_str("\nFacts:\n");
        for (key, value) in &facts {
            summary.push_str(&format!("- {key}: {value}\n"));
        }
    }
    if !chunks.is_empty() {
        summary.push_str("\nCode:\n");
        for chunk in &chunks {
            let text = if chunk.chunk_text.len() > MAX_CHUNK_CHARS { format!("{}... [truncated]", &chunk.chunk_text[..MAX_CHUNK_CHARS]) } else { chunk.chunk_text.clone() };
            summary.push_str(&format!("- {}:{}-{}\n{text}\n", chunk.file_path, chunk.start_line, chunk.end_line));
        }
    }
    Some(summary)
}
