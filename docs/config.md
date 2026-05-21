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
