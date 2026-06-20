use std::path::{Path, PathBuf};
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
    /// `None` means auto-detect a verify command from project marker files
    /// each time verification runs (since the project layout can change
    /// mid-run, e.g. a task that just created a `go.mod`). `Some` pins it
    /// to a fixed command, used by tests to avoid depending on whichever
    /// toolchains happen to be installed.
    override_command: Option<String>,
}

impl VerifierNode {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, override_command: None }
    }

    /// Test-only override so the suite doesn't depend on real toolchains
    /// (cargo/go/npm) being installed for every verification test. Not
    /// `#[cfg(test)]`-gated because integration tests under `tests/` need
    /// it too, and that attribute only applies within this crate's own
    /// `cfg(test)` compilation, not external test binaries.
    pub fn with_command(cwd: PathBuf, command: impl Into<String>) -> Self {
        Self { cwd, override_command: Some(command.into()) }
    }
}

#[async_trait::async_trait]
impl Node for VerifierNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, _state: Arc<RwLock<AgentState>>) {
        while let Ok(event) = rx.recv().await {
            let Event::ExecutionFinished(tid) = event else { continue };

            let command = self.override_command.clone().or_else(|| detect_verify_command(&self.cwd));

            let Some(command) = command else {
                tracing::info!("VerifierNode: no recognized project marker in {}, skipping verification for task {tid}", self.cwd.display());
                let _ = bus.publish(Event::VerificationPassed);
                let _ = bus.publish(Event::TaskCompleted(tid));
                continue;
            };

            tracing::info!("VerifierNode: running '{command}' to verify task {tid}");
            match run_verification(&self.cwd, &command).await {
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

/// cog operates on arbitrary external projects, not just itself, so the
/// "did this still build" check has to match whatever language the agent
/// is actually working in. Picks a command from marker files in `cwd`
/// (not searched recursively — same scope cargo/go/npm themselves use),
/// and deliberately returns `None` for anything unrecognized rather than
/// forcing a foreign toolchain that would fail for reasons unrelated to
/// the agent's actual change (the bug this replaces: cargo check failing
/// with "no Cargo.toml" on a freshly-scaffolded Go project).
fn detect_verify_command(cwd: &Path) -> Option<String> {
    if cwd.join("Cargo.toml").exists() {
        Some("cargo check".to_string())
    } else if cwd.join("go.mod").exists() {
        Some("go build ./...".to_string())
    } else if cwd.join("package.json").exists() {
        Some("npm run build --if-present".to_string())
    } else {
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cargo_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(detect_verify_command(dir.path()), Some("cargo check".to_string()));
    }

    #[test]
    fn detects_go_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        assert_eq!(detect_verify_command(dir.path()), Some("go build ./...".to_string()));
    }

    #[test]
    fn detects_node_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_verify_command(dir.path()), Some("npm run build --if-present".to_string()));
    }

    #[test]
    fn returns_none_for_unrecognized_project_layout() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_verify_command(dir.path()), None);
    }

    #[test]
    fn cargo_takes_precedence_when_multiple_markers_are_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        assert_eq!(detect_verify_command(dir.path()), Some("cargo check".to_string()));
    }
}
