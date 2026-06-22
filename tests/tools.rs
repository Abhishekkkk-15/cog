use serde_json::json;
use tempfile::tempdir;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cog::tools::{FileSnapshots, ToolContext, ToolRegistry};
use cog::tui::AgentToUi;

fn ctx(cwd: std::path::PathBuf) -> ToolContext {
    ToolContext { cwd, ui_tx: None, memory: None, snapshots: None }
}

fn ctx_with_snapshots(cwd: std::path::PathBuf) -> (ToolContext, FileSnapshots) {
    let snapshots: FileSnapshots = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    (ToolContext { cwd, ui_tx: None, memory: None, snapshots: Some(snapshots.clone()) }, snapshots)
}

async fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = tokio::process::Command::new("git").args(args).current_dir(dir).output().await.expect("git should run");
    assert!(status.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&status.stderr));
}

#[tokio::test]
async fn read_file_returns_numbered_lines_and_respects_range() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("read_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let full = tool.execute(json!({"path": "a.txt"}), &context).await.unwrap();
    assert!(full.contains("one"));
    assert!(full.contains("four"));

    let ranged = tool.execute(json!({"path": "a.txt", "start_line": 2, "end_line": 3}), &context).await.unwrap();
    assert!(ranged.contains("two"));
    assert!(ranged.contains("three"));
    assert!(!ranged.contains("one"));
    assert!(!ranged.contains("four"));
}

#[tokio::test]
async fn write_file_creates_then_overwrites() {
    let dir = tempdir().unwrap();
    let registry = ToolRegistry::new();
    let tool = registry.get("write_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let created = tool.execute(json!({"path": "out.txt", "content": "hello\n"}), &context).await.unwrap();
    assert!(created.contains("created"));
    assert_eq!(std::fs::read_to_string(dir.path().join("out.txt")).unwrap(), "hello\n");

    let overwritten = tool.execute(json!({"path": "out.txt", "content": "bye\n"}), &context).await.unwrap();
    assert!(overwritten.contains("wrote"));
    assert_eq!(std::fs::read_to_string(dir.path().join("out.txt")).unwrap(), "bye\n");
}

/// The rollback feature (`RecoveryNode::restore_snapshots`) depends on
/// `write_file`/`edit_file` actually recording prior content — this is the
/// part of that mechanism that lives in the tools themselves.
#[tokio::test]
async fn write_file_and_edit_file_record_snapshots_for_rollback() {
    let dir = tempdir().unwrap();
    let registry = ToolRegistry::new();
    let (context, snapshots) = ctx_with_snapshots(dir.path().to_path_buf());

    // A brand-new file: the snapshot should record "didn't exist" (None).
    let new_path = dir.path().join("new.txt");
    registry.get("write_file").unwrap().execute(json!({"path": "new.txt", "content": "v1"}), &context).await.unwrap();
    assert_eq!(snapshots.lock().unwrap().get(&new_path), Some(&None));

    // An existing file: the snapshot should capture its content from
    // *before* this run touched it, even across multiple writes.
    let existing_path = dir.path().join("existing.txt");
    std::fs::write(&existing_path, "original").unwrap();
    registry.get("write_file").unwrap().execute(json!({"path": "existing.txt", "content": "v2"}), &context).await.unwrap();
    registry.get("write_file").unwrap().execute(json!({"path": "existing.txt", "content": "v3"}), &context).await.unwrap();
    assert_eq!(snapshots.lock().unwrap().get(&existing_path), Some(&Some(b"original".to_vec())));

    // edit_file uses the same mechanism.
    let edited_path = dir.path().join("edited.txt");
    std::fs::write(&edited_path, "line1\nline2\n").unwrap();
    registry
        .get("edit_file")
        .unwrap()
        .execute(json!({"path": "edited.txt", "old_string": "line2", "new_string": "CHANGED"}), &context)
        .await
        .unwrap();
    assert_eq!(snapshots.lock().unwrap().get(&edited_path), Some(&Some(b"line1\nline2\n".to_vec())));
}

#[tokio::test]
async fn write_file_confirmation_description_shows_a_diff() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "old\n").unwrap();
    let registry = ToolRegistry::new();
    let tool = registry.get("write_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let description = tool.confirmation_description(&json!({"path": "f.txt", "content": "new\n"}), &context);
    assert!(description.contains("-old"));
    assert!(description.contains("+new"));
}

#[tokio::test]
async fn edit_file_replaces_a_unique_match() {
    let dir = tempdir().unwrap();
    let old = "line1\nline2\nline3\nline4\nline5\n";
    let new = "line1\nCHANGED\nline3\nline4\nline5\n";
    std::fs::write(dir.path().join("f.txt"), old).unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"path": "f.txt", "old_string": "line2", "new_string": "CHANGED"}), &context).await.unwrap();
    assert!(result.contains("edited"));
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), new);
}

#[tokio::test]
async fn edit_file_preserves_crlf_line_endings() {
    let dir = tempdir().unwrap();
    let old_lf = "line1\nline2\nline3\n";
    let new_lf = "line1\nCHANGED\nline3\n";
    let old_crlf = old_lf.replace('\n', "\r\n");
    std::fs::write(dir.path().join("f.txt"), &old_crlf).unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    tool.execute(json!({"path": "f.txt", "old_string": "line2", "new_string": "CHANGED"}), &context).await.unwrap();
    let result = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(result, new_lf.replace('\n', "\r\n"));
}

#[tokio::test]
async fn edit_file_rejects_a_string_not_found_in_the_file() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "actual content here\n").unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"path": "f.txt", "old_string": "not present", "new_string": "x"}), &context).await;
    let err = result.expect_err("a non-matching old_string should fail");
    assert!(err.to_string().contains("not found"));
}

#[tokio::test]
async fn edit_file_rejects_an_ambiguous_match() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "dup\ndup\n").unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"path": "f.txt", "old_string": "dup", "new_string": "x"}), &context).await;
    let err = result.expect_err("a match occurring twice should fail");
    assert!(err.to_string().contains("2 locations"));
}

/// The file has a real curly/smart quote (as an editor's autocorrect or a
/// copy-paste from a rendered doc would produce); the model's old_string
/// uses the plain ASCII apostrophe instead. The exact match fails, but the
/// lookalike-normalized fallback should still find the unique match and
/// replace the right span in the *original* (still-curly-quoted) text.
#[tokio::test]
async fn edit_file_falls_back_to_a_lookalike_normalized_match() {
    let dir = tempdir().unwrap();
    let old = "it\u{2019}s working\nline2\n";
    std::fs::write(dir.path().join("f.txt"), old).unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"path": "f.txt", "old_string": "it's working", "new_string": "it's CHANGED"}), &context).await.unwrap();
    assert!(result.contains("edited"));
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "it's CHANGED\nline2\n");
}

#[tokio::test]
async fn list_dir_lists_non_recursive_and_recursive() {
    let dir = tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    std::fs::write(dir.path().join("sub").join("b.txt"), "y").unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("list_dir").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let shallow = tool.execute(json!({}), &context).await.unwrap();
    assert!(shallow.contains("a.txt"));
    assert!(shallow.contains("sub/"));
    assert!(!shallow.contains("b.txt"));

    let deep = tool.execute(json!({"recursive": true}), &context).await.unwrap();
    assert!(deep.contains("b.txt"));
}

#[tokio::test]
async fn search_regex_finds_matches_and_respects_glob() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn needle() {}\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "needle in a text file\n").unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("search_regex").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let all = tool.execute(json!({"pattern": "needle"}), &context).await.unwrap();
    assert!(all.contains("a.rs"));
    assert!(all.contains("b.txt"));

    let rs_only = tool.execute(json!({"pattern": "needle", "file_glob": "*.rs"}), &context).await.unwrap();
    assert!(rs_only.contains("a.rs"));
    assert!(!rs_only.contains("b.txt"));
}

#[tokio::test]
async fn search_semantic_finds_rust_function_by_name() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), "fn find_me_please() {}\nstruct Other;\n").unwrap();

    let registry = ToolRegistry::new();
    let tool = registry.get("search_semantic").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"query": "find_me"}), &context).await.unwrap();
    assert!(result.contains("find_me_please"));
    assert!(!result.contains("Other"));
}

#[tokio::test]
async fn run_command_times_out_on_a_long_running_process() {
    let dir = tempdir().unwrap();
    let registry = ToolRegistry::new();
    let tool = registry.get("run_command").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let command = if cfg!(windows) { "ping -n 6 127.0.0.1 >NUL" } else { "sleep 5" };
    let result = tool.execute(json!({"command": command, "timeout_secs": 1}), &context).await;
    assert!(result.is_err(), "a 5s command with a 1s timeout should time out");
}

#[tokio::test]
async fn run_command_captures_stdout_and_exit_status() {
    let dir = tempdir().unwrap();
    let registry = ToolRegistry::new();
    let tool = registry.get("run_command").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let command = if cfg!(windows) { "echo hello" } else { "echo hello" };
    let result = tool.execute(json!({"command": command}), &context).await.unwrap();
    assert!(result.contains("hello"));
    assert!(result.contains("exit status"));
}

#[tokio::test]
async fn git_commit_and_git_diff_round_trip() {
    let dir = tempdir().unwrap();
    run_git(dir.path(), &["init"]).await;
    run_git(dir.path(), &["config", "user.email", "test@example.com"]).await;
    run_git(dir.path(), &["config", "user.name", "Test"]).await;

    std::fs::write(dir.path().join("f.txt"), "v1\n").unwrap();

    let registry = ToolRegistry::new();
    let context = ctx(dir.path().to_path_buf());

    let commit_result = registry.get("git_commit").unwrap().execute(json!({"message": "chore: initial commit"}), &context).await.unwrap();
    let _ = commit_result;

    let clean_diff = registry.get("git_diff").unwrap().execute(json!({}), &context).await.unwrap();
    assert_eq!(clean_diff, "no changes");

    std::fs::write(dir.path().join("f.txt"), "v2\n").unwrap();
    let dirty_diff = registry.get("git_diff").unwrap().execute(json!({}), &context).await.unwrap();
    assert!(dirty_diff.contains("v2"));
}

#[tokio::test]
async fn web_fetch_converts_html_to_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html><body><p>Hello there</p></body></html>", "text/html"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let registry = ToolRegistry::new();
    let tool = registry.get("web_fetch").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"url": format!("{}/page", server.uri())}), &context).await.unwrap();
    assert!(result.contains("Hello there"));
    assert!(!result.contains("<p>"));
}

#[tokio::test]
async fn ask_user_round_trips_through_ui_channel() {
    let dir = tempdir().unwrap();
    let registry = ToolRegistry::new();
    let tool = registry.get("ask_user").unwrap();

    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel();
    let context = ToolContext { cwd: dir.path().to_path_buf(), ui_tx: Some(agent_tx), memory: None, snapshots: None };

    let responder = tokio::spawn(async move {
        if let Some(AgentToUi::AskUser { respond_to, .. }) = agent_rx.recv().await {
            let _ = respond_to.send("b".to_string());
        }
    });

    let answer = tool.execute(json!({"question": "pick one", "options": ["a", "b"]}), &context).await.unwrap();
    assert_eq!(answer, "b");
    responder.await.unwrap();
}
