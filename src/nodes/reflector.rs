use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::message::Message;
use crate::nodes::{recv_lossy, Node};
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
                    "You are reviewing why a coding agent's task failed verification. The failure could be a \
                     code/build error (a compiler or test error), or a judgment that the agent's response didn't \
                     actually accomplish what the task asked for (e.g. it only described or talked instead of \
                     acting, or skipped a requested step). Given the failure reason below, give a concise \
                     (~100 word) diagnosis of why it likely happened and a concrete suggestion for what to do \
                     differently on retry. Don't assume there's a code diff to review unless the failure reason \
                     itself mentions one.",
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
        while let Some(event) = recv_lossy(&mut rx, "ReflectorNode").await {
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
