# Config file

grain-agent 启动时会自动读取 TOML 配置文件。加载顺序：项目级配置覆盖用户级配置，命令行参数覆盖所有配置。**不需要手动 `export` 任何环境变量**——API key 可以直接写在配置文件里。

## 读完这篇文章你会...

1. 知道配置文件放在哪里
2. 把 API key 写进文件，彻底告别 `export`
3. 为不同项目配置不同的模型 / 权限 / 插件
4. 看懂所有可配字段

---

## 第 1 步 — 配置文件放哪里？

两个位置（都是可选的，不存在也没关系）：

| 优先级 | 位置 | 作用范围 |
|--------|------|----------|
| **高** | `<项目根目录>/.grain/config.toml` | 只对当前项目生效 |
| **低** | `~/.config/grain/config.toml` | 对所有项目生效（全局默认） |

**规则**：项目配置覆盖全局配置，命令行参数覆盖一切。

> 💡 第一次用？在项目根目录创建 `.grain/config.toml` 就行。`mkdir -p .grain && touch .grain/config.toml`

---

## 第 2 步 — 最小可用配置（3 分钟跑起来）

把下面这段粘贴到 `.grain/config.toml`，把 API key 替换成你自己的：

```toml
[[provider]]
name  = "anthropic"
kind  = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth  = { kind = "api_key", env = "ANTHROPIC_API_KEY", value = "sk-ant-你的key在这里" }
```

然后直接跑（不需要 `export` 任何东西）：

```bash
grain-headless -C . --prompt "看看 main.rs 做了什么？"
```

**发生了什么？** 启动时 engine 读到 `auth.value`，自动帮你执行了 `export ANTHROPIC_API_KEY="sk-ant-..."`，后续的 LLM 调用就能拿到 key。配置文件没读到的 provider，仍然走环境变量。

> 🔐 关于安全：把 API key 写进文件意味着任何能读这个文件的人都能用你的 key。如果你在意这一点，保持用传统的 `export` 方式（`auth` 里不写 `value`，只写 `env`，key 由 shell 环境提供）。两种方式完全兼容。

### 用其他模型？

```toml
# OpenAI
[[provider]]
name  = "openai"
kind  = "openai"
model = "openai/gpt-4o"
auth  = { kind = "api_key", env = "OPENAI_API_KEY", value = "sk-..." }

# DeepSeek
[[provider]]
name  = "deepseek"
kind  = "openai-compat"
base_url = "https://api.deepseek.com/v1"
model = "deepseek/deepseek-chat"
auth  = { kind = "api_key", env = "DEEPSEEK_API_KEY", value = "sk-..." }
```

---

## 第 3 步 — 完整字段参考

所有字段都是可选的。`[[provider]]` 可以写多个。

### 模型与 LLM

```toml
# 默认模型 id（来自 models.dev 注册表）
model = "anthropic/claude-sonnet-4-5"

# context-guard 截断时为系统提示 + 完成阶段预留的 token 数
headroom_tokens = 4096

# 是否显示 LLM 的思考过程（暗色字体）
show_thinking = false

# OpenAI 兼容端点预设："none"（不用）或 "common"（Kimi + SiliconFlow）
openai_compat = "common"
```

### 能力开关（全部默认关闭）

```toml
allow_write           = false   # 允许 agent 写 / 编辑文件
allow_bash            = false   # 允许 agent 执行 shell 命令
allow_web             = false   # 允许 agent 访问网页
allow_semantic_search = false   # 允许语义搜索（需编译时 --features rig）
```

### 路径覆盖

```toml
# 技能目录（默认 <workspace>/.claude/skills）
skills_dir = ".claude/skills"

# 会话存档目录（默认 <workspace>/.grain/sessions）
session_dir = ".grain/sessions"
```

### 网络与代理

```toml
# 是否绕过代理。不设时（默认）：如果注册了本地 OpenAI 兼容端点
# 则自动绕过，否则跟随 HTTPS_PROXY/ALL_PROXY 环境变量。
# 设为 true 强制绕过，设为 false 强制使用代理。
bypass_proxy = false
```

### TUI 折叠行为

```toml
# 默认折叠工具调用块（展开为一行摘要；用户可逐条展开）
fold_tool_calls = true

# 默认折叠思考块（同上）
fold_thinking = true
```

---

## 第 4 步 — 声明插件（`[[plugin]]`）

想把插件放在 config.toml 里统一管理？加 `[[plugin]]` 块：

```toml
[[plugin]]
name = "lazy-gagent"              # 插件名 = .grain/plugins/ 下的目录名
src  = "../lazy-gagent"           # 本地路径 → 直接读取源码

[[plugin]]
name = "rust-helper"
src  = "https://github.com/me/rust-helper.git"   # Git URL → 启动时自动 clone
rev  = "v1.0.0"                                  # 可选，指定 tag/branch/commit
```

`src` 的识别逻辑：
- 以 `http://` / `https://` / `git@` / `ssh://` / `git://` 开头，或以 `.git` 结尾 → **Git clone**
- 其他 → **本地路径**（相对于 `config.toml` 所在目录）

> 📝 运行时安装的插件（`/install` 命令）写入 `plugin-lock.toml`，不会修改你手写的 `config.toml`。启动时两份文件合并，同名取 config.toml。详见 [plugins.md](./plugins.md)。

---

## 第 5 步 — 声明 provider（`[[provider]]`）

完整 provider 块的所有字段：

```toml
[[provider]]
name     = "my-provider"          # 唯一名称（用于 --provider 和 /provider 切换）
kind     = "openai-compat"        # anthropic | openai | gemini | openai-compat
base_url = "https://api.openai.com/v1"  # openai-compat 必填，其他忽略
model    = "openai/gpt-4o"        # models.dev 注册表里的 id
auth     = { kind = "api_key", env = "OPENAI_API_KEY", value = "sk-..." }
```

### `auth` 字段详解

| 字段 | 必填 | 说明 |
|------|------|------|
| `auth.kind` | ✅ | `"api_key"`（可用）或 `"anthropic_oauth"`（Phase 2） |
| `auth.env` | ✅（api_key） | 环境变量名——系统用它来查找 key。`value` 写了的话启动时会自动设这个变量 |
| `auth.value` | ❌ | API key 明文。写这里就不用手动 `export`。不写则从 shell 环境变量读取 |

```toml
# 方式 1：key 写进文件（方便）
auth = { kind = "api_key", env = "ANTHROPIC_API_KEY", value = "sk-ant-..." }

# 方式 2：key 从环境变量读（传统）
auth = { kind = "api_key", env = "ANTHROPIC_API_KEY" }

# 方式 3：OAuth（Phase 2）
auth = { kind = "anthropic_oauth" }
```

> 更多 provider 用法（多账号、自建端点、OAuth 订阅），见 [providers.md](./providers.md)。

---

## 第 6 步 — 完整示例

### `~/.config/grain/config.toml`（全局默认）

```toml
# 全局偏好
show_thinking = true
openai_compat = "common"
fold_tool_calls = true

# 平时用的 provider（不写 value，key 从 shell 环境拿）
[[provider]]
name  = "anthropic"
kind  = "anthropic"
model = "anthropic/claude-sonnet-4-5"
auth  = { kind = "api_key", env = "ANTHROPIC_API_KEY" }
```

### `<项目>/.grain/config.toml`（项目配置）

```toml
# 这个项目比较随意，什么权限都开
allow_write = true
allow_bash = true
allow_web = true
model = "anthropic/claude-haiku-4-5"  # 便宜模型就够了

# 项目专属的本地插件
[[plugin]]
name = "my-dev-tools"
src  = "../my-dev-tools"
```

### 混合使用的结果

当你在 `<项目>` 下运行 `grain-tui` 时：
- `show_thinking = true`（来自全局配置，项目没覆盖）
- `allow_write = true`（来自项目配置，覆盖全局默认的 `false`）
- `model = "anthropic/claude-haiku-4-5"`（项目覆盖全局）
- `my-dev-tools` 插件被加载（项目专属）

---

## CLI 优先级规则

| 来源 | 优先级 | 示例 |
|------|--------|------|
| 命令行参数 | **最高** | `--allow-bash` 写了就以它为准 |
| 项目 config.toml | 中 | `<项目>/.grain/config.toml` |
| 全局 config.toml | 低 | `~/.config/grain/config.toml` |
| 内置默认值 | **最低** | `allow_bash` 默认 `false` |

**双向覆盖**：config 里的 `allow_bash = false` 会关掉命令行默认值，但命令行显式传了 `--allow-bash` 又会重新打开。不会出现「config 只能开不能关」的意外。

---

## 快速排错

| 现象 | 检查 |
|------|------|
| 修改 config 不生效 | 重启 grain（`config.toml` 只在启动时读一次） |
| `unknown field` 错误 | TOML 字段名写错了，对照上面的参考 |
| provider 找不到 | `name` 是否和 `--provider` 参数一致？ |
| API key 报错 | `auth.env` 的环境变量名对不对？`value` 有没有写错？跑 `grain-headless --doctor` 检查 |
