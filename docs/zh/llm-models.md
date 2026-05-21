# `grain_llm_models`

标准化的模型注册表。在 `@earendil-works/pi-ai` 里对应 `models.dev` 集成：context window、capability 标记、定价、provider 特有的字段（如 thinking / reasoning 字段名）的单一数据源。

独立成 crate，方便在不依赖 `genai` SDK 的情况下复用。

> English version: [../llm-models.md](../llm-models.md).

## 类型

```rust
pub struct ModelDescriptor {
    pub id: String,              // "<provider>/<model>" 规范键
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

`ProviderId` 是开放枚举：上游 npm 包含 `openai-compatible` 标记的 provider 落到 `OpenAiCompatible { id }`，其它未知 provider 落到 `Other { id }`。`api` 字段仍是 `OpenAi`——wire 协议才是要紧的，品牌 id 单独保留用于路由。

`ThinkingProfile.reasoning_field_name` 与 `grain_agent_core::ThinkingContent::provider_metadata` 配对，用于跨轮回放 reasoning 内容。

## Registry

只读、`Arc<HashMap>` 包装的查找接口：

```rust
use grain_llm_models::Registry;

let registry = Registry::from_embedded_snapshot();   // 来自 models.dev 的 4803 个模型

let descriptor = registry.lookup("anthropic/claude-sonnet-4-5").unwrap();
assert_eq!(descriptor.context_window, 200_000);

// 投影到 Agent / AgentOptions 用的 core 类型。
let core_model = registry.to_core_model("anthropic/claude-sonnet-4-5").unwrap();

// 合并两个 registry —— overlay 按 id 覆写。
let merged = base.merged_with(&overlay);
```

`Registry::from_embedded_snapshot()` 仅在 vendored JSON 损坏时 panic（属于构建期 bug）。正常调用方按"不会失败"处理。

## 内嵌 snapshot

`data/models-dev.json` 提交在仓库里，`lib.rs` 通过 `include_str!` 加载。**构建期永不联网。**

```
grain-llm-models/
  data/
    models-dev.json    # 4803 模型，~3.4 MB，事实来源
  src/
    snapshot.rs        # 版本化包装、include_str! 加载
```

Schema：

```json
{
  "version": 1,
  "models": [ /* ModelDescriptor */ ]
}
```

`CURRENT_SNAPSHOT_VERSION = 1`。任何破坏性 JSON 修改都要 bump，老二进制会拒绝加载不兼容数据。

## 运行时 fetch (`fetch` feature)

需要比 vendored 更新的数据时：

```toml
[dependencies]
grain-llm-models = { path = "...", features = ["fetch"] }
```

```rust
use grain_llm_models::{fetch_models_dev, Registry};

let live = fetch_models_dev().await?;            // 访问 https://models.dev/api.json
let merged = Registry::from_embedded_snapshot().merged_with(&live);
```

设置 `MODELS_DEV_URL=https://your-mirror/api.json` 可指向私有镜像。

### 刷新 vendored snapshot

```bash
cargo run -p grain-llm-models --features fetch --bin refresh-models
```

binary 会按 id 排序写回 `data/models-dev.json`，让 git diff 仍可 review。把结果作为 `chore(llm-models): refresh ...` commit 进库。

## 分类逻辑

`fetch.rs` 消费 models.dev 的 `Object<provider_id, Provider>` 原始结构。对每个 entry：

- provider key 在已知名单（`anthropic`、`openai`、`google`、`deepseek`、…）→ 对应 `ProviderId` 变体。
- `npm` 包含 `openai-compatible` → `ProviderId::OpenAiCompatible { id: provider_key }`。
- 其它 → `ProviderId::Other { id: provider_key }`。

`ApiKind` 从 `npm` 包名推断（`@ai-sdk/anthropic` → Anthropic 等），无信号时默认 OpenAI。

`ThinkingProfile.reasoning_field_name` 仅在模型声明 `reasoning: true` 时按 `ApiKind` 派生：

- Anthropic → `"thinking"`（签名块）
- OpenAI → `"reasoning_content"`（o-series 约定）
- Gemini → `"reasoning"`
- 其它 → `None`

## 注意事项

- snapshot 里的价格是 models.dev 当前发布值——定期跑 `refresh-models` 保持时效。
- 3.4 MB JSON 在 git 里比较大但压缩效果不错；启动时只是一次 `serde_json` 反序列化。
- 4803 条 registry 里包含很多聚合器前缀（`302ai/...`、`aihubmix/...` 等）。如果只想要规范的一线 provider，按 `provider` 字段过滤。
