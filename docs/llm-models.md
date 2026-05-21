# `grain_llm_models`

Standardized model registry. Plays the role `models.dev` integration plays in `@earendil-works/pi-ai`: a single source of truth for context window, capability flags, pricing, and provider-specific quirks (the thinking / reasoning field name).

Sits in its own crate so it can be reused without dragging in the `genai` SDK.

## Types

```rust
pub struct ModelDescriptor {
    pub id: String,              // "<provider>/<model>" canonical key
    pub name: String,
    pub provider: ProviderId,
    pub api: ApiKind,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub cost: grain_agent_core::Cost,
    pub capabilities: Capabilities,
    pub thinking: ThinkingProfile,
    pub extra: serde_json::Value,
}

pub enum ProviderId {
    Anthropic, OpenAi, Google, DeepSeek, Mistral, Meta, Cohere, Xai,
    OpenAiCompatible { id: String },
    Other { id: String },
}

pub enum ApiKind { OpenAi, Anthropic, Gemini, Mistral, Cohere }

pub struct Capabilities {
    pub streaming: bool,
    pub tool_use: bool,
    pub vision: bool,
    pub json_mode: bool,
    pub structured_output: bool,
}

pub struct ThinkingProfile {
    pub supported: bool,
    pub default_level: ThinkingLevel,
    pub supported_levels: Vec<ThinkingLevel>,
    pub reasoning_field_name: Option<String>,  // "thinking" / "reasoning_content" / ...
}
```

`ProviderId` is open: any provider id outside the well-known set lands under `OpenAiCompatible { id }` (when the upstream npm package signals OpenAI compat) or `Other { id }`. `api` stays `OpenAi` for compat providers — the wire protocol is what counts, the brand id is preserved for routing.

`ThinkingProfile.reasoning_field_name` pairs with `grain_agent_core::ThinkingContent::provider_metadata` for round-tripping reasoning blocks across turns.

## Registry

Read-only, `Arc`-wrapped `HashMap` lookup:

```rust
use grain_llm_models::Registry;

let registry = Registry::from_embedded_snapshot();   // 4803 models from models.dev

let descriptor = registry.lookup("anthropic/claude-sonnet-4-5").unwrap();
assert_eq!(descriptor.context_window, 200_000);

// Project to the core type used by Agent / AgentOptions.
let core_model = registry.to_core_model("anthropic/claude-sonnet-4-5").unwrap();

// Merge two registries — overlay wins per id.
let merged = base.merged_with(&overlay);
```

`Registry::from_embedded_snapshot()` panics only if the vendored JSON is malformed (a build-time bug). Normal callers should treat it as infallible.

## Embedded snapshot

`data/models-dev.json` is checked into the repo. `lib.rs` loads it via `include_str!`. **Build never touches the network.**

```
grain-llm-models/
  data/
    models-dev.json    # 4803 models, ~3.4 MB, source of truth
  src/
    snapshot.rs        # versioned wrapper, include_str! loader
```

Schema:

```json
{
  "version": 1,
  "models": [ /* ModelDescriptor */ ]
}
```

`CURRENT_SNAPSHOT_VERSION = 1`. Bump alongside any breaking JSON change so older binaries refuse to silently load incompatible data.

## Runtime fetch (`fetch` feature)

When you need a fresher snapshot than what's vendored:

```toml
[dependencies]
grain-llm-models = { path = "...", features = ["fetch"] }
```

```rust
use grain_llm_models::{fetch_models_dev, Registry};

let live = fetch_models_dev().await?;            // hits https://models.dev/api.json
let merged = Registry::from_embedded_snapshot().merged_with(&live);
```

Set `MODELS_DEV_URL=https://your-mirror/api.json` to point at a private mirror.

### Refreshing the vendored snapshot

```bash
cargo run -p grain-llm-models --features fetch --bin refresh-models
```

The bin writes a deterministic id-sorted snapshot back to `data/models-dev.json` so git diffs stay reviewable. Commit the result as a `chore(llm-models): refresh ...` commit.

## How the transform classifies providers

`fetch.rs` consumes models.dev's raw `Object<provider_id, Provider>` shape. The classifier resolves each entry as:

- Known provider key (`anthropic`, `openai`, `google`, `deepseek`, …) → matching `ProviderId` variant.
- `npm` contains `openai-compatible` → `ProviderId::OpenAiCompatible { id: provider_key }`.
- Everything else → `ProviderId::Other { id: provider_key }`.

`ApiKind` is read off the `npm` package name (`@ai-sdk/anthropic` → Anthropic, etc.), defaulting to OpenAI when no signal.

`ThinkingProfile.reasoning_field_name` is derived from `ApiKind` only when the model advertises `reasoning: true`:

- Anthropic → `"thinking"` (signed blocks)
- OpenAI → `"reasoning_content"` (o-series convention)
- Gemini → `"reasoning"`
- Otherwise → `None`

## Caveats

- Cost data in the snapshot is what models.dev publishes — re-run `refresh-models` to keep it current.
- 3.4 MB JSON is large to vendor but compresses well in git; loading at startup is a single `serde_json` pass.
- The 4803-entry registry includes many aggregator-prefixed ids (`302ai/...`, `aihubmix/...`, etc.). Filter on `provider` if you only want canonical first-party providers.
