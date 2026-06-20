use serde::{Deserialize, Serialize};

use crate::utils::estimate_tokens;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string as returned by the provider, not pre-parsed —
    /// callers deserialize it into the tool's own params struct.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

impl Default for Role {
    fn default() -> Self {
        Role::User
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Message {
            role: Role::Tool,
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    pub(crate) fn approx_tokens(&self) -> usize {
        let content_tokens = self.content.as_deref().map(estimate_tokens).unwrap_or(0);
        let tool_call_tokens: usize = self
            .tool_calls
            .iter()
            .map(|c| estimate_tokens(&c.arguments) + estimate_tokens(&c.name))
            .sum();
        content_tokens + tool_call_tokens
    }
}

#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub token_budget: usize,
    pub estimated_tokens: usize,
}

impl Conversation {
    pub fn new(token_budget: usize) -> Self {
        Conversation {
            messages: Vec::new(),
            token_budget,
            estimated_tokens: 0,
        }
    }

    pub fn push(&mut self, msg: Message) {
        self.estimated_tokens += msg.approx_tokens();
        self.messages.push(msg);
    }

    pub fn needs_summarization(&self) -> bool {
        // Stubbed off until the agent loop + a real provider both work end-to-end
        // (see plan risk callouts) — an unbounded-but-correct history beats a
        // buggy summarizer that silently drops context.
        false
    }

    /// Recomputes `estimated_tokens` from scratch — needed after an
    /// operation (e.g. compression) replaces a slice of `messages` directly
    /// rather than going through `push`, which maintains the running total
    /// incrementally.
    pub fn recompute_estimate(&mut self) {
        self.estimated_tokens = self.messages.iter().map(Message::approx_tokens).sum();
    }
}
