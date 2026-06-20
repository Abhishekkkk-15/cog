use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::nodes::Node;
use crate::state::{AgentState, Event};

pub struct ContextNode;

#[async_trait::async_trait]
impl Node for ContextNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, _state: Arc<RwLock<AgentState>>) {
        while let Ok(event) = rx.recv().await {
            if let Event::TaskStarted(tid) = event {
                tracing::info!("ContextNode: Gathering context for task {}", tid);
                // In a real implementation, we query MemoryManager here.
                let _ = bus.publish(Event::ContextReady(tid));
            }
        }
    }
}
