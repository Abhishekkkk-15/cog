use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::message::Message;
use crate::nodes::Node;
use crate::provider::{ChatRequest, Provider, ProviderError};
use crate::state::{AgentState, Event};

pub struct ReflectorNode {
    provider: Arc<dyn Provider>,
    model: String,
}

impl ReflectorNode {
    pub fn new(provider: Arc<dyn Provider>, model: String) -> Self {
        Self { provider, model }
    }

    async fn diagnose(&self, error: &str) -> Result<String, ProviderError> {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(
                    "You are debugging a failed code change. Given the verification error below, \
                     give a concise (~100 word) diagnosis of the likely cause and a concrete next step to fix it.",
                ),
                Message::user(error.to_string()),
            ],
            tools: None,
            tool_choice: None,
            stream: false,
            temperature: None,
            max_tokens: None,
        };
        let resp = self.provider.chat(&req).await?;
        Ok(resp.message.content.unwrap_or_else(|| "(model returned an empty diagnosis)".to_string()))
    }
}

#[async_trait::async_trait]
impl Node for ReflectorNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, _state: Arc<RwLock<AgentState>>) {
        while let Ok(event) = rx.recv().await {
            let Event::VerificationFailed { tid, error } = event else { continue };
            tracing::info!("ReflectorNode: analyzing failure for task {tid}");

            let reflection = match self.diagnose(&error).await {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!("ReflectorNode: provider call failed ({e}), falling back to raw error");
                    format!("Failed because: {error}")
                }
            };

            let _ = bus.publish(Event::ReflectionGenerated { tid, reflection });
        }
    }
}
