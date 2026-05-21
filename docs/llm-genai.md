# `grain_llm_genai`

`grain_agent_core::LlmStream` implementation backed by the [`genai`](https://crates.io/crates/genai) 0.6 crate (currently pinned to `0.6.0-beta.20` — the upstream maintainer recommends this over 0.5 for robustness, broader provider coverage, and bug fixes). Bridges the transport-agnostic agent loop to genai's multi-provider chat API.

This is the crate you wire into `AgentOptions::stream_fn` to talk to a real LLM.

## Quick start

```rust
use std::sync::Arc;
use grain_agent_core::{Agent, AgentOptions};
use grain_llm_genai::GenaiStream;
use grain_llm_models::Registry;

let stream: Arc<GenaiStream> = Arc::new(GenaiStream::new());
let model = Registry::from_embedded_snapshot()
    .to_core_model("anthropic/claude-sonnet-4-5")
    .unwrap();

let agent = Agent::new(AgentOptions::new(model, stream));
agent.prompt_text("hello").await?;
```

`GenaiStream::new()` uses `genai::Client::default()` (env-var-based auth, prefix-based provider detection) and our [`baseline_chat_options`].

For anything beyond the defaults, use the builder.

## Builder

```rust
use grain_llm_genai::{GenaiStream, GenaiStreamBuilder, OpenAiCompatPreset};
use std::sync::Arc;
use grain_llm_models::Registry;

let stream = GenaiStream::builder()
    .with_openai_compat_preset(OpenAiCompatPreset::Common)   // kimi + siliconflow
    .with_env_override("openai", "MY_OPENAI_KEY")            // override env var name
    .with_registry(Arc::new(Registry::from_embedded_snapshot()))
    .build();
```

The builder configures a `genai::Client` with both an auth resolver (env-var-based key lookup) and a service-target resolver (OpenAI-compat endpoint rewriting). Defaults are sane: `EnvKeyResolver::default_mapping()` covers all genai-native providers; `OpenAiCompatPreset::None` (empty); `ProviderRouter::default()` renames `google` → `gemini`, `zhipu` → `bigmodel`, `moonshot` → `kimi`.

## Model id format

grain ids are `"<provider>/<model>"` (e.g. `"anthropic/claude-sonnet-4-5"`). genai dispatches on `"<namespace>::<model>"`. Translation runs automatically inside `stream()`:

| grain id | translated | genai adapter |
|---|---|---|
| `anthropic/claude-sonnet-4-5` | `anthropic::claude-sonnet-4-5` | Anthropic native |
| `openai/gpt-4o` | `openai::gpt-4o` | OpenAI native |
| `google/gemini-2.0-flash` | `gemini::gemini-2.0-flash` | Gemini native (router rename) |
| `zhipu/glm-4-plus` | `bigmodel::glm-4-plus` | BigModel native (router rename) |
| `kimi/moonshot-v1-128k` | `kimi::moonshot-v1-128k` | OpenAI adapter, Kimi endpoint (compat preset) |
| `gpt-4o` (no `/`) | `gpt-4o` | genai auto-detect |

Override the router via `with_provider_router(ProviderRouter::new().with_override(...))` if you need different mapping.

## OpenAI-compat routing

When a model id's namespace matches a registered `OpenAiCompatEndpoint`, the builder's `service_target_resolver` rewrites the request:

- `endpoint` → the preset's `base_url`
- `auth` → reads the preset's env var
- `adapter_kind` → `OpenAI` (we speak the OpenAI wire format)
- model name → stripped of namespace before sending

`OpenAiCompatPreset::Common` ships these out of the box:

| id | base URL | env var |
|---|---|---|
| `kimi` | `https://api.moonshot.cn/v1` | `MOONSHOT_API_KEY` |
| `siliconflow` | `https://api.siliconflow.cn/v1` | `SILICONFLOW_API_KEY` |

Need more? Append your own:

```rust
.with_openai_compat(OpenAiCompatEndpoint::new(
    "my-host", "https://api.example.com/v1", "MY_HOST_API_KEY",
))
```

genai 0.5 natively supports Anthropic, OpenAI, Gemini, DeepSeek, Groq, Mimo, Nebius, xAI, Zai, BigModel (Zhipu), Cohere, Together, Fireworks, and Ollama — they are **deliberately not** in the OpenAI-compat preset; touching them would override their native adapter and break per-provider quirks.

## Env-based API keys

`EnvKeyResolver::default_mapping()` covers 19 providers (all genai-native + the OpenAI-compat presets). To customize:

```rust
let resolver = grain_llm_genai::EnvKeyResolver::default_mapping()
    .with_override("openai", "MY_OPENAI_KEY")
    .with_override("acme",   "ACME_LLM_KEY");

let stream = GenaiStream::builder().with_env_resolver(resolver).build();
```

The builder's `auth_resolver` consults this map first; on miss, falls through to genai's own default lookup.

## Streaming events

`GenaiStream::stream(...)` returns a `Pin<Box<dyn Stream<Item = AssistantMessageEvent>>>` that emits a well-formed event sequence:

1. Exactly one `Start { partial }`
2. Per content block: `TextStart` / `TextDelta` / `TextEnd`, `ThinkingStart` / `ThinkingDelta` / `ThinkingEnd`, or `ToolcallStart` / `ToolcallEnd`
3. Exactly one terminal `Done { result }` or `Error { error, result }`

Internally `mapping::inbound::InboundState` is a small state machine that:
- Aggregates consecutive same-type chunks into one block
- Closes the current block on type transition
- Silently merges Anthropic-style `ThoughtSignatureChunk`s into the open `Thinking` block's `signature` field
- Synthesizes `Done` from accumulated content (stop_reason inferred: any tool call → `ToolUse`, otherwise `Stop`)

## Thinking / reasoning replay

Both directions are wired:

- **Inbound**: `ReasoningChunk` → `AssistantContent::Thinking`; `ThoughtSignatureChunk` populates `signature`. PR 3b's state machine handles the bookkeeping.
- **Outbound**: when an `AssistantMessage` carries a `Thinking` block with a signature, the signature is attached to the **first** outgoing `ToolCall::thought_signatures`. This is what Anthropic uses to validate multi-turn signed thinking — without it, multi-turn signed flows break.

**Reasoning text is intentionally not echoed back to the provider.** genai's outbound API has no `reasoning_content` slot, and providers regenerate their own reasoning each turn (OpenAI o-series, DeepSeek-R1). The text stays in the grain transcript for app-side use (UI display, audit, …) but doesn't go on the wire.

## genai 0.6 streaming behavior (important)

genai 0.6 changed how tool-call streaming events arrive, and our state machine handles both quirks transparently:

1. **Cumulative argument chunks.** A single tool call can emit *multiple* `ToolCallChunk` events that share the same `call_id`; each subsequent chunk carries the latest **accumulated** JSON arguments (not a delta). The inbound state machine tracks `call_id → block index` and overwrites the existing block's `arguments` on every refresh, instead of pushing a duplicate `ToolCall` content block. Without this, a tool call would appear N times in the assistant message and get executed N times — and the next-turn request would contain N tool-result messages with identical `tool_call_id`, which strict providers (DeepSeek, OpenAI) reject with a 400.
2. **String-encoded arguments.** `GenaiToolCall.fn_arguments` is sometimes delivered as `Value::String("{ ... }")` — the JSON object encoded as a string — rather than as a real `Value::Object`. The state machine runs every incoming argument value through `normalize_tool_args(...)`: when the value is a string that parses as valid JSON, we substitute the parsed object before the args reach `tool.execute(...)`. Without this, tools would fail with `invalid type: string "...", expected struct FooArgs`.

Both of these are exercised end-to-end by the live tests; if you ever pin to a newer genai version and see "tool ran N times" or "expected struct ... found string" symptoms, the issue is one of these helpers diverging from upstream behavior.

## Cancellation

The implementation races genai's stream against the `CancellationToken` you pass in. When cancellation fires:

1. The inner genai stream is dropped (it won't be polled again).
2. The state machine produces a terminal `Error` event with `stop_reason = Aborted` and `error_message = "aborted"`, preserving any partial content already received.

## Live tests

`tests/live.rs` contains five `#[ignore]`-gated tests that hit real provider endpoints (OpenAI, Anthropic, OpenAI-compat Kimi, plus a cancellation race). Run them manually when you change anything in the outbound mapper, inbound state machine, or builder.

The recommended workflow is to put your keys in `.env.test` at the workspace root (see [testing.md](./testing.md) for the format) and then:

```bash
cargo test -p grain-llm-genai --test live -- --ignored
```

Each test individually skips with a printed note when its env var isn't set, so passing `--ignored` is always safe even with only one provider configured.

## Caveats

- `genai`'s `ServiceTarget` resolver is sync; we can't run async work (DNS, OAuth refresh) inside auth/target resolution. If you need async key lookup, do it before calling the agent and pass the resolved key via env or a custom resolver.
- The provider router only handles the namespace translation; renames also live in `grain-llm-models::Registry` (e.g. `provider: "google"`, `api: "gemini"`). Keep them in sync if you customize either side.
- The OpenAI-compat preset uses `"kimi"` and `"siliconflow"` as ids; models.dev's catalog uses `"moonshotai"` for Kimi's native endpoint. If you load models by their models.dev id directly, register an additional `OpenAiCompatEndpoint { id: "moonshotai", ... }`.
