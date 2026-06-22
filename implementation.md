# Implementation: Editing, Steering Messages, and Per-Tool Prompt Guidelines

## Context
Following a review of cog's agent loop against Claude Code's capabilities, three improvements were planned. Before implementing the second and third, the `pi` coding agent (github.com/earendil-works/pi) was researched directly from its source for validation — pi has no plan-approval gate at all (it's purely reactive, using "steering" messages typed mid-task instead), and attaches prompt guidance per-tool rather than in one large system-prompt block. Both findings changed the original plan; the final design below reflects that.

**Status: all three implemented, tested, and live-verified.**

---

## 1. `edit_file`: exact string-replace + lookalike-Unicode fallback

`edit_file` (`src/tools/edit_file.rs`) takes `{path, old_string, new_string}` instead of a unified diff. `old_string` must match the file's current content exactly and occur **exactly once**:
- Zero matches → error telling the model to re-read the file.
- Multiple matches → error with the occurrence count, asking for more surrounding context.

This avoids the failure mode of the old diff-based mechanism, where an LLM-generated unified diff with a slightly wrong line number or missing context line would fail to apply even when the model's intent was clear.

**Fuzzy fallback** (`find_match`/`normalize_lookalikes`): if the exact match finds nothing, a second attempt normalizes visually-confusable Unicode (smart quotes, en/em dashes, non-breaking/thin spaces) to their ASCII equivalent in both the file content and `old_string`, one character at a time, before retrying the match. Character-for-character normalization preserves character positions (not byte length, since some of these are multi-byte), so a match found in normalized space is mapped back to the correct byte span in the *original* (non-normalized) file before the replacement is written — only the matched span is affected, nothing else in the file is touched. This recovers from a real LLM failure mode: copying text that looks identical but uses a different Unicode character than what's actually in the file.

The confirmation popup's diff is generated host-side (`similar::TextDiff`), never trusted from the model. CRLF line endings are detected and round-tripped. The now-unused `diffy` dependency was removed.

Tests (`tests/tools.rs`): unique match, CRLF preservation, not-found, ambiguous (now asserts the occurrence count appears in the message), and a new `edit_file_falls_back_to_a_lookalike_normalized_match` covering the fuzzy path.

---

## 2. Per-tool `prompt_guidelines()`

The general engineering-practice guidance (avoid over-engineering, follow existing conventions, don't over-comment) already lived in `SYSTEM_PROMPT` (`src/lib.rs`) and stays there — it's genuinely general, not tool-specific.

What's new: an optional `prompt_guidelines(&self) -> Option<&str>` method on the `Tool` trait (`src/tools/mod.rs`), for narrow, tool-specific usage tips — pi's pattern of attaching guidance at the tool that needs it, rather than growing one large prompt block as new tools are added. `ToolRegistry::prompt_guidelines()` assembles every registered tool's guideline (sorted by tool name for determinism), and `Agent::with_system_prompt` (`src/agent.rs`) appends the assembled block automatically after the base prompt — callers don't need to change anything.

Three tools currently implement it:
- `edit_file`: keep `old_string` short but unique; re-read the file on "not found" rather than guessing; add real context on "ambiguous" rather than repeating the same snippet.
- `write_file`: prefer `edit_file` for existing files; reserve `write_file` for new files or full rewrites.
- `run_command`: avoid long-running/interactive commands, which will hit the timeout instead of completing.

Tests (`src/tools/mod.rs`): guidelines appear in deterministic sorted order; a registry with no guideline-bearing tools produces an empty string.

---

## 3. Steering messages (replaces the originally-planned plan-approval gate)

**Why not a plan-approval gate:** pi — a well-regarded, competitive coding agent — has no planner, no decomposition step, and no plan-approval gate anywhere in its codebase (confirmed via direct source inspection). It relies entirely on letting the user redirect the agent *while it's already working*. Since cog's existing Planner/Task structure isn't shared by pi, the gate wasn't nonsensical for cog specifically, but pi's total absence of it was treated as real evidence against "decomposed plan + approval gate" being where agent quality comes from — steering was built instead.

**Mechanism:** `Agent` owns `pub steering: Arc<Mutex<Vec<String>>>` (`src/agent.rs`), passed into `ExecutorNode` (`src/nodes/executor.rs`) and shared with the TUI (`src/tui/mod.rs`).

- `App` (`src/tui/app.rs`) tracks `running: bool` — set `true` the moment a goal is submitted, cleared on `RunFinished`.
- While `running`, pressing Enter sends `UiToAgent::SteeringMessage(text)` instead of `UiToAgent::UserPrompt(text)` (`src/tui/event.rs`); the line renders distinctly in the chat panel as "↳ You (steering): ..." (`src/tui/widgets/chat_panel.rs`) rather than starting a second, unrelated goal/plan on top of the one already executing.
- `tui/mod.rs` pushes the text into the shared `steering` queue rather than publishing `GoalReceived` — this never touches `PlannerNode` at all.
- `ExecutorNode` drains the queue into `state.conversation` at the **top of each round**, before building that round's request — never mid-tool-call, which would otherwise risk inserting a `user` message between an assistant's `tool_calls` and its results and break the expected message ordering for the provider API.

Test (`tests/nodes.rs`): `executor_node_drains_a_steering_message_into_the_conversation_before_the_next_round` — pushes a message into the queue while round 1 (a real tool call) is in flight, confirms it lands in the conversation before round 2 and the queue ends up drained.

**Live-verified**: sent a goal ("read main.go and tell me what language it is"), immediately followed by a steering message ("actually ignore that, just tell me the go.mod module name instead") before the first round completed. The agent correctly abandoned the original framing, read `go.mod` instead of `main.go`, and answered the redirected question — confirmed via both the rendered TUI and an instrumented debug log showing the full event sequence.
