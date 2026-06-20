use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::nodes::Node;
use crate::state::{AgentState, Event};
use crate::tools::run_test_suite::{build_shell_command, parse_compiler_errors};

const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ERROR_CHARS: usize = 4000;

pub struct VerifierNode {
    cwd: PathBuf,
    command: String,
}

impl VerifierNode {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, command: "cargo check".to_string() }
    }

    /// Test-only override so the suite doesn't spawn a real `cargo check`
    /// (which would recompile this crate) for every verification test. Not
    /// `#[cfg(test)]`-gated because integration tests under `tests/` need
    /// it too, and that attribute only applies within this crate's own
    /// `cfg(test)` compilation, not external test binaries.
    pub fn with_command(cwd: PathBuf, command: impl Into<String>) -> Self {
        Self { cwd, command: command.into() }
    }
}

#[async_trait::async_trait]
impl Node for VerifierNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, _state: Arc<RwLock<AgentState>>) {
        while let Ok(event) = rx.recv().await {
            let Event::ExecutionFinished(tid) = event else { continue };
            tracing::info!("VerifierNode: running '{}' to verify task {tid}", self.command);

            match run_verification(&self.cwd, &self.command).await {
                Ok(()) => {
                    let _ = bus.publish(Event::VerificationPassed);
                    let _ = bus.publish(Event::TaskCompleted(tid));
                }
                Err(error) => {
                    let _ = bus.publish(Event::VerificationFailed { tid, error });
                }
            }
        }
    }
}

async fn run_verification(cwd: &PathBuf, command: &str) -> Result<(), String> {
    let mut cmd = build_shell_command(command);
    cmd.current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd.spawn().map_err(|e| format!("failed to spawn verification command: {e}"))?;

    let output = match tokio::time::timeout(VERIFY_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("verification command failed: {e}")),
        Err(_) => return Err(format!("verification timed out after {}s", VERIFY_TIMEOUT.as_secs())),
    };

    if output.status.success() {
        return Ok(());
    }

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));

    let errors = parse_compiler_errors(&combined);
    let summary = if errors.is_empty() {
        if combined.len() > MAX_ERROR_CHARS { combined[..MAX_ERROR_CHARS].to_string() } else { combined }
    } else {
        serde_json::to_string_pretty(&errors).unwrap_or(combined)
    };

    Err(summary)
}
