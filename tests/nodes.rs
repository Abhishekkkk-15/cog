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
use cog::tools::ToolRegistry;
use cog::tui::AgentToUi;

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

    let executor = ExecutorNode::new(provider, tools, "test-model".into(), dir.path().to_path_buf(), Some(ui_tx), false, None);
    let mut bus_rx = bus.subscribe();
    let exec_rx = bus.subscribe();

    let exec_bus = bus.clone();
    let exec_state = state.clone();
    tokio::spawn(async move { executor.start(exec_bus, exec_rx, exec_state).await });

    tokio::spawn(async move {
        if let Some(AgentToUi::ConfirmRequest { respond_to, .. }) = ui_rx.recv().await {
            let _ = respond_to.send(false);
        }
    });

    let _ = bus.publish(Event::ContextReady("t1".into()));

    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::ExecutionFinished(tid)) => break tid,
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

/// End-to-end through the real node graph: no tool calls means the
/// executor finishes immediately, verification (`cargo check` against this
/// crate, which already builds cleanly) passes, and the planner should
/// have nothing left to do — so `RunFinished(true)` must reach the bus.
#[tokio::test]
async fn agent_run_reaches_run_finished_with_no_tool_calls() {
    let agent = Agent::new(Box::new(DummyProvider::echo()), ToolRegistry::new(), "test-model");
    let bus = agent.bus.clone();
    let mut bus_rx = bus.subscribe();

    agent.spawn_nodes().await;
    let _ = bus.publish(Event::GoalReceived("say hello".into()));

    let success = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::RunFinished(success)) => break success,
                Ok(_) => continue,
                Err(_) => panic!("bus closed before RunFinished"),
            }
        }
    })
    .await
    .expect("run should finish within 120s");

    assert!(success, "a no-op echo response should pass cargo check and finish successfully");
}
