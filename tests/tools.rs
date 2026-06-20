use serde_json::json;
use tempfile::tempdir;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cog::tools::{ToolContext, ToolRegistry};
use cog::tui::AgentToUi;

fn ctx(cwd: std::path::PathBuf) -> ToolContext {
    ToolContext { cwd, ui_tx: None, memory: None }
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
async fn edit_file_applies_multi_hunk_diff() {
    let dir = tempdir().unwrap();
    let old = "line1\nline2\nline3\nline4\nline5\n";
    let new = "line1\nCHANGED\nline3\nline4\nADDED\nline5\n";
    std::fs::write(dir.path().join("f.txt"), old).unwrap();

    let diff = diffy::create_patch(old, new).to_string();
    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"path": "f.txt", "diff": diff}), &context).await.unwrap();
    assert!(result.contains("applied diff"));
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), new);
}

#[tokio::test]
async fn edit_file_preserves_crlf_line_endings() {
    let dir = tempdir().unwrap();
    let old_lf = "line1\nline2\nline3\n";
    let new_lf = "line1\nCHANGED\nline3\n";
    let old_crlf = old_lf.replace('\n', "\r\n");
    std::fs::write(dir.path().join("f.txt"), &old_crlf).unwrap();

    let diff = diffy::create_patch(old_lf, new_lf).to_string();
    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    tool.execute(json!({"path": "f.txt", "diff": diff}), &context).await.unwrap();
    let result = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(result, new_lf.replace('\n', "\r\n"));
}

#[tokio::test]
async fn edit_file_rejects_a_diff_that_does_not_apply() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "actual content here\n").unwrap();

    // A diff generated against completely different content won't match
    // the file's context lines.
    let diff = diffy::create_patch("totally different\nbase content\n", "totally different\nedited content\n").to_string();
    let registry = ToolRegistry::new();
    let tool = registry.get("edit_file").unwrap();
    let context = ctx(dir.path().to_path_buf());

    let result = tool.execute(json!({"path": "f.txt", "diff": diff}), &context).await;
    assert!(result.is_err(), "a non-matching diff should fail to apply");
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
    let context = ToolContext { cwd: dir.path().to_path_buf(), ui_tx: Some(agent_tx), memory: None };

    let responder = tokio::spawn(async move {
        if let Some(AgentToUi::AskUser { respond_to, .. }) = agent_rx.recv().await {
            let _ = respond_to.send("b".to_string());
        }
    });

    let answer = tool.execute(json!({"question": "pick one", "options": ["a", "b"]}), &context).await.unwrap();
    assert_eq!(answer, "b");
    responder.await.unwrap();
}
