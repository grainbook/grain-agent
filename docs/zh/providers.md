# Provider profiles（厂商配置）

一份 TOML 让一个 binary 支持多家 LLM 厂商、同一厂商多账号、或多种订阅方式 —— 不用再折腾环境变量。

实现位于 [`grain-llm-genai`](./llm-genai.md)（一等公民），[`grain-headless`](./headless-cli.md) 和 `grain-tui`（`/provider` 弹层）都消费它。

English: [../providers.md](../providers.md).

---

## 为什么要 profile

bare 的 builder 已经能用环境变量调主流厂商。Profile 又给了三件事：

1. **同厂商多账号**：`openai-work` / `openai-personal` 两个 `openai-compat` profile，各自指定 env var，运行时切换。
2. **自定义 host**：任何 OpenAI 兼容端点（DeepSeek、MiniMax、OpenRouter、Together、自建 vLLM）一行 TOML 就接上。
3. **订阅鉴权（Phase 2）**：`anthropic_oauth` 这种通过 Claude Pro/Max 登录的方式 —— 今天 *parse* 但不可用，选中会提示 "login flow not yet wired"。浏览器回调 + token 刷新留给下个补丁。

---

## 文件路径查找顺序

按以下顺序，第一个存在的文件生效：

1. `--providers-file <path>` (CLI 覆盖)
2. `<workspace>/.grain/providers.toml` (项目级)
3. `~/.config/grain/providers.toml` (用户级)

文件不存在不报错 —— 直接返回空列表。

---

## TOML 格式

```toml
[[profile]]
name = "openai-work"
kind = "openai-compat"
base_url = "https://api.openai.com/v1"
model = "openai/gpt-4o"
auth = { kind = "api_key", env = "OPENAI_API_KEY_WORK", value = "sk-..." }

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
auth = { kind = "api_key", env = "ANTHROPIC_API_KEY", value = "sk-ant-..." }

[[profile]]
name = "claude-pro"
kind = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth = { kind = "anthropic_oauth" }
```

| 字段 | 必填 | 说明 |
|------|------|------|
| `name` | 是 | 显示名，同时作为 genai 路由用的 provider id，要全局唯一 |
| `kind` | 是 | `anthropic` / `openai` / `gemini` / `openai-compat` 之一 |
| `base_url` | `openai-compat` 必填 | 其他 kind 忽略 |
| `model` | 是 | `grain-llm-models` 注册表里的 id (e.g. `openai/gpt-4o`) |
| `auth.kind` | 是 | `api_key`（可用） 或 `anthropic_oauth`（Phase 2 stub） |
| `auth.env` | `api_key` 时必填 | 调用时读这个 env var |
| `auth.value` | 否 | API key 明文。写了启动时自动设环境变量，不需要手动 `export`。不写则从 shell 环境读取 |

格式错的条目 `[warn]` 一行跳过，文件里其他条目继续加载。

---

## 路由原理

- **`openai-compat`** profile 在内部注册成 `(name, base_url, env_var)` 的 `OpenAiCompatEndpoint`。地址写 `<profile_name>/<model>` 即走该端点 + env var。**同厂商多账号必须用这种**，因为每个 profile 是一个独立的 genai namespace。
- **`anthropic` / `openai` / `gemini`** profile 用 env var 覆盖原生 adapter 的默认 key。同 kind 多个 profile 时后写入的赢；需要真正多账号 → 用 `openai-compat`。

所有逻辑在一个 builder 调用里完成：

```rust
let stream = grain_llm_genai::GenaiStream::builder()
    .with_provider_profiles(&profiles)
    .with_registry(registry)
    .build();
```

builder 其余部分见 [llm-genai.md](./llm-genai.md)。

---

## CLI 用法

### `grain-headless`

```bash
grain-headless -C ./proj --provider openai-work --prompt "hi"
grain-headless --providers-file /etc/grain/providers.toml --provider kimi-trial --prompt "hi"
```

`--provider` 一旦设置，profile 的 `model` 会覆盖 `--model`，且 `Model.provider` 会被改写成 profile 名（让 `openai-compat` 路由起作用）。选中 `anthropic_oauth` 直接 fail-fast 并打印 Phase 2 提示。

### `grain-tui`

支持同样两个 flag，再加交互式 picker：

```bash
grain-tui -C ./proj --provider openai-work
```

TUI 内按 `/provider` 打开选择器：↑↓ 导航，Enter 应用（**运行时切换，不重启**）。每行显示当前激活标记 `✓` 和鉴权状态 `[ready]` / `[no key]` / `[needs login]`，配色提示。

---

## 同厂商多账号示例

两个 OpenAI key：

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

shell 里两个 env var 都 export，然后用 `/provider` 或 `--provider` 切换。

---

## Phase 2 —— Anthropic OAuth 订阅

当前 `auth = { kind = "anthropic_oauth" }` 的状态：

- 能 parse、能列出来
- `/provider` 显示 `[needs login]`
- 一旦实际调用，clean error: `provider 'X' uses OAuth; login flow is not yet wired`

Phase 2 PR 会补上：

- PKCE + localhost 回调
- Token 存到 `<data_dir>/grain/oauth/<profile>.json`（权限 `0600`）
- 401 自动刷新
- `grain-tui login <profile>` / `grain-headless login <profile>` 子命令驱动浏览器

数据模型 + UI + worker 切换路径都已经为它准备好；Phase 2 仅插入一个 refresh-aware transport 即可。
