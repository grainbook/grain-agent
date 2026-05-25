# 插件系统（`lazy.gagent`）

Neovim / lazy.nvim 风格的插件层。`<workspace>/.grain/plugins/<name>/` 下扔一个目录，就能携带 skill、theme、系统提示片段、JS 脚本工具 —— 启动时自动发现，无需重新编译。

English: [../plugins.md](../plugins.md).

---

## 心智模型

| 角色 | Crate | 类比 |
|---|---|---|
| **引擎** —— manifest 格式、发现、agent 启动期集成 | `grain-ai-agent-headless::plugins` | Neovim 内核 |
| **UI** —— `/plugins` 遮罩面板、主题选择器 | `grain-ai-agent-tui` | 用户的终端 |
| **管理器** —— 安装 / 更新 / 删除插件（Phase C）| `lazy-gagent` | lazy.nvim |

引擎对管理器一无所知：管理器最终也只是 `<workspace>/.grain/plugins/lazy-gagent/` 下的另一个目录，通过同一套机制（skill + JS 工具）提供管理命令。今天 `lazy-gagent` crate 是个 placeholder，只 re-export headless 类型。

---

## 目录布局

**插件 = `<workspace>/.grain/plugins/<name>/` 下含 `plugin.toml` 的任意目录**。其它子目录按约定被拾起：

```text
<workspace>/.grain/plugins/<name>/
  plugin.toml              # 必需 —— 标识此插件
  skills/<skill>/SKILL.md  # 可选 —— 合并进 find_skills
  themes/<theme>.toml      # 可选 —— TUI 主题选择器拾取
  prompts/*.md             # 可选 —— append 到系统提示
  scripts/*.js             # 可选 —— Boa 脚本（需 `scripts-boa` feature）
```

发现规则：

- 子目录没 `plugin.toml` 的静默跳过
- 隐藏目录（`.foo`）和缓存目录（`_cache`）跳过
- `plugin.toml` 损坏在 stderr 打 `[warn]`，**其它插件继续加载**（一个坏插件永远不会拖垮整体）
- 按 manifest name 字母序排序，启动日志 + `/plugins` 遮罩面板顺序可预测

默认目录 `<workspace>/.grain/plugins/` 可通过 `grain-tui --plugins-dir <PATH>` 覆盖；headless 库直接传路径给 `discover_plugins(...)`。

---

## 声明式安装：`plugin.toml`

手动 `cd .grain/plugins && git clone …` 装每个插件能用，但换机器就废了。引擎启动时会读可选的 `<workspace>/.grain/plugin.toml`，**把列里没安装的插件自动拉过来**：

```toml
# <workspace>/.grain/plugin.toml

[[plugin]]
name = "rust-helper"
src  = "https://github.com/me/rust-helper.git"
rev  = "v1.0.0"               # 可选，默认主分支

[[plugin]]
name = "lazy-gagent"
src  = "git@github.com:me/lazy-gagent.git"

[[plugin]]
name = "local-dev"
src  = "/Users/me/dev/my-plugin"  # 文件系统路径 → symlink 过去
```

`src` 语义：

| 识别为 | 何时 | 动作 |
|---|---|---|
| `Git` | 以 `http://`、`https://`、`git@`、`ssh://`、`git://` 开头，或以 `.git` 结尾 | `git clone <src> <plugins_dir>/<name>`（若 `rev` 设了再 `git checkout`）|
| `Local` | 其它（绝对路径、相对路径、`~/...`）| Symlink `<plugins_dir>/<name>` → 解析后的绝对路径（源码改动立即生效 —— 适合开发）|

在 `[[plugin]]` 里加 `kind = "git"` / `kind = "local"` 可强制覆盖自动判断。

**Bootstrap 妙处**：解决 `lazy-gagent` 管理器的鸡生蛋问题 —— 跟其它插件一样写进 spec 就行。引擎在 plugin discover 之前先拉好，到 agent 启动时「管理器」跟「管理器装的插件」没区别。

**HTTPS 鉴权注意** —— `git clone` 在 `GIT_TERMINAL_PROMPT=0` + 关闭 stdin 下跑，所以**永远不会**提示输密码也**不会**卡住启动。私有仓库的两条路：

- 预配置 credential helper（macOS: `git config --global credential.helper osxkeychain`、Windows: `manager-core`、其它: `store`），**或者**
- 用 SSH URL 形式 (`git@github.com:owner/repo.git`)，靠你机器上的 SSH agent 鉴权

凭证缺失会作为干净的 `failed` 行落在启动日志里；spec 里其它插件继续装。

跳过规则：`<plugins_dir>/<name>/` 已存在时**永不动**（不重新 clone、不覆盖）。要重装得自己先 `rm -rf`。

失败不致命：单个 src 坏掉打 `[warn]` 行，其它插件照装。

库使用方式：

```rust
let spec_path = h::default_spec_path(workspace_root);
let spec = h::load_plugin_spec(&spec_path).unwrap_or_default();
let report = h::sync_plugins(&spec, &plugins_dir);
report.log_to_stderr();
```

---

## `plugin.toml`

最小 manifest：

```toml
name = "rust-helper"
```

完整字段（除 `name` 外都可选）：

```toml
name = "rust-helper"
version = "0.1.0"
description = "Rust 专属 skill + 系统提示规则"
author = "you"
```

未填的字段降级为空串 —— Phase A 不强加更多 schema，将来加 `dependencies = [...]` 也不会破坏旧 manifest。

---

## 每个子目录的作用

### `skills/<name>/SKILL.md`

通过 `find_skills_with_plugins(primary_dir, plugins)` 合并进 agent 的 skill 目录。skill 文件格式跟 workspace 的 `<workspace>/.claude/skills/` 一致 —— 详见 [harness-system-prompt.md](./harness-system-prompt.md) 和引擎里的 `find_skills`。

TUI 的 slash palette 和 `/skills` 遮罩面板会跟 workspace 自身的 skill 一起列出来。Phase B 还没做命名空间，两个插件同名 `lint` 会后注册赢；后续工作：显示前缀 `<plugin>/`。

### `themes/<name>.toml`

`<plugin>/themes/` 下的每个 `.toml` 被 `grain-ai-agent-tui` 的主题加载器解析（跟 `<workspace>/.grain/themes/` 同一代码路径）。`/theme` 选择器把插件主题跟内置 + 用户主题混在一起；激活通过 `tui-state.toml` 持久化，下次启动恢复同一主题。

### `prompts/*.md`

启动时按字母序读每个 `.md` 文件，append 到基础系统提示，每段前面带 banner：

```text
<基础提示>

## Plugin: <插件名>

<prompts/01-rules.md 内容>

## Plugin: <插件名>

<prompts/02-style.md 内容>
```

组装发生在 **harness pin 系统提示之前**，所以 LLM 看到的 plugin 规则是 canonical prefix 的一部分，上游 prefix cache（Anthropic / OpenAI / DeepSeek …）跨 turn 仍能命中。

适合用来给模型注入领域规则 ——「提交前先跑 clippy」、「用 cargo fmt --check 格式化」、「尊重现有 axum router 布局」等。

### `scripts/*.js`

TUI 用 `--features scripts-boa` 构建时，每个 `<plugin>/scripts/*.js` 都被加载到**同一个** Boa worker（跟 workspace 自己的 `<workspace>/.grain/scripts/` 共享）。通过 `grain.register_tool({...})` 注册的工具都暴露给同一个 agent。

实现靠新增的 `BoaExtension::from_scripts_dirs(&[...])` 构造器；JS API 见 [scripting.md](./scripting.md)。加载顺序：workspace 主目录优先，然后按插件发现顺序；同名工具后注册赢。

---

## CLI

`grain-tui` 跟插件相关的旗标：

```text
--plugins-dir <DIR>   # 默认: <workspace>/.grain/plugins
```

`grain-headless` 库使用者可以直接调用 plugin 发现：

```rust
use grain_ai_agent_headless as h;
let dir = h::default_plugins_dir(workspace_root);
let plugins = h::discover_plugins(&dir);
for p in &plugins { eprintln!("{}", h::summarize_plugin(p)); }

// 跟 TUI 一样组装系统提示 + skill。
let prompt = h::compose_system_prompt_with_plugins(base_prompt, &plugins);
let skills = h::find_skills_with_plugins(&skills_dir, &plugins)?;
```

---

## TUI 内表现

| Slash 命令 | 效果 |
|---|---|
| `/plugins` | 只读遮罩面板，列出所有发现的插件（manifest name + version + description + 各子目录条目数）|
| `/skills` | 插件 skill 跟 workspace skill 一起出现 |
| `/theme` | 插件主题在选择器里 |

Phase B 设计上就是只读 —— 安装 / 启用 / 禁用留给 Phase C 配合 `lazy-gagent` 管理器一起。

---

## 端到端示例

```text
<workspace>/.grain/plugins/rust-helper/
├── plugin.toml         # name = "rust-helper"
├── skills/
│   └── clippy/SKILL.md
├── themes/
│   └── rust-night.toml
├── prompts/
│   ├── 01-rules.md     # 「提交前先跑 cargo clippy」
│   └── 02-style.md
└── scripts/
    └── cargo-helper.js # grain.register_tool({ name: "cargo_check", ... })
```

启动日志：
```text
[info] plugin 'rust-helper' (skills: 1, themes: 1, scripts: 1, prompts: 2)
[info] system prompt pinned (... bytes, ...)
[info] loaded 1 JS tool(s) from 2 dir(s) (1 from plugins)
```

TUI 里：

- `/plugins` 显示 `rust-helper` 卡片及各子目录条目数
- `/skills` 列出 `clippy`（以及其它 plugin / workspace skill）
- `/theme` 列出 `rust-night`
- LLM 看到的系统提示尾巴是 `## Plugin: rust-helper\n\n提交前先跑 cargo clippy。`
- 模型可以调用 `cargo-helper.js` 里注册的 `cargo_check` 工具

---

## Phase 状态

| Phase | 范围 | 状态 |
|---|---|---|
| **A** | 发现 + skills/themes 合并 | ✓ 已发布 |
| **B-1** | scripts 合并到同一个 Boa worker | ✓ 已发布 |
| **B-2** | `/plugins` 遮罩面板 UI | ✓ 已发布 |
| **B-3** | `prompts/*.md` append 到系统提示 | ✓ 已发布 |
| **C-0** | `plugin.toml` 声明式安装（git + 本地 symlink）| ✓ 已发布 |
| **C-1** | `lazy-gagent` 管理器插件：让 agent 通过工具调用 install / update / remove | 计划中 |

---

## 模块定位

- [`grain-ai-agent-headless::plugins`](../../grain-ai-agent-headless/src/plugins.rs) —— manifest 类型、发现、集成 helper（`find_skills_with_plugins`、`compose_system_prompt_with_plugins`、`plugin_script_dirs`、`PromptFragment`）
- [`grain-script-boa`](../../grain-script-boa/src/extension.rs) —— `BoaExtension::from_scripts_dirs(&[Path])` 多目录 JS 加载
- [`grain-ai-agent-tui`](../../grain-ai-agent-tui/src/agent_worker.rs) —— `spawn()` 里串起 prompt + skill + script，`/plugins` 遮罩面板在 `app.rs` + `ui.rs`
- [`lazy-gagent`](../../lazy-gagent/src/lib.rs) —— 未来 Phase C 管理器的 placeholder crate
