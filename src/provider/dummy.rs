use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::message::{Message, Role};

use super::{ChatRequest, ChatResponse, FinishReason, Provider, ProviderError, StreamEvent};

/// No-network provider for Phase 1-2 testing. With no script it echoes the
/// last user message back; with a script it pops one canned response per
/// call, letting tests simulate "call a tool, then answer" sequences
/// deterministically.
pub struct DummyProvider {
    scripted: Mutex<VecDeque<ChatResponse>>,
}

impl DummyProvider {
    pub fn echo() -> Self {
        DummyProvider { scripted: Mutex::new(VecDeque::new()) }
    }

    pub fn scripted(responses: Vec<ChatResponse>) -> Self {
        DummyProvider { scripted: Mutex::new(responses.into_iter().collect()) }
    }
}

#[async_trait]
impl Provider for DummyProvider {
    async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        if let Some(resp) = self.scripted.lock().unwrap().pop_front() {
            return Ok(resp);
        }

        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: Some(format!("You said: {last_user}")),
                ..Default::default()
            },
            finish_reason: FinishReason::Stop,
            usage: None,
        })
    }

    /// Overrides the trait's default (which only forwards `content`) so
    /// scripted tool calls reach streaming-only consumers like
    /// `ExecutorNode`, matching this provider's documented purpose.
    async fn chat_stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<(), ProviderError> {
        let resp = self.chat(req).await?;
        if let Some(content) = resp.message.content.clone() {
            let _ = tx.send(StreamEvent::ContentDelta(content));
        }
        for (index, call) in resp.message.tool_calls.iter().enumerate() {
            let _ = tx.send(StreamEvent::ToolCallDelta {
                index,
                id: Some(call.id.clone()),
                name: Some(call.name.clone()),
                arguments_delta: Some(call.arguments.clone()),
            });
        }
        let _ = tx.send(StreamEvent::Done { finish_reason: resp.finish_reason, usage: resp.usage });
        Ok(())
    }

    fn name(&self) -> &str {
        "dummy"
    }
}
