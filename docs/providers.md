# Provider profiles

Provider profiles let one binary talk to multiple LLM vendors, multiple accounts per vendor, or multiple subscription paths — all configured through a single TOML file rather than juggling env vars.

This lives in [`grain-llm-genai`](./llm-genai.md) as a first-class capability, then both `grain-headless` ([CLI reference](./headless-cli.md)) and `grain-tui` (the `/provider` overlay) consume it.

中文版：[zh/providers.md](./zh/providers.md).

---

## Why profiles

The bare `grain-llm-genai` builder already knows how to call major vendors via env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …). Profiles add three things on top:

1. **Multiple accounts per vendor.** Declare `openai-work` and `openai-personal` as two `openai-compat` profiles with their own env-var names and you can switch between them at runtime.
2. **Custom-host vendors.** Any OpenAI-compatible endpoint (DeepSeek, MiniMax, OpenRouter, Together, Fireworks, a self-hosted vLLM) becomes a one-line TOML entry.
3. **Subscription auth (Phase 2).** OAuth profiles like Claude Pro / Max are *parsed* today and listed in the picker; selecting one currently surfaces a clear "login flow not yet wired" message. The browser-callback + token-refresh implementation lands as a follow-up.

---

## File location

The loader searches in this order; the first existing file wins:

1. `--providers-file <path>` (CLI override).
2. `<workspace>/.grain/providers.toml` (per-project).
3. `~/.config/grain/providers.toml` (user-wide fallback).

Missing files are not an error — the loader simply returns no profiles.

---

## TOML schema

```toml
[[profile]]
name = "openai-work"
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
model = "openai/gpt-4o"
auth = { kind = "api_key", env = "OPENAI_API_KEY_WORK" }

[[profile]]
name = "kimi-trial"
kind = "openai-compat"
base_url = "https://api.moonshot.cn/v1"
model = "kimi/moonshot-v1-128k"
auth = { kind = "api_key", env = "MOONSHOT_API_KEY" }

[[profile]]
name = "anthropic-default"
kind = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth = { kind = "api_key", env = "ANTHROPIC_API_KEY" }

[[profile]]
name = "claude-pro"
kind = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth = { kind = "anthropic_oauth" }
```

| Field | Required | Notes |
|-------|----------|-------|
| `name` | yes | User-facing label. Doubles as the genai provider id for routing — must be unique across profiles. |
| `kind` | yes | One of `anthropic`, `openai`, `gemini`, `openai-compat`. |
| `base_url` | required for `openai-compat` | Ignored for native kinds. |
| `model` | yes | `grain-llm-models` registry id (e.g. `openai/gpt-4o`). |
| `auth.kind` | yes | `api_key` (works today) or `anthropic_oauth` (Phase 2 — parsed and stubbed). |
| `auth.env` | required when `auth.kind = api_key` | Env var name to read the key from at use time. |

A malformed entry is skipped with a `[warn]` line — the rest of the file still loads.

---

## How routing works

- **`openai-compat`** profiles register a `(name, base_url, env_var)` tuple as an `OpenAiCompatEndpoint`. Models you address as `<profile_name>/<model>` route through that endpoint with that env var. This is the way to do *multi-account per vendor*: each profile is its own genai namespace.
- **`anthropic` / `openai` / `gemini`** profiles override the auth env var for the native genai adapter. When multiple profiles share a native kind, the last one wins — use `openai-compat` if you need true multiple-account semantics.

Internally `grain-llm-genai` plumbs all of this through one call:

```rust
let stream = grain_llm_genai::GenaiStream::builder()
    .with_provider_profiles(&profiles)   // <-- here
    .with_registry(registry)
    .build();
```

See [llm-genai.md](./llm-genai.md) for the rest of the builder.

---

## CLI usage

### `grain-headless`

```bash
# Activate a profile for this run (model + auth env come from the profile):
grain-headless -C ./proj --provider openai-work --prompt "hi"

# Override the search path:
grain-headless --providers-file /etc/grain/providers.toml --provider kimi-trial --prompt "hi"
```

When `--provider` is set, the profile's `model` overrides `--model` and `Model.provider` is rewritten to the profile name (so `openai-compat` routing kicks in). Selecting an `anthropic_oauth` profile fails fast with a Phase-2 message.

### `grain-tui`

Same flags, plus an interactive picker:

```bash
grain-tui -C ./proj --provider openai-work
```

Type `/provider` in the TUI to open the picker overlay. Up/Down navigates, Enter applies (runtime — no restart). The picker shows the active profile (`✓`), the auth status (`[ready]`, `[no key]`, `[needs login]`), and color-codes it.

---

## Multiple-account recipe

Two OpenAI keys on the same machine:

```toml
[[profile]]
name = "openai-work"
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
model = "openai/gpt-4o"
auth = { kind = "api_key", env = "OPENAI_API_KEY_WORK" }

[[profile]]
name = "openai-personal"
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
model = "openai/gpt-4o-mini"
auth = { kind = "api_key", env = "OPENAI_API_KEY_PERSONAL" }
```

Set both env vars in your shell; switch with `/provider` (TUI) or `--provider <name>` (headless).

---

## Phase 2 — Anthropic OAuth subscription

Today, `auth = { kind = "anthropic_oauth" }` profiles:

- Are accepted by the loader.
- Show up in `/provider` with `[needs login]`.
- Fail fast with a clear error if you try to actually use them: `provider 'X' uses OAuth; login flow is not yet wired`.

The Phase 2 PR will add:

- PKCE flow with localhost redirect server.
- Token storage under `<data_dir>/grain/oauth/<profile>.json` with `0600` perms.
- Auto-refresh on 401.
- A `grain-tui login <profile>` / `grain-headless login <profile>` subcommand to drive the browser flow.

The data model + UI + worker switch path are already shaped for it — Phase 2 just plugs `ProviderAuth::AnthropicOauth` into a refresh-aware transport via genai's `ServiceTargetResolver`.
