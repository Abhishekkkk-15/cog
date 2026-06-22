use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

use crate::bus::EventBus;
use crate::memory::MemoryManager;
use crate::message::{Message, Role, ToolCall};
use crate::nodes::{recv_lossy, Node};
use crate::provider::{ChatRequest, Provider, StreamEvent};
use crate::state::{AgentState, Event};
use crate::tools::{FileSnapshots, ToolContext, ToolRegistry};
use crate::tui::{AgentToUi, ConfirmDecision};

pub struct ExecutorNode {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    model: String,
    cwd: std::path::PathBuf,
    ui_tx: Option<mpsc::UnboundedSender<AgentToUi>>,
    auto_approve: bool,
    memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
    /// Tool names the user has answered "always" for — session-only
    /// (never persisted), shared so the trust sticks for the rest of this
    /// run regardless of which task is currently executing.
    trusted: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Shared with `RecoveryNode`, which restores from and drains this on
    /// a give-up. Cleared here on `RunFinished` (success or failure) so a
    /// finished run's snapshots never bleed into the next prompt in a
    /// long-lived TUI session.
    snapshots: FileSnapshots,
    /// Messages the TUI queues here when the user types while a task is
    /// already running (see `Agent::steering`). Drained into the
    /// conversation only at the top of a round — never mid-tool-call, which
    /// would otherwise interleave a `user` message between an assistant's
    /// `tool_calls` and its results and break the expected message ordering.
    steering: Arc<std::sync::Mutex<Vec<String>>>,
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
        trusted: Arc<std::sync::Mutex<HashSet<String>>>,
        snapshots: FileSnapshots,
        steering: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        Self { provider, tools, model, cwd, ui_tx, auto_approve, memory, trusted, snapshots, steering }
    }

    /// Confirmation round-trip: via the TUI channel if wired, otherwise
    /// auto-approve (`--yes`) or an interactive stdin prompt. Mirrors the
    /// pre-migration `Agent::confirm()`, extended with an "always" answer.
    async fn confirm(&self, tool_name: &str, description: String) -> ConfirmDecision {
        if let Some(ui_tx) = &self.ui_tx {
            let (tx, rx) = oneshot::channel();
            let sent = ui_tx.send(AgentToUi::ConfirmRequest { tool_name: tool_name.to_string(), description, respond_to: tx });
            if sent.is_err() {
                return ConfirmDecision::Deny;
            }
            return rx.await.unwrap_or(ConfirmDecision::Deny);
        }

        if self.auto_approve {
            return ConfirmDecision::Once;
        }

        print!("Allow tool '{tool_name}' to run?\n{description}\n[y/a/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            match line.trim().to_lowercase().as_str() {
                "y" | "yes" => ConfirmDecision::Once,
                "a" | "always" => ConfirmDecision::Always,
                _ => ConfirmDecision::Deny,
            }
        } else {
            ConfirmDecision::Deny
        }
    }
}

const MAX_ROUNDS: usize = 15;

#[async_trait::async_trait]
impl Node for ExecutorNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        let session_id = state.read().await.run_id.clone();

        while let Some(event) = recv_lossy(&mut rx, "ExecutorNode").await {
            if matches!(event, Event::RunFinished(_)) {
                self.snapshots.lock().unwrap().clear();
                continue;
            }
            let Event::ContextReady(tid) = event else { continue };
            tracing::info!("ExecutorNode: ContextReady received for task {tid}, running ReAct loop");

            let mut round = 0;
            let mut made_tool_calls = false;
            loop {
                if round >= MAX_ROUNDS {
                    tracing::warn!("ExecutorNode: max rounds ({MAX_ROUNDS}) exceeded for task {tid}, finishing anyway");
                    break;
                }
                round += 1;

                // Pick up anything the user typed while this task was
                // already running. Draining here — top of the round, after
                // the previous round's tool results are already pushed and
                // before this round's request is built — is the only point
                // where inserting a `user` message can't end up between an
                // assistant's `tool_calls` and its results.
                let steered: Vec<String> = {
                    let mut queue = self.steering.lock().unwrap();
                    queue.drain(..).collect()
                };
                if !steered.is_empty() {
                    let mut st = state.write().await;
                    for text in steered {
                        st.conversation.push(Message::user(text));
                    }
                }

                // Summarize the middle of the conversation once it's grown
                // large enough, before spending tokens on this round's request.
                if let Some(memory) = &self.memory {
                    let mem = memory.lock().await;
                    let mut st = state.write().await;
                    let _ = mem.compress_if_needed(&mut st.conversation, &*self.provider, &self.model).await;
                }

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
                let assistant_msg = Message {
                    role: Role::Assistant,
                    content: if content.is_empty() { None } else { Some(content) },
                    tool_calls: tool_calls.clone(),
                    tool_call_id: None,
                    name: None,
                };
                {
                    let mut st = state.write().await;
                    st.conversation.push(assistant_msg.clone());
                }
                if let Some(memory) = &self.memory {
                    let mem = memory.lock().await;
                    let _ = mem.save_message(&session_id, &assistant_msg).await;
                }

                if tool_calls.is_empty() {
                    break;
                }
                made_tool_calls = true;

                // 3. Process each tool call
                let ctx = ToolContext { cwd: self.cwd.clone(), ui_tx: self.ui_tx.clone(), memory: self.memory.clone(), snapshots: Some(self.snapshots.clone()) };
                for call in &tool_calls {
                    let _ = bus.publish(Event::ActionProposed { call: call.clone() });

                    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| serde_json::json!({}));
                    let tool = self.tools.get(&call.name);
                    let already_trusted = self.trusted.lock().unwrap().contains(&call.name);

                    let approved = match tool {
                        Some(t) if t.requires_confirmation() && !already_trusted => {
                            let description = t.confirmation_description(&args, &ctx);
                            match self.confirm(&call.name, description).await {
                                ConfirmDecision::Once => true,
                                ConfirmDecision::Always => {
                                    self.trusted.lock().unwrap().insert(call.name.clone());
                                    true
                                }
                                ConfirmDecision::Deny => false,
                            }
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
                    let tool_msg = Message::tool_result(call.id.clone(), call.name.clone(), result_str);
                    {
                        let mut st = state.write().await;
                        st.conversation.push(tool_msg.clone());
                    }
                    if let Some(memory) = &self.memory {
                        let mem = memory.lock().await;
                        let _ = mem.save_message(&session_id, &tool_msg).await;
                    }
                }
            }

            let _ = bus.publish(Event::TurnComplete);
            let _ = bus.publish(Event::ExecutionFinished { tid, made_tool_calls });
        }
    }
}
