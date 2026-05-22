# Config file

`grain-headless` reads optional TOML config from two locations. Loading is layered: workspace overrides user, CLI flags override both.

中文版：[zh/config.md](./zh/config.md).

## Locations

1. **CLI flag** (highest priority; non-overridable by config).
2. **Workspace**: `<workspace>/.grain/config.toml`.
3. **User XDG**: `~/.config/grain/config.toml` (or platform equivalent via the `dirs` crate).
4. **Built-in defaults** (clap `default_value_t`).

A missing file at any layer is fine — it falls through to the next layer.

## Schema

Every field optional. Unknown fields are rejected (so typos surface immediately).

```toml
# Pick a default model
model = "anthropic/claude-sonnet-4-5"

# Reserved tokens for system prompt + completion when context-guard truncates
headroom_tokens = 4096

# Show LLM's thinking deltas (dim text) inline
show_thinking = false

# "none" or "common" (Kimi + SiliconFlow OpenAI-compat preset)
openai_compat = "common"

# Capability gates — set true to enable by default
allow_write = false
allow_bash = false
allow_web = false
allow_semantic_search = false

# Override the skill directory (default <workspace>/.claude/skills)
skills_dir = ".claude/skills"
```

## Consolidated plugin / provider declarations

Two more block kinds are accepted, so the workspace doesn't need a
fan-out of single-purpose TOMLs:

```toml
[[plugin]]
name = "lazy-gagent"
src  = "../lazy-gagent"

[[plugin]]
name = "rust-helper"
src  = "https://github.com/me/rust-helper.git"
rev  = "v1.0.0"

[[provider]]
name  = "anthropic"
kind  = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth  = { kind = "api_key", env = "ANTHROPIC_API_KEY" }
```

These have the same field shape as the legacy `.grain/plugin-spec.toml`
`[[plugin]]` blocks and `.grain/providers.toml` `[[profile]]` blocks
respectively. Both files are still read for back-compat — see [plugins.md](./plugins.md)
and [providers.md](./providers.md). When a name appears in both,
**`config.toml` wins**.

### plugin-lock.toml

Runtime plugin operations (`lazy_install`, `lazy_remove`, the
`/install` / `/remove` slash commands) **never edit `config.toml`**
— they write to `<workspace>/.grain/plugin-lock.toml`, a separate
file with the same `[[plugin]]` shape. This keeps your hand-written
declarations intact while still letting the agent install / remove
plugins on demand.

The boot-time effective spec is the union:

1. `config.toml` `[[plugin]]` blocks (declarative; user-authored).
2. `plugin-lock.toml` (auto-managed; runtime install / remove).
3. Legacy `plugin-spec.toml` (deprecated; engine still reads and
   mutates it for back-compat).

First-source-wins on name collision. Trying to `lazy_remove` an
entry that lives in `config.toml` is refused with a "edit the file
directly" message — that's the whole point of keeping declarative
state separate from runtime state.

## Explicit-vs-default semantics

The CLI uses clap's `value_source()` API to tell "user explicitly set this on the command line" from "user accepted the default":

- If the user passed `--allow-bash` (even with the value `false`), the config's `allow_bash` is ignored.
- If the user didn't pass it, the config's value wins — including `allow_bash = false`, which keeps it off.

This means config booleans are **bidirectional**: you can both enable *and* disable from config without surprising fallbacks.

## Example: per-project policy

`~/code/risky-project/.grain/config.toml`:

```toml
# Tighter defaults for a sensitive project
allow_write = false
allow_bash = false
allow_web = false
headroom_tokens = 8192
model = "anthropic/claude-haiku-4-5"  # cheaper model, sufficient here
```

`~/code/sandbox/.grain/config.toml`:

```toml
# Throw-away playground — let it do anything
allow_write = true
allow_bash = true
allow_web = true
```

`~/.config/grain/config.toml`:

```toml
# Global defaults
show_thinking = true
openai_compat = "common"
```
