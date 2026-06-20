use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, RwLock};

use crate::bus::EventBus;
use crate::message::{Message, Role};
use crate::nodes::{recv_lossy, Node};
use crate::provider::{ChatRequest, Provider};
use crate::state::{AgentState, Event};
use crate::tools::run_test_suite::{build_shell_command, parse_compiler_errors};

const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ERROR_CHARS: usize = 4000;
const MAX_RECENT_MESSAGES: usize = 12;
const MAX_SUMMARY_FIELD_CHARS: usize = 300;

pub struct VerifierNode {
    cwd: PathBuf,
    provider: Arc<dyn Provider>,
    model: String,
    /// `None` means auto-detect a verify command from project marker files
    /// each time verification runs (since the project layout can change
    /// mid-run, e.g. a task that just created a `go.mod`). `Some` pins it
    /// to a fixed command, used by tests to avoid depending on whichever
    /// toolchains happen to be installed.
    override_command: Option<String>,
}

impl VerifierNode {
    pub fn new(cwd: PathBuf, provider: Arc<dyn Provider>, model: String) -> Self {
        Self { cwd, provider, model, override_command: None }
    }

    /// Test-only override so the suite doesn't depend on real toolchains
    /// (cargo/go/npm) being installed for every verification test. Not
    /// `#[cfg(test)]`-gated because integration tests under `tests/` need
    /// it too, and that attribute only applies within this crate's own
    /// `cfg(test)` compilation, not external test binaries.
    pub fn with_command(cwd: PathBuf, provider: Arc<dyn Provider>, model: String, command: impl Into<String>) -> Self {
        Self { cwd, provider, model, override_command: Some(command.into()) }
    }

    /// Asks the LLM whether the actions just taken plausibly accomplish
    /// `task_description`. Returns `None` for PASS *or* if the judgment
    /// call itself fails (network/parse error) — an infra hiccup here
    /// shouldn't masquerade as "the agent failed its task", the same
    /// lesson behind not hardcoding `cargo check` for every project.
    /// Returns `Some(reason)` for FAIL.
    ///
    /// Deliberately does **not** auto-fail just because `made_tool_calls`
    /// is false: with real planner decomposition, a later task can
    /// legitimately have nothing left to do because an earlier task
    /// already satisfied it (e.g. one tool call created two files a
    /// 2-step plan asked for) — "no action because already done" and "no
    /// action because it only described a plan" look identical from the
    /// event alone, so the judge gets the actual transcript to tell them
    /// apart instead of a blunt short-circuit on that one bit.
    async fn judge_goal_completion(&self, task_description: &str, made_tool_calls: bool, actions_summary: &str) -> Option<String> {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(
                    "You are reviewing whether a coding agent's actions actually accomplished the task it was \
                     given. Respond with the single word PASS if the actions plausibly accomplish the task — \
                     including when no new action was needed because the task was already satisfied by earlier \
                     steps in the conversation, AND including when the task itself was purely conversational \
                     (a greeting, a question, a clarifying reply) and the agent gave a reasonable conversational \
                     response — conversational tasks don't require tool calls to 'pass'. Respond with FAIL \
                     followed by a one-sentence reason only if the agent should have taken a concrete action and \
                     didn't (e.g. it only described or proposed a plan instead of doing it), or did something \
                     unrelated to the task. When in doubt, prefer PASS — this check exists to catch the agent \
                     doing nothing useful at all, not to nitpick a reasonable response. Start your response with \
                     PASS or FAIL.",
                ),
                Message::user(format!(
                    "Task: {task_description}\n\nTool calls made this turn: {}\n\nRecent conversation:\n{actions_summary}",
                    if made_tool_calls { "yes" } else { "no" }
                )),
            ],
            tools: None,
            tool_choice: None,
            stream: false,
            temperature: None,
            max_tokens: None,
        };

        let Ok(resp) = self.provider.chat(&req).await else { return None };
        let Some(content) = resp.message.content else { return None };
        let trimmed = content.trim();

        if trimmed.to_uppercase().starts_with("PASS") {
            return None;
        }
        if trimmed.to_uppercase().starts_with("FAIL") {
            let reason = trimmed[4..].trim_start_matches([':', ' ']).trim();
            return Some(if reason.is_empty() { "the agent's actions did not accomplish the task".to_string() } else { reason.to_string() });
        }
        // Didn't follow the format at all — treat as inconclusive, not a failure.
        None
    }
}

#[async_trait::async_trait]
impl Node for VerifierNode {
    async fn start(&self, bus: EventBus, mut rx: broadcast::Receiver<Event>, state: Arc<RwLock<AgentState>>) {
        while let Some(event) = recv_lossy(&mut rx, "VerifierNode").await {
            let Event::ExecutionFinished { tid, made_tool_calls } = event else { continue };

            // A genuinely empty turn (no tool calls, no text at all) is
            // never a legitimate "nothing left to do" — there's no
            // evidence to even judge. Fail it deterministically instead
            // of asking the LLM judge: under the "no tool calls because
            // already satisfied" framing it needs to allow already-done
            // tasks to pass, the judge reliably defaults to PASS on pure
            // silence too, which is exactly the silent-no-op failure mode
            // this whole check exists to catch.
            if !made_tool_calls && last_assistant_message_is_empty(&state).await {
                tracing::info!("VerifierNode: task {tid} produced no response and no tool calls, failing verification");
                let _ = bus.publish(Event::VerificationFailed { tid, error: "the agent produced no response and took no action for this task".to_string() });
                continue;
            }

            if let Some(command) = self.override_command.clone().or_else(|| detect_verify_command(&self.cwd)) {
                tracing::info!("VerifierNode: running '{command}' to verify task {tid}");
                if let Err(error) = run_verification(&self.cwd, &command).await {
                    let _ = bus.publish(Event::VerificationFailed { tid, error });
                    continue;
                }
            } else {
                tracing::info!("VerifierNode: no recognized project marker in {}, skipping build check for task {tid}", self.cwd.display());
            }

            let (task_description, actions_summary) = {
                let st = state.read().await;
                let description = st.plan.milestones.iter().flat_map(|m| m.tasks.iter()).find(|t| t.id == tid).map(|t| t.description.clone()).unwrap_or_default();
                (description, summarize_recent_actions(&st))
            };

            match self.judge_goal_completion(&task_description, made_tool_calls, &actions_summary).await {
                None => {
                    let _ = bus.publish(Event::VerificationPassed);
                    let _ = bus.publish(Event::TaskCompleted(tid));
                }
                Some(reason) => {
                    let _ = bus.publish(Event::VerificationFailed { tid, error: reason });
                }
            }
        }
    }
}

/// True if the most recent message in the conversation is an assistant
/// turn with no text content — i.e. the model produced literally nothing
/// (already known to have made no tool calls, via `made_tool_calls`).
async fn last_assistant_message_is_empty(state: &Arc<RwLock<AgentState>>) -> bool {
    let st = state.read().await;
    st.conversation.messages.last().is_some_and(|m| m.role == Role::Assistant && m.content.as_deref().unwrap_or("").trim().is_empty())
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

/// Renders the last few non-system messages (across the whole shared
/// conversation, not perfectly scoped to just this task — acceptable
/// since this is a "plausibility" check, not a precise audit) into a
/// compact transcript for the goal-completion judgment call.
fn summarize_recent_actions(state: &AgentState) -> String {
    let messages = &state.conversation.messages;
    let recent = if messages.len() > MAX_RECENT_MESSAGES { &messages[messages.len() - MAX_RECENT_MESSAGES..] } else { &messages[..] };

    recent
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
            };
            let mut line = format!("[{role}]");
            if let Some(content) = &m.content {
                line.push_str(&format!(" {}", truncate_chars(content, MAX_SUMMARY_FIELD_CHARS)));
            }
            for call in &m.tool_calls {
                line.push_str(&format!(" called {}({})", call.name, truncate_chars(&call.arguments, MAX_SUMMARY_FIELD_CHARS)));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max { format!("{}...", s.chars().take(max).collect::<String>()) } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatResponse, DummyProvider, FinishReason};
    use crate::state::AgentState;
    use tokio::sync::RwLock;

    /// The exact regression this guards against: an LLM judge told to be
    /// lenient on ambiguous "no action needed" cases will also default to
    /// PASS on a *completely* empty turn, since there's no positive
    /// evidence either way for it to weigh. A deterministic check doesn't
    /// have that failure mode.
    #[tokio::test]
    async fn last_assistant_message_is_empty_detects_a_truly_blank_turn() {
        let state = Arc::new(RwLock::new(AgentState::default()));
        state.write().await.conversation.push(Message::system("system prompt"));
        state.write().await.conversation.push(Message { role: Role::Assistant, content: None, tool_calls: vec![], tool_call_id: None, name: None });
        assert!(last_assistant_message_is_empty(&state).await);
    }

    #[tokio::test]
    async fn last_assistant_message_is_empty_is_false_when_the_assistant_actually_said_something() {
        let state = Arc::new(RwLock::new(AgentState::default()));
        state.write().await.conversation.push(Message::user("hi"));
        state.write().await.conversation.push(Message { role: Role::Assistant, content: Some("hello there".into()), tool_calls: vec![], tool_call_id: None, name: None });
        assert!(!last_assistant_message_is_empty(&state).await);
    }

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

    fn scripted(content: &str) -> Arc<dyn Provider> {
        Arc::new(DummyProvider::scripted(vec![ChatResponse {
            message: Message { role: Role::Assistant, content: Some(content.to_string()), ..Default::default() },
            finish_reason: FinishReason::Stop,
            usage: None,
        }]))
    }

    #[tokio::test]
    async fn judge_goal_completion_returns_none_on_pass() {
        let node = VerifierNode::with_command(std::env::temp_dir(), scripted("PASS"), "test-model".into(), "true");
        assert_eq!(node.judge_goal_completion("write a file", true, "[tool] called write_file(...)").await, None);
    }

    #[tokio::test]
    async fn judge_goal_completion_returns_reason_on_fail() {
        let node = VerifierNode::with_command(std::env::temp_dir(), scripted("FAIL: the agent only described a plan"), "test-model".into(), "true");
        assert_eq!(node.judge_goal_completion("write a file", false, "[assistant] Here's a plan...").await, Some("the agent only described a plan".to_string()));
    }

    #[tokio::test]
    async fn judge_goal_completion_is_lenient_on_malformed_responses() {
        let node = VerifierNode::with_command(std::env::temp_dir(), scripted("uh, I think it's fine?"), "test-model".into(), "true");
        assert_eq!(node.judge_goal_completion("write a file", true, "[tool] called write_file(...)").await, None);
    }

    /// The exact regression this fix targets: a later decomposed task with
    /// nothing left to do (because an earlier task already satisfied the
    /// goal) must not be auto-failed just because it made no tool calls —
    /// the judge has to look at the transcript and say PASS.
    #[tokio::test]
    async fn judge_goal_completion_passes_when_no_tool_calls_were_needed() {
        let node = VerifierNode::with_command(std::env::temp_dir(), scripted("PASS"), "test-model".into(), "true");
        assert_eq!(
            node.judge_goal_completion("write a second file", false, "[assistant] Both files were already created in the previous step.").await,
            None
        );
    }
}
