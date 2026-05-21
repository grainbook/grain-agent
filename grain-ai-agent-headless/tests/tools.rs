//! Tool smoke tests against a temp-directory workspace.

use std::sync::Arc;

use grain_agent_core::{AgentTool, AgentToolResult, UserContent};
use grain_ai_agent_headless::{GlobTool, GrepTool, ListTool, ReadTool, Workspace};
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
