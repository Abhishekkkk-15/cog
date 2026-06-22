use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use tokio::sync::mpsc;

use cog::agent::Agent;
use cog::bus::EventBus;
use cog::message::{Message, Role, ToolCall};
use cog::nodes::executor::ExecutorNode;
use cog::nodes::Node;
use cog::provider::{ChatResponse, DummyProvider, FinishReason};
use cog::state::{AgentState, Event};
use cog::tools::{FileSnapshots, ToolRegistry};
use cog::tui::{AgentToUi, ConfirmDecision};

fn no_trust() -> Arc<std::sync::Mutex<HashSet<String>>> {
    Arc::new(std::sync::Mutex::new(HashSet::new()))
}

fn no_snapshots() -> FileSnapshots {
    Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn no_steering() -> Arc<std::sync::Mutex<Vec<String>>> {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

struct ConstEmbedder;

#[async_trait::async_trait]
impl cog::memory::Embedder for ConstEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, cog::memory::EmbedError> {
        Ok(vec![vec![0.1; 1024]; texts.len()])
    }
    fn dimensions(&self) -> usize {
        1024
    }
}

/// `MemoryManager::compress_if_needed`/`save_message` are fully built and
/// tested in isolation (`tests/memory.rs`) but were never actually called
/// anywhere in the live agent loop until now — this proves `ExecutorNode`
/// actually persists conversation turns when a memory manager is wired in.
#[tokio::test]
async fn executor_node_persists_messages_to_memory() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("mem.db");
    let memory = Arc::new(tokio::sync::Mutex::new(cog::memory::MemoryManager::open(&db_path, Arc::new(ConstEmbedder)).unwrap()));

    let tools = Arc::new(ToolRegistry::new());
    let provider = Arc::new(DummyProvider::echo());

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let run_id = state.read().await.run_id.clone();
    memory.lock().await.create_session(&run_id, "test-project").await.unwrap();

    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), None, false, Some(memory.clone()), no_trust(), no_snapshots(), no_steering());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    state.write().await.conversation.push(Message::user("say hello"));
    let _ = bus.publish(Event::ContextReady("t1".into()));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 5s");

    let saved = memory.lock().await.load_recent(&run_id, 10).await.unwrap();
    assert!(saved.iter().any(|m| m.role == Role::Assistant), "the assistant's reply should have been persisted to memory");
}

/// End-to-end proof that a failed task's file changes actually get rolled
/// back: a real `write_file` call records a snapshot via the shared map,
/// then driving `RecoveryNode` through enough failures to exhaust its
/// retries restores the touched file to its pre-run state (here: removed,
/// since it didn't exist before this run).
#[tokio::test]
async fn recovery_node_restores_files_after_exhausting_retries() {
    use cog::state::{Milestone, MilestoneStatus, Task, TaskStatus};
    use cog::tools::ToolContext;

    let dir = tempdir().unwrap();
    let snapshots = no_snapshots();

    let write_ctx = ToolContext { cwd: dir.path().to_path_buf(), ui_tx: None, memory: None, snapshots: Some(snapshots.clone()) };
    let registry = ToolRegistry::new();
    registry.get("write_file").unwrap().execute(serde_json::json!({"path": "created.txt", "content": "from a failed attempt"}), &write_ctx).await.unwrap();

    let created_path = dir.path().join("created.txt");
    assert!(created_path.exists());
    assert!(!snapshots.lock().unwrap().is_empty(), "write_file should have recorded a snapshot");

    let bus = EventBus::new(32);
    let state = AgentState::new();
    {
        let mut st = state.write().await;
        st.plan.milestones.push(Milestone {
            id: "m1".into(),
            description: String::new(),
            status: MilestoneStatus::Active,
            tasks: vec![Task { id: "t1".into(), description: String::new(), dependencies: vec![], status: TaskStatus::Active, retry_count: 0 }],
        });
    }

    let recovery = cog::nodes::recovery::RecoveryNode::new(snapshots.clone());
    let mut bus_rx = bus.subscribe();
    let rec_rx = bus.subscribe();
    let rec_bus = bus.clone();
    let rec_state = state.clone();
    tokio::spawn(async move { recovery.start(rec_bus, rec_rx, rec_state).await });

    // 3 failures: the first 2 retry (retry_count 1, 2), the 3rd exceeds
    // MAX_RETRIES and triggers the give-up/rollback path.
    for _ in 0..3 {
        let _ = bus.publish(Event::ReflectionGenerated { tid: "t1".into(), reflection: "it broke".into() });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match bus_rx.recv().await {
                    Ok(Event::TaskStarted(_)) | Ok(Event::RunFinished(_)) => break,
                    Ok(_) => continue,
                    Err(_) => panic!("bus closed unexpectedly"),
                }
            }
        })
        .await
        .expect("recovery should react within 5s");
    }

    assert!(!created_path.exists(), "the file created by the failed attempt should have been rolled back");
    assert!(snapshots.lock().unwrap().is_empty(), "the snapshot map should be drained after rollback");
}

/// `ContextNode` was a pure stub before this — it published `ContextReady`
/// without doing any retrieval at all. This proves it now actually pulls
/// relevant facts out of memory and injects them before the executor runs.
#[tokio::test]
async fn context_node_injects_relevant_memory_before_context_ready() {
    use cog::state::{Milestone, MilestoneStatus, Task, TaskStatus};

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("mem.db");
    let memory = Arc::new(tokio::sync::Mutex::new(cog::memory::MemoryManager::open(&db_path, Arc::new(ConstEmbedder)).unwrap()));
    memory.lock().await.remember("project_lang", "this project uses Rust").await.unwrap();

    let bus = EventBus::new(32);
    let state = AgentState::new();
    {
        let mut st = state.write().await;
        st.plan.milestones.push(Milestone {
            id: "m1".into(),
            description: String::new(),
            status: MilestoneStatus::Active,
            tasks: vec![Task { id: "t1".into(), description: "Rust".into(), dependencies: vec![], status: TaskStatus::Active, retry_count: 0 }],
        });
    }

    let context_node = cog::nodes::context::ContextNode::new(Some(memory.clone()));
    let mut bus_rx = bus.subscribe();
    let ctx_rx = bus.subscribe();
    let ctx_bus = bus.clone();
    let ctx_state = state.clone();
    tokio::spawn(async move { context_node.start(ctx_bus, ctx_rx, ctx_state).await });

    let _ = bus.publish(Event::TaskStarted("t1".into()));

    let tid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ContextReady(tid)) => break tid,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ContextReady"),
            }
        }
    })
    .await
    .expect("context node should finish within 5s");

    assert_eq!(tid, "t1");
    let st = state.read().await;
    assert!(
        st.conversation.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("this project uses Rust")),
        "the relevant fact should have been injected into conversation"
    );
}

/// Mirrors the existing `ask_user_round_trips_through_ui_channel` pattern in
/// `tests/tools.rs`: a `requires_confirmation()` tool (write_file) should
/// route through `ui_tx`'s `ConfirmRequest`, and a rejection must skip
/// execution and tell the model so via the tool-result message instead of
/// silently writing the file.
#[tokio::test]
async fn executor_node_respects_a_confirmation_rejection() {
    let dir = tempdir().unwrap();

    let tools = Arc::new(ToolRegistry::new());
    let provider = Arc::new(DummyProvider::scripted(vec![ChatResponse {
        message: Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall { id: "call1".into(), name: "write_file".into(), arguments: serde_json::json!({"path": "out.txt", "content": "hello"}).to_string() }],
            tool_call_id: None,
            name: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }]));

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<AgentToUi>();

    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), Some(ui_tx), false, None, no_trust(), no_snapshots(), no_steering());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    tokio::spawn(async move {
        if let Some(AgentToUi::ConfirmRequest { respond_to, .. }) = ui_rx.recv().await {
            let _ = respond_to.send(ConfirmDecision::Deny);
        }
    });

    let _ = bus.publish(Event::ContextReady("t1".into()));

    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { tid, .. }) => break tid,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 5s");

    assert_eq!(finished, "t1");
    assert!(!dir.path().join("out.txt").exists(), "rejected tool call must not write the file");

    let st = state.read().await;
    assert!(
        st.conversation.messages.iter().any(|m| m.role == Role::Tool && m.content.as_deref().unwrap_or("").contains("declined")),
        "conversation should record that the user declined the tool call"
    );
}

/// Answering "always" for a `requires_confirmation()` tool should trust
/// that tool name for the rest of the session: a second call to the same
/// tool must skip the prompt entirely and auto-execute.
#[tokio::test]
async fn executor_node_remembers_always_trust_for_the_rest_of_the_session() {
    let dir = tempdir().unwrap();
    let tools = Arc::new(ToolRegistry::new());
    let provider = Arc::new(DummyProvider::scripted(vec![
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: None,
                tool_calls: vec![ToolCall { id: "call1".into(), name: "write_file".into(), arguments: serde_json::json!({"path": "a.txt", "content": "a"}).to_string() }],
                tool_call_id: None,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: None,
                tool_calls: vec![ToolCall { id: "call2".into(), name: "write_file".into(), arguments: serde_json::json!({"path": "b.txt", "content": "b"}).to_string() }],
                tool_call_id: None,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
    ]));

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<AgentToUi>();

    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), Some(ui_tx), false, None, no_trust(), no_snapshots(), no_steering());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    let prompt_count = Arc::new(AtomicUsize::new(0));
    let prompt_count_clone = prompt_count.clone();
    tokio::spawn(async move {
        while let Some(msg) = ui_rx.recv().await {
            if let AgentToUi::ConfirmRequest { respond_to, .. } = msg {
                prompt_count_clone.fetch_add(1, Ordering::SeqCst);
                let _ = respond_to.send(ConfirmDecision::Always);
            }
        }
    });

    let _ = bus.publish(Event::ContextReady("t1".into()));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 5s");

    assert!(dir.path().join("a.txt").exists(), "first write_file call should have run after explicit approval");
    assert!(dir.path().join("b.txt").exists(), "second write_file call should have run via the trust set, not a fresh prompt");
    assert_eq!(prompt_count.load(Ordering::SeqCst), 1, "only the first write_file call should have prompted at all");
}

/// End-to-end through the real node graph with a real tool call: planner
/// decomposition falls back to a single task (the scripted "decompose"
/// response isn't JSON), the executor calls a real, non-confirmation-gated
/// tool, the build check runs for real against this crate (which builds
/// cleanly), and once the scripted queue is exhausted the goal-completion
/// judge call falls through to `DummyProvider`'s default echo response —
/// which says neither PASS nor FAIL, so it's treated as inconclusive (see
/// `judge_goal_completion`'s doc comment for why that's the safe default)
/// — reaching `RunFinished(true)`.
///
/// The zero-tool-calls auto-fail heuristic this replaced is deliberately
/// gone (see `VerifierNode::judge_goal_completion`'s doc comment): it
/// produced a false failure whenever a later decomposed task correctly
/// had nothing left to do. That specific regression is covered directly
/// by `judge_goal_completion_passes_when_no_tool_calls_were_needed` in
/// `src/nodes/verifier.rs`, which doesn't need a full multi-node run to
/// exercise.
#[tokio::test]
async fn agent_run_reaches_run_finished_with_a_real_tool_call() {
    let provider: Box<dyn cog::provider::Provider> = Box::new(DummyProvider::scripted(vec![
        ChatResponse {
            message: Message { role: Role::Assistant, content: Some("just do it".into()), ..Default::default() },
            finish_reason: FinishReason::Stop,
            usage: None,
        },
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: None,
                tool_calls: vec![ToolCall { id: "call1".into(), name: "list_dir".into(), arguments: "{}".into() }],
                tool_call_id: None,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
    ]));

    let agent = Agent::new(provider, ToolRegistry::new(), "test-model");
    let bus = agent.bus.clone();
    let mut bus_rx = bus.subscribe();

    agent.spawn_nodes().await;
    let _ = bus.publish(Event::GoalReceived("list the files in this directory".into()));

    let success = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::RunFinished(success)) => break success,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before RunFinished"),
            }
        }
    })
    .await
    .expect("run should finish within 60s");

    assert!(success, "a real tool call should pass the build check and the (inconclusive-but-allowed) goal-completion check");
}

/// Simulates the TUI pushing a steering message into the shared queue while
/// round 1 (a real tool call) is still in flight — exercises the same path
/// `tui/mod.rs` uses for `UiToAgent::SteeringMessage`. The message should
/// land in `state.conversation` before round 2's request is built, and the
/// queue should end up drained (not left for some later task to re-inject).
#[tokio::test]
async fn executor_node_drains_a_steering_message_into_the_conversation_before_the_next_round() {
    let dir = tempdir().unwrap();
    let tools = Arc::new(ToolRegistry::new());
    let provider = Arc::new(DummyProvider::scripted(vec![
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: None,
                tool_calls: vec![ToolCall { id: "call1".into(), name: "list_dir".into(), arguments: "{}".into() }],
                tool_call_id: None,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        ChatResponse {
            message: Message { role: Role::Assistant, content: Some("done".into()), ..Default::default() },
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ]));

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let steering = Arc::new(std::sync::Mutex::new(Vec::new()));

    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), None, false, None, no_trust(), no_snapshots(), steering.clone());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    state.write().await.conversation.push(Message::user("list the files"));
    let _ = bus.publish(Event::ContextReady("t1".into()));
    steering.lock().unwrap().push("actually, also check for a README".to_string());

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 10s");

    let messages = state.read().await.conversation.messages.clone();
    let steered = messages.iter().find(|m| m.role == Role::User && m.content.as_deref() == Some("actually, also check for a README"));
    assert!(steered.is_some(), "steering message should have been injected into the conversation");
    assert!(steering.lock().unwrap().is_empty(), "the queue should be drained, not left for a future task to pick up twice");
}

struct SlowEchoTool;

#[async_trait::async_trait]
impl cog::tools::Tool for SlowEchoTool {
    fn name(&self) -> &str {
        "slow_echo"
    }

    fn description(&self) -> &str {
        "Sleeps briefly then echoes its input — test-only tool used to measure whether independent tool calls run concurrently."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]})
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &cog::tools::ToolContext) -> Result<String, cog::tools::ToolError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(args.get("value").and_then(serde_json::Value::as_str).unwrap_or("").to_string())
    }
}

/// Two calls to a 200ms tool in the same round, neither requiring
/// confirmation, should overlap rather than run back to back — sequential
/// execution would take >=400ms; concurrent execution should finish well
/// under that.
#[tokio::test]
async fn executor_node_runs_independent_tool_calls_concurrently() {
    let dir = tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(SlowEchoTool));
    let tools = Arc::new(registry);

    let provider = Arc::new(DummyProvider::scripted(vec![
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: None,
                tool_calls: vec![
                    ToolCall { id: "call1".into(), name: "slow_echo".into(), arguments: r#"{"value": "a"}"#.into() },
                    ToolCall { id: "call2".into(), name: "slow_echo".into(), arguments: r#"{"value": "b"}"#.into() },
                ],
                tool_call_id: None,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        ChatResponse {
            message: Message { role: Role::Assistant, content: Some("done".into()), ..Default::default() },
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ]));

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), None, false, None, no_trust(), no_snapshots(), no_steering());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    state.write().await.conversation.push(Message::user("run both"));
    let started = std::time::Instant::now();
    let _ = bus.publish(Event::ContextReady("t1".into()));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 5s");

    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(350), "two 200ms tool calls should overlap, not run back to back (took {elapsed:?})");
}

/// Two *confirmation-gated* calls in the same round must still each get
/// answered correctly — these stay in the sequential batch (one UI prompt
/// at a time) even though independent non-gated calls now run concurrently.
#[tokio::test]
async fn executor_node_confirms_two_gated_calls_in_the_same_round() {
    let dir = tempdir().unwrap();
    let tools = Arc::new(ToolRegistry::new());
    let provider = Arc::new(DummyProvider::scripted(vec![
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: None,
                tool_calls: vec![
                    ToolCall { id: "call1".into(), name: "write_file".into(), arguments: serde_json::json!({"path": "a.txt", "content": "a"}).to_string() },
                    ToolCall { id: "call2".into(), name: "write_file".into(), arguments: serde_json::json!({"path": "b.txt", "content": "b"}).to_string() },
                ],
                tool_call_id: None,
                name: None,
            },
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        ChatResponse {
            message: Message { role: Role::Assistant, content: Some("done".into()), ..Default::default() },
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ]));

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<AgentToUi>();

    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), Some(ui_tx), false, None, no_trust(), no_snapshots(), no_steering());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    let prompt_count = Arc::new(AtomicUsize::new(0));
    let prompt_count_clone = prompt_count.clone();
    tokio::spawn(async move {
        while let Some(msg) = ui_rx.recv().await {
            if let AgentToUi::ConfirmRequest { respond_to, .. } = msg {
                prompt_count_clone.fetch_add(1, Ordering::SeqCst);
                let _ = respond_to.send(ConfirmDecision::Once);
            }
        }
    });

    state.write().await.conversation.push(Message::user("write both"));
    let _ = bus.publish(Event::ContextReady("t1".into()));

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 10s");

    assert_eq!(prompt_count.load(Ordering::SeqCst), 2);
    assert!(dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").exists());
}

/// Exercises the real stdin y/N fallback in `ExecutorNode::confirm()` — the
/// path used by `cog run` with no TUI wired (no `ui_tx`) and without
/// `--yes`. Reads the actual process stdin rather than a mock, so it's
/// `#[ignore]`d to avoid racing other tests for the same fd under the
/// default parallel test runner. Run explicitly with stdin piped, e.g.:
///   echo y | cargo test --test nodes stdin_confirmation_accepts -- --ignored --exact --nocapture
///   echo n | cargo test --test nodes stdin_confirmation_declines -- --ignored --exact --nocapture
async fn run_command_via_stdin_confirmation(dir: &std::path::Path) -> AgentState {
    let tools = Arc::new(ToolRegistry::new());
    let provider = Arc::new(DummyProvider::scripted(vec![ChatResponse {
        message: Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall { id: "call1".into(), name: "run_command".into(), arguments: serde_json::json!({"command": "echo confirmed-run-marker"}).to_string() }],
            tool_call_id: None,
            name: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }]));

    let bus = EventBus::new(32);
    let state = AgentState::new();
    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.to_path_buf(), None, false, None, no_trust(), no_snapshots(), no_steering());
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    let _ = bus.publish(Event::ContextReady("t1".into()));

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished { .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before ExecutionFinished"),
            }
        }
    })
    .await
    .expect("executor should finish within 30s — did you forget to pipe an answer into stdin?");

    // The executor task loops forever waiting for the next event, so its
    // clone of the Arc is still alive here — read out a clone instead of
    // trying to unwrap the Arc.
    state.read().await.clone()
}

#[tokio::test]
#[ignore]
async fn stdin_confirmation_accepts_when_user_types_y() {
    let dir = tempdir().unwrap();
    let state = run_command_via_stdin_confirmation(dir.path()).await;
    assert!(
        state.conversation.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("confirmed-run-marker")),
        "approving via stdin should have actually run the command"
    );
}

#[tokio::test]
#[ignore]
async fn stdin_confirmation_declines_when_user_types_n() {
    let dir = tempdir().unwrap();
    let state = run_command_via_stdin_confirmation(dir.path()).await;
    assert!(
        !state.conversation.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("confirmed-run-marker")),
        "declining via stdin must not run the command"
    );
    assert!(
        state.conversation.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("declined")),
        "conversation should record the decline"
    );
}
