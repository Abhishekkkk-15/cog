# Implementation Plan: Editing, Plan Review, and System Prompt Improvements

## Context
Following a review of cog's agent loop against Claude Code's capabilities, three improvements were selected:

1. Replace `edit_file`'s unified-diff mechanism with exact string replacement
2. Surface the decomposed plan to the user before execution starts (Plan Mode parity)
3. Enrich `SYSTEM_PROMPT` with engineering-practice guidance

This file tracks scope, rationale, and status for each. **Status: planning only — no code changes made yet.**

---

## 1. Replace `edit_file`'s diff mechanism with exact string-replace

**Problem:** `edit_file` currently requires the model to generate a unified diff (`diff -u` format, applied via the `diffy` crate) with correct line numbers and context lines. LLMs are unreliable at producing syntactically correct unified diffs — a single off-by-one line number or missing context line causes the whole edit to fail to apply, even when the model's *intent* was clear.

**Plan:**
- Change the tool's parameters from `{path, diff}` to `{path, old_string, new_string}`.
- `old_string` must match the file's current content exactly and occur **exactly once**. Zero matches or multiple matches both return a clear error telling the model to re-read the file or add more surrounding context — no guessing, no partial application.
- Generate the confirmation-popup diff internally (via `similar::TextDiff`, the same crate `write_file` already uses), rather than relying on a diff the model produced.
- Preserve the existing CRLF-handling behavior (detect and round-trip line endings).
- Remove the now-unused `diffy` dependency from `Cargo.toml`.
- Rewrite `tests/tools.rs`'s `edit_file_*` tests for the new params; add coverage for the "not found" and "ambiguous match" error cases.

**Status:** Not started.

---

## 2. Surface the decomposed plan before execution starts

**Problem:** `PlannerNode` decomposes a goal into a task list and `ExecutorNode` starts executing the first task immediately — the user never sees the plan or gets a chance to redirect it before tool calls start happening.

**Open question (pending input):** should execution actually *block* until the plan is approved — a y/n-style gate, mirroring the existing tool-confirmation flow — or just *display* the plan as a heads-up while proceeding automatically? This changes the implementation shape (a new confirmation round-trip vs. a one-way notification) and isn't decided yet.

**Status:** Blocked on the above design decision.

---

## 3. Enrich `SYSTEM_PROMPT` with engineering-practice guidance

**Problem:** The current system prompt only addresses *taking action* (use tools, don't just describe what you'd do). It says nothing about code quality — avoiding unnecessary abstractions, following a project's existing conventions, or not over-commenting.

**Plan:**
- Add guidance against over-engineering: no abstractions or features beyond what was actually asked for.
- Add guidance to follow the existing code conventions/style of whatever project is being worked on.
- Add guidance against unnecessary comments (explain *why* something non-obvious is done, not *what* the code already says).
- Purely additive — the existing action-oriented instructions stay as-is.

**Status:** Not started.
