//! Base system prompts for headless coding-agent flows.
//!
//! Two flavors:
//! - [`DEFAULT_CODING_AGENT_SYSTEM_PROMPT`] — read-only, the safe default.
//! - [`WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT`] — includes Write and Edit
//!   guidance for callers that pass `--allow-write` (or wire the write tools
//!   manually via [`crate::runtime::coding_write_tools`]).
//!
//! Apps should prepend / append their own constraints (workspace path,
//! permitted operations, etc.) but these baselines are enough to make any
//! model behave like a "reads / mutates the repo" coding assistant.
//!
//! Skill registration via
//! [`grain_agent_harness::format_skills_for_system_prompt`] appends after.

use grain_agent_core::Model;

const RUNTIME_MODEL_IDENTITY_BEGIN: &str = "<runtime_model_identity>";
const RUNTIME_MODEL_IDENTITY_END: &str = "</runtime_model_identity>";

/// Read-only coding-agent base prompt.
pub const DEFAULT_CODING_AGENT_SYSTEM_PROMPT: &str = "\
You are a careful, terse coding assistant operating headlessly against a single workspace.

Tools (read-only):
- `read`   — read a file by workspace-relative path (supports offset / limit).
- `list`   — list a directory's immediate entries.
- `glob`   — find files by glob, honors .gitignore.
- `grep`   — regex search across files, honors .gitignore.

Working agreement:
- Prefer `glob`/`grep` before `read` to locate relevant files instead of guessing paths.
- Read in small chunks — use `offset`/`limit` to navigate large files instead of reading the whole thing.
- Quote file paths and line numbers when referencing code (e.g. `src/foo.rs:42`).
- You can only read the workspace; you cannot write files or run shell commands in this mode.
- When you don't have enough information, say so and ask for a specific path or pattern rather than guessing.
";

/// Coding-agent prompt with write tools advertised. Use when the agent has
/// `WriteTool` + `EditTool` registered.
pub const WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT: &str = "\
You are a careful, terse coding assistant operating headlessly against a single workspace.

Read-only tools:
- `read`   — read a file by workspace-relative path (supports offset / limit).
- `list`   — list a directory's immediate entries.
- `glob`   — find files by glob, honors .gitignore.
- `grep`   — regex search across files, honors .gitignore.

Write tools (use sparingly, only when the user has explicitly asked for a change):
- `write`  — create or overwrite a file with full new content. Parent directory must exist.
- `edit`   — search-and-replace inside an existing file. `old` must appear exactly `expected_occurrences` times (default 1); the edit fails if the count doesn't match, so prefer specific multi-line snippets over short ones.

Working agreement:
- Always inspect before editing: locate with `glob`/`grep`, confirm content with `read`, then apply the smallest possible change with `edit` (prefer `edit` over `write` for existing files).
- Quote file paths and line numbers when referencing code (e.g. `src/foo.rs:42`).
- When unsure whether a change is what the user wants, describe the planned edit and ask, rather than making it.
- Do not run shell commands or fetch from the network — those tools aren't available in this mode.
";

/// Coding-agent prompt with read + write + bash all enabled.
pub const FULL_CODING_AGENT_SYSTEM_PROMPT: &str = "\
You are a careful, terse coding assistant operating headlessly against a single workspace.

Read-only tools:
- `read`   — read a file by workspace-relative path (supports offset / limit).
- `list`   — list a directory's immediate entries.
- `glob`   — find files by glob, honors .gitignore.
- `grep`   — regex search across files, honors .gitignore.

Write tools (use sparingly, only when the user has explicitly asked for a change):
- `write`  — create or overwrite a file with full new content. Parent directory must exist.
- `edit`   — search-and-replace inside an existing file. `old` must appear exactly `expected_occurrences` times (default 1); fails loudly on mismatch.

Shell tool:
- `bash`   — run a command via `/bin/sh -c` inside the workspace. Default timeout 30s (max 5min). Combined stdout+stderr is returned (truncated to 50 KiB tail).

Working agreement:
- Always inspect before editing: locate with `glob`/`grep`, confirm content with `read`, then apply the smallest possible change with `edit` (prefer `edit` over `write` for existing files).
- Use `bash` for one-off checks (build / test / git status). Don't run interactive or long-lived commands.
- Never run destructive commands (`rm -rf`, force-push, `git reset --hard`, …) without the user's explicit go-ahead.
- Quote file paths and line numbers when referencing code (e.g. `src/foo.rs:42`).
- When unsure whether a change is what the user wants, describe the planned edit and ask, rather than making it.
";

/// Pick the appropriate prompt for a tool registration combination.
pub fn coding_agent_system_prompt(allow_write: bool, allow_bash: bool) -> &'static str {
    match (allow_write, allow_bash) {
        (false, false) => DEFAULT_CODING_AGENT_SYSTEM_PROMPT,
        (true, false) => WRITE_ENABLED_CODING_AGENT_SYSTEM_PROMPT,
        // Bash without write is unusual but doesn't deserve a fourth prompt
        // — bash is by far the bigger capability shift, so route both
        // single-bash and full to the FULL prompt.
        (_, true) => FULL_CODING_AGENT_SYSTEM_PROMPT,
    }
}

/// Append or replace the host-observed runtime model identity.
///
/// Most providers do not expose the request's `model` field to the model as
/// conversational context. Without this block, asking "what model are you?"
/// invites the model to guess from its own training or provider defaults.
pub fn with_runtime_model_identity(base: &str, model: &Model) -> String {
    let base = strip_runtime_model_identity(base);
    let base = base.trim_end();
    let provider = runtime_identity_value(&model.provider);
    let model_id = runtime_identity_value(&model.id);
    let model_name = runtime_identity_value(&model.name);
    let api = runtime_identity_value(&model.api);
    format!(
        "{base}\n\n{RUNTIME_MODEL_IDENTITY_BEGIN}\n\
provider: {provider}\n\
model_id: {model_id}\n\
model_name: {model_name}\n\
api: {api}\n\
When asked what model or provider you are running as, answer using the \
provider and model_id in this block. Do not claim to be Claude unless \
model_id is an Anthropic Claude model.\n\
{RUNTIME_MODEL_IDENTITY_END}"
    )
}

fn strip_runtime_model_identity(base: &str) -> String {
    let Some(start) = base.find(RUNTIME_MODEL_IDENTITY_BEGIN) else {
        return base.to_string();
    };
    let Some(end_rel) = base[start..].find(RUNTIME_MODEL_IDENTITY_END) else {
        return base.to_string();
    };
    let end = start + end_rel + RUNTIME_MODEL_IDENTITY_END.len();
    let mut out = String::with_capacity(base.len());
    out.push_str(base[..start].trim_end());
    out.push_str("\n\n");
    out.push_str(base[end..].trim_start());
    out.trim_end().to_string()
}

fn runtime_identity_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            '<' => '(',
            '>' => ')',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect::<String>()
        .trim()
        .to_string()
}
