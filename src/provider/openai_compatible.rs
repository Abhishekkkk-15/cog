use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::message::{Message, Role, ToolCall};

use super::{ChatRequest, ChatResponse, FinishReason, Provider, ProviderError, ProviderQuirks, StreamEvent, Usage};

use async_trait::async_trait;

/// Shared HTTP implementation for any backend exposing an OpenAI-compatible
/// `/chat/completions` endpoint. Mistral/Groq/NVIDIA/OpenAI/custom all build
/// one of these with a different base_url/api_key/ProviderQuirks rather than
/// duplicating HTTP logic.
pub struct OpenAiCompatible {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    quirks: ProviderQuirks,
    provider_name: String,
}

impl OpenAiCompatible {
    pub fn new(provider_name: impl Into<String>, base_url: impl Into<String>, api_key: Option<String>, quirks: ProviderQuirks) -> Self {
        OpenAiCompatible { client: reqwest::Client::new(), base_url: base_url.into(), api_key, quirks, provider_name: provider_name.into() }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Converts our internal `Message`/`ToolCall` into the OpenAI wire shape
    /// (which nests tool calls under `{type:"function", function:{...}}`,
    /// unlike our flat internal `ToolCall`). Built by hand rather than via
    /// `Message`'s own `Serialize` impl, which intentionally stays
    /// provider-agnostic.
    fn wire_message(&self, msg: &Message) -> Value {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };

        let mut obj = serde_json::Map::new();
        obj.insert("role".into(), json!(role));
        obj.insert("content".into(), msg.content.clone().map(Value::String).unwrap_or(Value::Null));

        if !msg.tool_calls.is_empty() {
            let calls: Vec<Value> = msg
                .tool_calls
                .iter()
                .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                .collect();
            obj.insert("tool_calls".into(), Value::Array(calls));
        }
        if let Some(id) = &msg.tool_call_id {
            obj.insert("tool_call_id".into(), json!(id));
        }
        if self.quirks.send_tool_message_name_field {
            if let Some(name) = &msg.name {
                obj.insert("name".into(), json!(name));
            }
        }
        Value::Object(obj)
    }

    fn build_body(&self, req: &ChatRequest, stream: bool) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(|m| self.wire_message(m)).collect();
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": stream,
        });
        if let Some(tools) = &req.tools {
            body["tools"] = serde_json::to_value(tools).unwrap_or(Value::Null);
        }
        if let Some(choice) = &req.tool_choice {
            body["tool_choice"] = serde_json::to_value(choice).unwrap_or(Value::Null);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = json!(m);
        }
        body
    }

    fn request(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut request = self.client.post(self.endpoint()).json(body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        request
    }

    async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let message = resp.text().await.unwrap_or_default();
        Err(ProviderError::Api { status: status.as_u16(), message })
    }

    fn parse_finish_reason(raw: Option<&str>, has_tool_calls: bool) -> FinishReason {
        match raw {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_string()),
            None if has_tool_calls => FinishReason::ToolCalls,
            None => FinishReason::Stop,
        }
    }

    fn parse_usage(value: &Value) -> Option<Usage> {
        let usage = value.get("usage")?;
        Some(Usage {
            prompt_tokens: usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            completion_tokens: usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            total_tokens: usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        })
    }

    fn parse_completion(&self, body: Value) -> Result<ChatResponse, ProviderError> {
        let choice = body.get("choices").and_then(|c| c.get(0)).ok_or_else(|| ProviderError::Parse("missing choices[0]".into()))?;
        let message = choice.get("message").cloned().unwrap_or(Value::Null);
        let content = message.get("content").and_then(Value::as_str).map(str::to_string);

        let mut tool_calls = Vec::new();
        if let Some(arr) = message.get("tool_calls").and_then(Value::as_array) {
            for tc in arr {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let func = tc.get("function");
                let name = func.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default().to_string();
                let arguments = func.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}").to_string();
                tool_calls.push(ToolCall { id, name, arguments });
            }
        }
        if !self.quirks.supports_parallel_tool_calls && tool_calls.len() > 1 {
            tool_calls.truncate(1);
        }

        let finish_reason = Self::parse_finish_reason(choice.get("finish_reason").and_then(Value::as_str), !tool_calls.is_empty());
        let usage = Self::parse_usage(&body);

        Ok(ChatResponse { message: Message { role: Role::Assistant, content, tool_calls, tool_call_id: None, name: None }, finish_reason, usage })
    }
}

#[async_trait]
impl Provider for OpenAiCompatible {
    async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        let body = self.build_body(req, false);
        let resp = self.request(&body).send().await?;
        let resp = Self::error_for_status(resp).await?;
        let body: Value = resp.json().await.map_err(|e| ProviderError::Parse(e.to_string()))?;
        self.parse_completion(body)
    }

    async fn chat_stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<(), ProviderError> {
        let body = self.build_body(req, true);
        let resp = self.request(&body).send().await?;
        let resp = Self::error_for_status(resp).await?;

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut final_finish_reason = FinishReason::Stop;
        let mut final_usage = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buf.find("\n\n") {
                let event: String = buf.drain(..pos + 2).collect();

                for line in event.lines() {
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }

                    tracing::debug!(provider = %self.provider_name, raw = %data, "sse chunk");

                    let Ok(chunk_json) = serde_json::from_str::<Value>(data) else { continue };
                    let Some(choice) = chunk_json.get("choices").and_then(|c| c.get(0)) else { continue };
                    let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

                    if let Some(content) = delta.get("content").and_then(Value::as_str) {
                        let _ = tx.send(StreamEvent::ContentDelta(content.to_string()));
                    }

                    if let Some(arr) = delta.get("tool_calls").and_then(Value::as_array) {
                        for (position, tc) in arr.iter().enumerate() {
                            // Groq has been reported to omit `index` on some
                            // chunks — fall back to array position rather
                            // than panicking or dropping the delta.
                            let index = tc.get("index").and_then(Value::as_u64).map(|v| v as usize).unwrap_or(position);
                            let id = tc.get("id").and_then(Value::as_str).map(str::to_string);
                            let func = tc.get("function");
                            let name = func.and_then(|f| f.get("name")).and_then(Value::as_str).map(str::to_string);
                            let arguments_delta = func.and_then(|f| f.get("arguments")).and_then(Value::as_str).map(str::to_string);
                            let _ = tx.send(StreamEvent::ToolCallDelta { index, id, name, arguments_delta });
                        }
                    }

                    if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                        final_finish_reason = Self::parse_finish_reason(Some(fr), false);
                    }
                    if let Some(usage) = Self::parse_usage(&chunk_json) {
                        final_usage = Some(usage);
                    }
                }
            }
        }

        let _ = tx.send(StreamEvent::Done { finish_reason: final_finish_reason, usage: final_usage });
        Ok(())
    }

    fn name(&self) -> &str {
        &self.provider_name
    }
}
