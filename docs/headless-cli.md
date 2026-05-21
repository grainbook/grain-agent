# `grain-headless` CLI reference

`grain-headless` is the binary that ships with the `grain-ai-agent-headless` crate. It's a ready-to-run coding agent built on top of the rest of the workspace; this page is the canonical reference for its flags and behavior.

中文版：[zh/headless-cli.md](./zh/headless-cli.md).

## Build and install

```bash
cargo build --release -p grain-ai-agent-headless --bin grain-headless
```

Binary at `target/release/grain-headless`. Symlink onto `$PATH` for convenience.

To enable the optional `semantic_search` tool (rig + OpenAI embeddings), build with `--features rig`:

```bash
cargo build --release -p grain-ai-agent-headless --bin grain-headless --features rig
```

## One-line usage

```bash
grain-headless -C ./my-project --prompt "What does main.rs do?"
```

The agent runs read-only by default and prints a streaming event log to stdout.

## Flags

### Workspace and prompt

| Flag | Default | Notes |
|------|---------|-------|
| `-C, --workspace <path>` | `.` | Workspace root; all file tools refuse to read / write outside it |
| `-p, --prompt <text>` | (stdin) | The user message. Omitted → read from stdin |
| `--system-prompt-file <path>` | (built-in) | Override the default system prompt |

### Model + LLM provider

| Flag | Default | Notes |
|------|---------|-------|
| `-m, --model <id>` | `anthropic/claude-sonnet-4-5` | Any id from the embedded models.dev snapshot ([llm-models.md](./llm-models.md)) |
| `--openai-compat <none\|common>` | `common` | Pre-register Kimi / SiliconFlow as OpenAI-compatible endpoints |
| `--headroom-tokens <n>` | `4096` | Reserved tokens for system prompt + completion when context-guard truncates |
| `--show-thinking` | off | Print thinking-block deltas in dim text |

### Capability gates (all default off)

| Flag | Adds |
|------|------|
| `--allow-write` | `write` + `edit` tools |
| `--allow-bash` | `bash` tool (runs `/bin/sh -c`, kill-on-drop, configurable timeout) |
| `--allow-web` | `web_fetch` tool (HTTPS GET with SSRF guard + redirect cap) |
| `--allow-semantic-search` | `semantic_search` tool (requires `--features rig` and `OPENAI_API_KEY`) |

### Interactive + persistence

| Flag | Notes |
|------|-------|
| `-i, --interactive` | Read-prompt-respond loop. Type `/help` for slash commands; `/exit` / Ctrl-D to leave |
| `--session <path>` | JSONL transcript across runs. Loaded on startup, appended per message |
| `--telemetry-file <path>` | Opt-in audit log (one JSON line per event). See [telemetry.md](./telemetry.md) for sensitive-data warning |
| `--skills-dir <path>` | Override the default `<workspace>/.claude/skills` skills directory |

### Output / diagnostics

| Flag | Notes |
|------|-------|
| `--output <text\|json>` | `text` is human; `json` is one event per line (pipe to `jq`) |
| `--doctor` | Print workspace + provider + git diagnostic and exit 0; no LLM calls |

## Slash commands (interactive only)

| Command | Effect |
|---------|--------|
| `/help` | Show built-in help |
| `/clear` (or `/reset`) | Reset in-memory transcript **and** truncate the `--session` file if set |
| `/skills` | List discovered skills |
| `/doctor` | Same as `--doctor` but inline |
| `/source` (or `/git`) | Show workspace git source info |
| `/compact` | Placeholder (real compaction wires via `compaction_prepare_next_turn` API) |
| `/exit` (or `/quit`, `/q`) | Leave the loop |

Anything not starting with `/` is sent to the LLM as the next prompt.

## Skills

Place a `SKILL.md` per directory under `<workspace>/.claude/skills/<name>/`. Minimal frontmatter:

```markdown
---
name: rust-helper
description: Help with Rust code; prefer cargo check over edits when verifying changes.
disable_model_invocation: false
---

(full skill body — read by the agent on demand)
```

Discovered skills are auto-appended to the system prompt as an `<available_skills>` block (see [harness-system-prompt.md](./harness-system-prompt.md)).

Symlinked skill directories / SKILL.md files are refused for safety.

## Config file

Optional TOML config at `<workspace>/.grain/config.toml` and/or `~/.config/grain/config.toml`. Workspace overrides user; CLI flags override both. Every field is optional.

```toml
model = "anthropic/claude-sonnet-4-5"
headroom_tokens = 4096
show_thinking = false
openai_compat = "common"
allow_write = false
allow_bash = false
allow_web = false
allow_semantic_search = false
skills_dir = ".claude/skills"
```

Boolean fields are honored in both directions — `allow_bash = false` in config will turn the flag **off** unless the user passes `--allow-bash` on the command line.

## Environment variables

The genai builder auto-detects API keys by provider:

| Provider | Variable |
|---------|----------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google | `GEMINI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| xAI | `XAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| Cohere | `COHERE_API_KEY` |
| Kimi (Moonshot) | `MOONSHOT_API_KEY` |
| SiliconFlow | `SILICONFLOW_API_KEY` |
| Zhipu (BigModel) | `ZHIPU_API_KEY` |

Run `grain-headless --doctor` to see which keys are detected in your shell.
