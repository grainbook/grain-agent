//! Tool smoke tests against a temp-directory workspace.

use std::sync::Arc;

use grain_agent_core::{AgentTool, AgentToolResult, UserContent};
use grain_ai_agent_headless::{
    BashTool, EditTool, GlobTool, GrepTool, ListTool, ReadTool, Workspace, WriteTool,
};
use tokio_util::sync::CancellationToken;

fn workspace_with_files(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, content) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, content).expect("write");
    }
    let workspace = Arc::new(Workspace::new(dir.path()).expect("workspace"));
    (dir, workspace)
}

fn no_update() -> grain_agent_core::ToolUpdateCallback {
    Arc::new(|_| {})
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// Workspace path validation
// ---------------------------------------------------------------------------

#[test]
fn workspace_resolves_relative_path() {
    let (_dir, ws) = workspace_with_files(&[("foo.txt", "hi")]);
    let resolved = ws.resolve("foo.txt").expect("resolves");
    assert!(resolved.starts_with(ws.root()));
}

#[test]
fn workspace_rejects_parent_escape() {
    let (_dir, ws) = workspace_with_files(&[("foo.txt", "hi")]);
    let err = ws.resolve("../etc/passwd").unwrap_err();
    let msg = err.to_string();
    // Either the path doesn't exist (NotFound) or canonicalizes outside (Escape).
    // Both are correct rejections — never returning a path outside the root.
    assert!(
        msg.contains("escape") || msg.contains("not found"),
        "unexpected error: {msg}"
    );
}

#[test]
fn workspace_rejects_missing_path() {
    let (_dir, ws) = workspace_with_files(&[]);
    let err = ws.resolve("nonexistent.txt").unwrap_err();
    assert!(err.to_string().contains("not found"));
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_returns_file_contents() {
    let (_dir, ws) = workspace_with_files(&[("hello.txt", "line1\nline2\nline3\n")]);
    let tool = ReadTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "path": "hello.txt" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("line1"));
    assert!(text.contains("line3"));
}

#[tokio::test]
async fn read_applies_offset_and_limit() {
    let body: String = (0..50).map(|i| format!("L{i}\n")).collect();
    let (_dir, ws) = workspace_with_files(&[("big.txt", &body)]);
    let tool = ReadTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "path": "big.txt", "offset": 10, "limit": 5 }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("L10"));
    assert!(text.contains("L14"));
    assert!(!text.contains("L15"), "limit honored");
    assert!(!text.contains("L9"), "offset honored");
}

#[tokio::test]
async fn read_rejects_path_outside_workspace() {
    let (_dir, ws) = workspace_with_files(&[("foo.txt", "x")]);
    let tool = ReadTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "/etc/passwd" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("escape") || msg.contains("not found") || msg.contains("io"),
        "unexpected error: {msg}"
    );
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_shows_directories_first() {
    let (_dir, ws) = workspace_with_files(&[
        ("a.txt", ""),
        ("b.txt", ""),
        ("sub/inside.txt", ""),
    ]);
    let tool = ListTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "path": "." }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    let lines: Vec<&str> = text.lines().collect();
    // First line should be a directory entry (suffix `/`).
    assert!(lines[0].ends_with('/'), "expected dir first, got {lines:?}");
    assert!(text.contains("sub/"));
    assert!(text.contains("a.txt"));
    assert!(text.contains("b.txt"));
}

#[tokio::test]
async fn list_errors_on_file_target() {
    let (_dir, ws) = workspace_with_files(&[("foo.txt", "x")]);
    let tool = ListTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "foo.txt" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a directory"));
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glob_matches_files_by_pattern() {
    let (_dir, ws) = workspace_with_files(&[
        ("src/lib.rs", ""),
        ("src/foo/bar.rs", ""),
        ("docs/intro.md", ""),
    ]);
    let tool = GlobTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "pattern": "**/*.rs" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("src/lib.rs"));
    assert!(text.contains("src/foo/bar.rs"));
    assert!(!text.contains("docs/intro.md"));
}

#[tokio::test]
async fn glob_no_matches_yields_friendly_output() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "")]);
    let tool = GlobTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "pattern": "**/*.nonexistent" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("no matches"));
}

// ---------------------------------------------------------------------------
// Grep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_finds_pattern_with_line_and_column() {
    let (_dir, ws) = workspace_with_files(&[
        ("src/lib.rs", "fn foo() {}\nfn bar() {}\n// TODO: refactor\n"),
        ("src/other.rs", "fn baz() {}\n"),
    ]);
    let tool = GrepTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "pattern": "TODO" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("src/lib.rs:3:"));
    assert!(text.contains("TODO"));
}

#[tokio::test]
async fn grep_honors_file_glob() {
    let (_dir, ws) = workspace_with_files(&[
        ("src/foo.rs", "MATCH\n"),
        ("docs/notes.md", "MATCH\n"),
    ]);
    let tool = GrepTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "pattern": "MATCH", "file_glob": "*.rs" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("src/foo.rs"));
    assert!(!text.contains("docs/notes.md"));
}

#[tokio::test]
async fn grep_invalid_regex_yields_validation_error() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "")]);
    let tool = GrepTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "pattern": "[unclosed" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        grain_agent_core::AgentToolError::Validation(_)
    ));
}

// ---------------------------------------------------------------------------
// Runtime helper
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coding_read_tools_returns_all_four_tools() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "")]);
    let tools = grain_ai_agent_headless::coding_read_tools(ws);
    let names: Vec<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();
    assert_eq!(names, vec!["read", "list", "glob", "grep"]);
}

#[tokio::test]
async fn coding_all_tools_includes_write_and_edit() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "")]);
    let tools = grain_ai_agent_headless::coding_all_tools(ws);
    let names: Vec<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();
    assert_eq!(names, vec!["read", "list", "glob", "grep", "write", "edit"]);
}

// ---------------------------------------------------------------------------
// Workspace write-path validation
// ---------------------------------------------------------------------------

#[test]
fn workspace_resolve_for_write_allows_new_file() {
    let (_dir, ws) = workspace_with_files(&[]);
    let path = ws.resolve_for_write("new.txt").expect("resolves");
    assert!(path.starts_with(ws.root()));
    assert!(path.ends_with("new.txt"));
}

#[test]
fn workspace_resolve_for_write_allows_new_file_in_existing_subdir() {
    let (_dir, ws) = workspace_with_files(&[("sub/anchor.txt", "")]);
    let path = ws.resolve_for_write("sub/new.txt").expect("resolves");
    assert!(path.starts_with(ws.root()));
}

#[test]
fn workspace_resolve_for_write_rejects_missing_parent() {
    let (_dir, ws) = workspace_with_files(&[]);
    let err = ws.resolve_for_write("nonexistent/sub/new.txt").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("io") || msg.contains("not found") || msg.contains("does not exist"));
}

#[test]
fn workspace_resolve_for_write_rejects_outside_root() {
    let (_dir, ws) = workspace_with_files(&[]);
    let err = ws.resolve_for_write("/tmp/somewhere_else/foo.txt").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("escape") || msg.contains("io"));
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_creates_new_file() {
    let (dir, ws) = workspace_with_files(&[]);
    let tool = WriteTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "path": "new.txt", "content": "hello\nworld\n" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("created"));
    let written = std::fs::read_to_string(dir.path().join("new.txt")).expect("file exists");
    assert_eq!(written, "hello\nworld\n");
    assert_eq!(
        result.details.get("created").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn write_overwrites_existing_file() {
    let (dir, ws) = workspace_with_files(&[("a.txt", "old content")]);
    let tool = WriteTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "path": "a.txt", "content": "new content" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("overwrote"));
    let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("file exists");
    assert_eq!(written, "new content");
}

#[tokio::test]
async fn write_rejects_path_with_missing_parent() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = WriteTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "no_such_dir/file.txt", "content": "x" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("io") || msg.contains("not found") || msg.contains("escape"));
}

#[tokio::test]
async fn write_rejects_path_outside_workspace() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = WriteTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "/tmp/escape.txt", "content": "x" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("escape") || msg.contains("io"));
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_performs_search_replace() {
    let (dir, ws) = workspace_with_files(&[("src.rs", "fn foo() {}\n")]);
    let tool = EditTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "path": "src.rs", "old": "fn foo()", "new": "fn bar()" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("edited"));
    let written = std::fs::read_to_string(dir.path().join("src.rs")).expect("file exists");
    assert_eq!(written, "fn bar() {}\n");
    assert_eq!(
        result.details.get("replacements").and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[tokio::test]
async fn edit_rejects_no_op() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "x")]);
    let tool = EditTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "a.txt", "old": "x", "new": "x" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        grain_agent_core::AgentToolError::Validation(_)
    ));
    assert!(err.to_string().contains("no-op"));
}

#[tokio::test]
async fn edit_fails_when_old_missing() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "nothing here")]);
    let tool = EditTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "a.txt", "old": "absent", "new": "x" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("expected 1 occurrence(s)"));
}

#[tokio::test]
async fn edit_enforces_expected_occurrences() {
    // File has 3 occurrences; passing expected_occurrences=1 should fail loudly.
    let (_dir, ws) = workspace_with_files(&[("a.txt", "TODO foo\nTODO bar\nTODO baz\n")]);
    let tool = EditTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "path": "a.txt", "old": "TODO", "new": "DONE" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("found 3"));
}

#[tokio::test]
async fn edit_with_explicit_count_succeeds() {
    let (dir, ws) = workspace_with_files(&[("a.txt", "X\nX\nX\n")]);
    let tool = EditTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({
                "path": "a.txt", "old": "X", "new": "Y", "expected_occurrences": 3
            }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("file exists");
    assert_eq!(written, "Y\nY\nY\n");
    assert_eq!(
        result.details.get("replacements").and_then(|v| v.as_u64()),
        Some(3)
    );
}

// ---------------------------------------------------------------------------
// Bash (Unix-only — /bin/sh is required)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn bash_runs_simple_echo() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = BashTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "command": "echo hello" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("hello"));
    assert_eq!(result.details.get("exitCode").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(result.details.get("success").and_then(|v| v.as_bool()), Some(true));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_captures_stderr_in_combined_output() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = BashTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "command": "echo to-out; echo to-err 1>&2" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("to-out"), "stdout in output: {text}");
    assert!(text.contains("to-err"), "stderr in output: {text}");
    assert!(text.contains("--- stderr ---"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_reports_nonzero_exit_as_success_false() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = BashTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "command": "exit 7" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("tool result returned even for non-zero exit");
    assert_eq!(result.details.get("exitCode").and_then(|v| v.as_i64()), Some(7));
    assert_eq!(result.details.get("success").and_then(|v| v.as_bool()), Some(false));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_honors_workspace_cwd() {
    let (_dir, ws) = workspace_with_files(&[("sub/marker.txt", "anchor")]);
    let tool = BashTool::new(ws);
    let result = tool
        .execute(
            "c",
            serde_json::json!({ "command": "pwd && ls", "cwd": "sub" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .expect("ok");
    let text = text_of(&result);
    assert!(text.contains("marker.txt"), "cwd switched to sub/: {text}");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_rejects_cwd_outside_workspace() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = BashTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "command": "true", "cwd": "/tmp" }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("escape") || msg.contains("io") || msg.contains("not found"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_times_out_quickly() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = BashTool::new(ws);
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "command": "sleep 5", "timeout_ms": 100 }),
            CancellationToken::new(),
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_aborts_on_cancel() {
    let (_dir, ws) = workspace_with_files(&[]);
    let tool = BashTool::new(ws);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel_for_task.cancel();
    });
    let err = tool
        .execute(
            "c",
            serde_json::json!({ "command": "sleep 5" }),
            cancel,
            no_update(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, grain_agent_core::AgentToolError::Aborted));
}

#[tokio::test]
async fn coding_full_tools_includes_bash() {
    let (_dir, ws) = workspace_with_files(&[("a.txt", "")]);
    let tools = grain_ai_agent_headless::coding_full_tools(ws);
    let names: Vec<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();
    assert_eq!(
        names,
        vec!["read", "list", "glob", "grep", "write", "edit", "bash"]
    );
}
