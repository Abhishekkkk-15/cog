pub mod custom;
mod dummy;
pub mod groq;
pub mod mistral;
pub mod nvidia;
pub mod openai;
pub mod openai_compatible;

pub use dummy::DummyProvider;
pub use openai_compatible::OpenAiCompatible;

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::message::{Message, Role, ToolCall};

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Incremental event surfaced during a streaming call.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, arguments_delta: Option<String> },
    Done { finish_reason: FinishReason, usage: Option<Usage> },
}

/// Per-provider deviations from the textbook OpenAI-compatible wire format,
/// applied defensively rather than assumed away.
#[derive(Debug, Clone, Default)]
pub struct ProviderQuirks {
    pub supports_tool_choice_required: bool,
    pub supports_parallel_tool_calls: bool,
    pub streaming_omits_index: bool,
    pub send_tool_message_name_field: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("response parse error: {0}")]
    Parse(String),
    #[error("stream error: {0}")]
    Stream(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError>;

    /// Default falls back to a single non-streaming call surfaced as one `Done` event,
    /// for providers/cases that don't support streaming.
    async fn chat_stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<(), ProviderError> {
        let resp = self.chat(req).await?;
        if let Some(content) = resp.message.content.clone() {
            let _ = tx.send(StreamEvent::ContentDelta(content));
        }
        let _ = tx.send(StreamEvent::Done { finish_reason: resp.finish_reason, usage: resp.usage });
        Ok(())
    }

    fn name(&self) -> &str;
}

/// Drives `chat_stream` to completion and reconstructs a `ChatResponse` from
/// the accumulated deltas, keyed by `index` per the streaming tool-call
/// accumulation rules (never parse partial `arguments` JSON mid-stream).
/// Used by tests and by any headless caller that wants a streaming call's
/// final result without incremental rendering.
pub async fn collect_stream(provider: &dyn Provider, req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    provider.chat_stream(req, tx).await?;

    let mut content = String::new();
    let mut tool_calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut finish_reason = FinishReason::Stop;
    let mut usage = None;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::ContentDelta(delta) => content.push_str(&delta),
            StreamEvent::ToolCallDelta { index, id, name, arguments_delta } => {
                let entry = tool_calls.entry(index).or_default();
                if let Some(id) = id {
                    entry.0 = id;
                }
                if let Some(name) = name {
                    entry.1 = name;
                }
                if let Some(delta) = arguments_delta {
                    entry.2.push_str(&delta);
                }
            }
            StreamEvent::Done { finish_reason: fr, usage: u } => {
                finish_reason = fr;
                usage = u;
            }
        }
    }

    let tool_calls = tool_calls.into_values().map(|(id, name, arguments)| ToolCall { id, name, arguments }).collect::<Vec<_>>();

    Ok(ChatResponse {
        message: Message { role: Role::Assistant, content: if content.is_empty() { None } else { Some(content) }, tool_calls, tool_call_id: None, name: None },
        finish_reason,
        usage,
    })
}
