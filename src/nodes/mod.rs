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
