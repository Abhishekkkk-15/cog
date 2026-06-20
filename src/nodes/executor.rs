use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

use crate::bus::EventBus;
use crate::memory::MemoryManager;
use crate::message::{Message, Role, ToolCall};
use crate::nodes::Node;
use crate::provider::{ChatRequest, Provider, StreamEvent};
use crate::state::{AgentState, Event};
use crate::tools::{ToolContext, ToolRegistry};
use crate::tui::AgentToUi;

pub struct ExecutorNode {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    model: String,
    cwd: std::path::PathBuf,
    ui_tx: Option<mpsc::UnboundedSender<AgentToUi>>,
    auto_approve: bool,
    memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
}

impl ExecutorNode {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        model: String,
        cwd: std::path::PathBuf,
        ui_tx: Option<mpsc::UnboundedSender<AgentToUi>>,
        auto_approve: bool,
        memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
    ) -> Self {
        Self { provider, tools, model, cwd, ui_tx, auto_approve, memory }
    }

    /// Confirmation round-trip: via the TUI channel if wired, otherwise
    /// auto-approve (`--yes`) or an interactive stdin prompt. Mirrors the
    /// pre-migration `Agent::confirm()`.
    async fn confirm(&self, tool_name: &str, description: String) -> bool {
        if let Some(ui_tx) = &self.ui_tx {
            let (tx, rx) = oneshot::channel();
            let sent = ui_tx.send(AgentToUi::ConfirmRequest { tool_name: tool_name.to_string(), description, respond_to: tx });
            if sent.is_err() {
                return false;
            }
            return rx.await.unwrap_or(false);
        }

        if self.auto_approve {
            return true;
        }

        print!("Allow tool '{tool_name}' to run?\n{description}\n[y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
        } else {
            false
        }
    }
}

const MAX_ROUNDS: usize = 15;

#[async_trait::async_trait]
impl Node for ExecutorNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        while let Ok(event) = rx.recv().await {
            let Event::ContextReady(tid) = event else { continue };
            tracing::info!("ExecutorNode: ContextReady received for task {tid}, running ReAct loop");

            let mut round = 0;
            loop {
                if round >= MAX_ROUNDS {
                    tracing::warn!("ExecutorNode: max rounds ({MAX_ROUNDS}) exceeded for task {tid}, finishing anyway");
                    break;
                }
                round += 1;

                // 1. Build request from state.conversation
                let req = {
                    let st = state.read().await;
                    ChatRequest {
                        model: self.model.clone(),
                        messages: st.conversation.messages.clone(),
                        tools: Some(self.tools.schemas()),
                        tool_choice: None,
                        stream: true,
                        temperature: None,
                        max_tokens: None,
                    }
                };

                let (tx, mut stream_rx) = mpsc::unbounded_channel();

                // Spawn the provider stream so we don't block
                let provider_clone = self.provider.clone();
                tokio::spawn(async move {
                    if let Err(e) = provider_clone.chat_stream(&req, tx).await {
                        tracing::error!("Provider stream error: {}", e);
                    }
                });

                let mut content = String::new();
                let mut tool_calls_acc: BTreeMap<usize, (String, String, String)> = BTreeMap::new();

                while let Some(evt) = stream_rx.recv().await {
                    match evt {
                        StreamEvent::ContentDelta(text) => {
                            content.push_str(&text);
                            let _ = bus.publish(Event::AssistantStreaming(text));
                        }
                        StreamEvent::ToolCallDelta { index, id, name, arguments_delta } => {
                            let entry = tool_calls_acc.entry(index).or_default();
                            if let Some(id) = id { entry.0 = id; }
                            if let Some(name) = name { entry.1 = name; }
                            if let Some(delta) = arguments_delta { entry.2.push_str(&delta); }
                        }
                        StreamEvent::Done { .. } => {}
                    }
                }

                let _ = bus.publish(Event::AssistantStreamingDone);

                let tool_calls: Vec<ToolCall> = tool_calls_acc.into_values().map(|(id, name, arguments)| ToolCall { id, name, arguments }).collect();

                // 2. Append Assistant's turn to conversation
                {
                    let mut st = state.write().await;
                    let final_content = if content.is_empty() { None } else { Some(content) };
                    st.conversation.push(Message {
                        role: Role::Assistant,
                        content: final_content,
                        tool_calls: tool_calls.clone(),
                        tool_call_id: None,
                        name: None,
                    });
                }

                if tool_calls.is_empty() {
                    break;
                }

                // 3. Process each tool call
                let ctx = ToolContext { cwd: self.cwd.clone(), ui_tx: self.ui_tx.clone(), memory: self.memory.clone() };
                for call in &tool_calls {
                    let _ = bus.publish(Event::ActionProposed { call: call.clone() });

                    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| serde_json::json!({}));
                    let tool = self.tools.get(&call.name);

                    let approved = match tool {
                        Some(t) if t.requires_confirmation() => {
                            let description = t.confirmation_description(&args, &ctx);
                            self.confirm(&call.name, description).await
                        }
                        _ => true,
                    };

                    let result_str = if !approved {
                        let _ = bus.publish(Event::ActionRejected(call.id.clone()));
                        format!("User declined to run tool '{}'.", call.name)
                    } else {
                        let _ = bus.publish(Event::ActionApproved(call.id.clone()));
                        match tool {
                            Some(t) => match t.execute(args, &ctx).await {
                                Ok(res) => res,
                                Err(e) => format!("Error executing {}: {}", call.name, e),
                            },
                            None => format!("Error: tool {} not found", call.name),
                        }
                    };

                    let success = approved && !result_str.starts_with("Error");
                    let _ = bus.publish(Event::ActionFinished { id: call.id.clone(), result: result_str.clone(), success });

                    // Push tool result
                    {
                        let mut st = state.write().await;
                        st.conversation.push(Message::tool_result(call.id.clone(), call.name.clone(), result_str));
                    }
                }
            }

            let _ = bus.publish(Event::TurnComplete);
            let _ = bus.publish(Event::ExecutionFinished(tid));
        }
    }
}
