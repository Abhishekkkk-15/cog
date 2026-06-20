use crate::agent::AgentError;
use crate::memory::MemoryError;
use crate::provider::ProviderError;

#[derive(Debug, thiserror::Error)]
pub enum CogError {
    #[error("config error: {0}")]
    Config(String),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
    #[error("memory error: {0}")]
    Memory(#[from] MemoryError),
    #[error("tool error: {0}")]
    Tool(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CogError>;
