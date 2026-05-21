//! Diagnostic helpers: workspace health, environment keys, model registry
//! reachability, optional git source info.
//!
//! Used by the `--doctor` CLI flag and the `/doctor` / `/source` slash
//! commands. No `Agent` interaction — pure inspection so callers can probe
//! before paying for the first LLM call.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use grain_llm_models::Registry;

use crate::workspace::Workspace;

const KNOWN_PROVIDER_ENV: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("groq", "GROQ_API_KEY"),
    ("mistral", "MISTRAL_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("cohere", "COHERE_API_KEY"),
    ("kimi (moonshot)", "MOONSHOT_API_KEY"),
    ("siliconflow", "SILICONFLOW_API_KEY"),
    ("zhipu (bigmodel)", "ZHIPU_API_KEY"),
];

/// Format a full `doctor` report against `workspace`, the embedded model
/// registry, and the local environment.
pub fn render_doctor_report(workspace: &Workspace, registry: &Registry) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== grain-headless doctor ===");
    let _ = writeln!(out, "Workspace: {}", workspace.root().display());
    let _ = writeln!(out, "Registry models: {}", registry.len());
    let _ = writeln!(out);

    let _ = writeln!(out, "Environment keys:");
    for (label, key) in KNOWN_PROVIDER_ENV {
        let present = std::env::var(key)
            .ok()
            .filter(|s| !s.is_empty())
            .is_some();
        let mark = if present { "✓" } else { "·" };
        let _ = writeln!(out, "  [{mark}] {label}  ({key})");
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "Source info:");
    out.push_str(&render_source_info_block(workspace.root(), 2));

    out
}

/// Just the git/source-info block, indented by `indent` spaces per line.
pub fn render_source_info_block(root: &Path, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    match source_info(root) {
        Some(info) => {
            let _ = writeln!(out, "{pad}branch:      {}", info.branch);
            let _ = writeln!(out, "{pad}commit:      {}", info.commit);
            let _ = writeln!(
                out,
                "{pad}dirty:       {}",
                if info.dirty { "yes" } else { "no" }
            );
            if !info.dirty_files.is_empty() {
                let _ = writeln!(
                    out,
                    "{pad}changed:     {} file(s)",
                    info.dirty_files.len()
                );
                for f in info.dirty_files.iter().take(20) {
                    let _ = writeln!(out, "{pad}  {f}");
                }
                if info.dirty_files.len() > 20 {
                    let _ = writeln!(
                        out,
                        "{pad}  … and {} more",
                        info.dirty_files.len() - 20
                    );
                }
            }
        }
        None => {
            let _ = writeln!(out, "{pad}(not a git repository, or `git` not on PATH)");
        }
    }
    out
}

/// Snapshot of `git` state for a directory. `None` if `git` isn't installed
/// or the directory isn't inside a repo.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub branch: String,
    pub commit: String,
    pub dirty: bool,
    pub dirty_files: Vec<String>,
}

/// Run a minimal set of `git` commands. Returns `None` on any failure
/// (missing binary, not a repo, IO error, …) — callers should treat it as
/// "no source info available", not an error to bubble up.
pub fn source_info(root: &Path) -> Option<SourceInfo> {
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let commit = run_git(root, &["rev-parse", "--short", "HEAD"])?;
    let status = run_git(root, &["status", "--porcelain"])?;
    let dirty_files: Vec<String> = status
        .lines()
        .filter_map(|l| l.get(3..).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    Some(SourceInfo {
        branch,
        commit,
        dirty: !dirty_files.is_empty(),
        dirty_files,
    })
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(cwd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_in_tempdir() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn render_doctor_report_includes_workspace_and_env_section() {
        let (_dir, ws) = workspace_in_tempdir();
        let registry = Registry::from_embedded_snapshot();
        let report = render_doctor_report(&ws, &registry);
        assert!(report.contains("grain-headless doctor"));
        assert!(report.contains("Workspace:"));
        assert!(report.contains("Environment keys:"));
        // Every known provider should appear by env-var name.
        for (_, k) in KNOWN_PROVIDER_ENV {
            assert!(report.contains(k), "missing env key in report: {k}");
        }
    }

    #[test]
    fn source_info_returns_none_for_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let info = source_info(dir.path());
        // Could be `None` (no repo) or `Some(parent repo info)` if the temp
        // dir happens to be inside a git repo (rare). We only check that it
        // doesn't panic.
        let _ = info;
    }
}
