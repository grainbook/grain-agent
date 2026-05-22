# Plugin system (`lazy.gagent`)

A Neovim/lazy.nvim-style plugin layer for the grain agent. Drop a directory under `<workspace>/.grain/plugins/<name>/` and it can ship skills, themes, system-prompt fragments, and JS-scripted tools — all auto-discovered at startup, no rebuild required.

中文版：[zh/plugins.md](./zh/plugins.md)。

---

## Mental model

| Role | Crate | Analogue |
|---|---|---|
| **Engine** — manifest format, discovery, integration into the agent boot path | `grain-ai-agent-headless::plugins` | Neovim core |
| **UI** — `/plugins` overlay, theme picker | `grain-ai-agent-tui` | the user's terminal |
| **Manager** — install/update/remove plugins (Phase C) | `lazy-gagent` | lazy.nvim |

The engine has zero knowledge of the manager: the manager will eventually be just another `<workspace>/.grain/plugins/lazy-gagent/` directory that ships skills + JS tools implementing plugin management commands. Today the `lazy-gagent` crate is a placeholder that re-exports headless types.

---

## Directory layout

A plugin is **any directory under `<workspace>/.grain/plugins/<name>/` containing a `plugin.toml` manifest**. Other files / subdirectories are picked up by convention:

```text
<workspace>/.grain/plugins/<name>/
  plugin.toml              # required — identifies the plugin
  skills/<skill>/SKILL.md  # optional — merged into find_skills
  themes/<theme>.toml      # optional — picked up by the TUI theme list
  prompts/*.md             # optional — appended to the system prompt
  scripts/*.js             # optional — Boa scripts (needs `scripts-boa`)
```

Discovery rules:
- Subdirectories without `plugin.toml` are skipped silently.
- Hidden (`.foo`) and scratch (`_cache`) directories are skipped.
- A malformed `plugin.toml` emits a `[warn]` line on stderr; **other plugins continue to load** (corruption in one never breaks the rest).
- Plugins are sorted alphabetically by manifest name so the startup log + `/plugins` overlay are deterministic.

The default directory `<workspace>/.grain/plugins/` can be overridden with `--plugins-dir <PATH>` on `grain-tui`. Headless library callers pass the path directly to `discover_plugins(...)`.

---

## `plugin.toml`

Minimal manifest:

```toml
name = "rust-helper"
```

Full manifest (every field optional except `name`):

```toml
name = "rust-helper"
version = "0.1.0"
description = "Rust-specific skills + system prompt rules"
author = "you"
```

Empty fields decay to empty strings — Phase A doesn't impose any further schema, so adding a `dependencies = [...]` later won't break old manifests.

---

## What each subdirectory contributes

### `skills/<name>/SKILL.md`

Folded into the agent's skill catalog via `find_skills_with_plugins(primary_dir, plugins)`. The skill file format is the same as the workspace's `<workspace>/.claude/skills/` — see [harness-system-prompt.md](./harness-system-prompt.md) and the engine's `find_skills` for the precise SKILL.md frontmatter contract.

The TUI's slash palette and `/skills` overlay list plugin-supplied skills alongside the workspace's own. Names are not namespaced today (Phase B); if two plugins ship a `lint` skill, the last one wins. Future work: prefix display with `<plugin>/`.

### `themes/<name>.toml`

Each `.toml` file under `<plugin>/themes/` is parsed by `grain-ai-agent-tui`'s theme loader (same code path as `<workspace>/.grain/themes/`). The theme picker `/theme` lists plugin themes mixed with built-ins and user themes; activation is persisted via `tui-state.toml` so the next launch resumes the same theme.

### `prompts/*.md`

Each `.md` file gets read at boot and appended to the base system prompt, in sort order, with a banner:

```text
<base prompt>

## Plugin: <plugin-name>

<contents of prompts/01-rules.md>

## Plugin: <plugin-name>

<contents of prompts/02-style.md>
```

The composition happens **before** the harness pins the system prompt, so the LLM sees plugin rules as part of the canonical prefix and the upstream prefix cache (Anthropic / OpenAI / DeepSeek …) stays warm across turns.

Use this for plugins that need to teach the model domain rules — "always run clippy before committing", "format with `cargo fmt --check`", "respect the existing axum router layout", etc.

### `scripts/*.js`

When the TUI is built with `--features scripts-boa`, every `<plugin>/scripts/*.js` is loaded into the **same** Boa worker as the workspace's primary `<workspace>/.grain/scripts/` directory. All tools registered via `grain.register_tool({...})` end up exposed to the same agent.

This is enabled by the new `BoaExtension::from_scripts_dirs(&[...])` constructor; see [scripting.md](./scripting.md) for the JS API. Load order: workspace primary first, then each plugin in discovery order; later registrations of the same tool name win.

---

## CLI

`grain-tui` flags relevant to plugins:

```text
--plugins-dir <DIR>   # default: <workspace>/.grain/plugins
```

`grain-headless` library callers can do plugin discovery directly:

```rust
use grain_ai_agent_headless as h;
let dir = h::default_plugins_dir(workspace_root);
let plugins = h::discover_plugins(&dir);
for p in &plugins { eprintln!("{}", h::summarize_plugin(p)); }

// Compose the system prompt + skills as the TUI does.
let prompt = h::compose_system_prompt_with_plugins(base_prompt, &plugins);
let skills = h::find_skills_with_plugins(&skills_dir, &plugins)?;
```

---

## In-TUI surface

| Slash command | Effect |
|---|---|
| `/plugins` | Read-only overlay listing all discovered plugins (manifest name + version + description + per-subdir counts) |
| `/skills` | Skills from plugins appear alongside workspace skills |
| `/theme` | Themes from plugins appear in the picker |

Phase B is read-only by design — install / enable / disable land in Phase C alongside the `lazy-gagent` plugin manager.

---

## End-to-end example

```text
<workspace>/.grain/plugins/rust-helper/
├── plugin.toml         # name = "rust-helper"
├── skills/
│   └── clippy/SKILL.md
├── themes/
│   └── rust-night.toml
├── prompts/
│   ├── 01-rules.md     # "Always run cargo clippy before commit."
│   └── 02-style.md
└── scripts/
    └── cargo-helper.js # grain.register_tool({ name: "cargo_check", ... })
```

Boot output:
```text
[info] plugin 'rust-helper' (skills: 1, themes: 1, scripts: 1, prompts: 2)
[info] system prompt pinned (... bytes, ...)
[info] loaded 1 JS tool(s) from 2 dir(s) (1 from plugins)
```

Inside the TUI:
- `/plugins` shows the `rust-helper` card with counts.
- `/skills` lists `clippy` (and any other plugin / workspace skills).
- `/theme` shows `rust-night`.
- The system prompt that the LLM sees ends with `## Plugin: rust-helper\n\nAlways run cargo clippy before commit.`.
- The model can call the `cargo_check` tool registered by `cargo-helper.js`.

---

## Phase status

| Phase | Scope | Status |
|---|---|---|
| **A** | Discovery + skills/themes merging | ✓ Shipped |
| **B-1** | Scripts merged into one Boa worker | ✓ Shipped |
| **B-2** | `/plugins` overlay UI | ✓ Shipped |
| **B-3** | `prompts/*.md` appended to system prompt | ✓ Shipped |
| **C** | `lazy-gagent` manager: install / update / remove via git, `plugin-spec.toml` | Planned |

---

## Module layout

- [`grain-ai-agent-headless::plugins`](../grain-ai-agent-headless/src/plugins.rs) — manifest types, discovery, integration helpers (`find_skills_with_plugins`, `compose_system_prompt_with_plugins`, `plugin_script_dirs`, `PromptFragment`)
- [`grain-script-boa`](../grain-script-boa/src/extension.rs) — `BoaExtension::from_scripts_dirs(&[Path])` for multi-dir JS loading
- [`grain-ai-agent-tui`](../grain-ai-agent-tui/src/agent_worker.rs) — wiring in `spawn()` (prompt + skills + scripts), `/plugins` overlay in `app.rs` + `ui.rs`
- [`lazy-gagent`](../lazy-gagent/src/lib.rs) — placeholder crate for future Phase C manager
