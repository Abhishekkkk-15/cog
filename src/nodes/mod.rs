pub mod context;
pub mod executor;
pub mod planner;
pub mod recovery;
pub mod reflector;
pub mod verifier;

use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::state::{AgentState, Event};

#[async_trait::async_trait]
pub trait Node: Send + Sync {
    /// `rx` is created by `Agent::spawn_nodes()` via `bus.subscribe()`
    /// *before* this node's task is spawned — not inside `start()` itself.
    /// Subscribing only after spawning would race the publisher: on a
    /// multi-threaded runtime, nothing guarantees the spawned task reaches
    /// `subscribe()` before the caller publishes the next event, so an
    /// early event (e.g. the initial `GoalReceived`) could be silently
    /// missed forever.
    async fn start(&self, bus: EventBus, rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>);
}

/// Receives the next event, treating a recoverable `Lagged` error as "skip
/// and keep going" rather than fatal. Every node's loop used to be
/// `while let Ok(event) = rx.recv().await`, which silently and
/// *permanently* kills the node's loop on *any* `Err`, including `Lagged`
/// — and a burst of streamed `AssistantStreaming` deltas (one event per
/// token chunk) can easily exceed the bus's capacity for a node with
/// nothing to do in the meantime (e.g. `VerifierNode` waiting on
/// `ExecutionFinished` while `ExecutorNode` streams a long reply). That
/// made it possible for a node to silently stop forever mid-run, with
/// nothing it was responsible for ever happening again and no error
/// logged anywhere — a real, reproducible hang, not a hypothetical one.
pub(crate) async fn recv_lossy(rx: &mut broadcast::Receiver<Event>, node_name: &str) -> Option<Event> {
    loop {
        match rx.recv().await {
            Ok(event) => return Some(event),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("{node_name}: lagged behind by {n} events on the bus, continuing");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}
