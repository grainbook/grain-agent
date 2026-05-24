# `grain-ai-agent-tui`

基于 ratatui 的终端 UI，跑在 [`grain-ai-agent-headless`](./headless-cli.md) 之上。同样的 coding-agent 能力（读/写/bash/web 工具、会话、技能、slash 命令），换成多面板交互。

English: [../headless-tui.md](../headless-tui.md).

---

## 安装 + 运行

```bash
cargo build --release -p grain-ai-agent-tui --bin grain-tui
export DEEPSEEK_API_KEY=...
./target/release/grain-tui -C ./my-project
```

`-C` 指定工作区根（文件工具拒绝越界访问）。默认模型 `deepseek/deepseek-chat`，可用 `--model` 或 `--provider <name>` ([providers.md](./providers.md)) 替换。

---

## 布局

无边框，四行布局；slash 弹层打开时自动让位：

```
HEADER         grain-tui  model · workspace · [caps] · theme:default
TRANSCRIPT     可滚动的对话历史
[PALETTE]      ← 仅在输入以 '/' 开头时显示
PROMPT         › 这里输入
FOOTER         快捷键提示
```

弹层（help / doctor / skills / 主题 / provider / log / session resume）是定尺寸居中卡片，背景色取自主题的 `surface` slot，和主视图明显区分。

**鼠标**：滚轮滚动 transcript（每档 3 行，跟 PgUp/PgDn 共享 follow-bottom 和追到底部即恢复跟踪的逻辑）。为了滚轮事件能到 app，鼠标 capture 是**开着**的，所以终端也会拦截左键拖选 —— 按住 **Option**（macOS）或 **Shift**（多数 Linux 终端）绕过 capture，即可用原生的拖选 / 右键复制。粘贴照常用 `⌘V` / `Ctrl-Shift-V`。

**实时状态栏**：agent 跑起来时 footer 显示 `✻ Marinating… (Xm Ys · ↑input · ↓output tokens · cache N%)` 再加一个颜色化成本 chip（`$0.012`）。每 5 秒换一个动词、墙钟耗时、当前一轮累计 token 用量、prefix-cache 命中率（`cache_read / input_total`）、按 `models.dev` 价格表实时算出的 USD 成本。颜色阈值：绿 <$0.05 / 黄 0.05–0.20 / 红 ≥0.20。空闲时消失。`models.dev` 没记录价格的模型不显示 cost chip。

---

## 按键

| 键 | 行为 |
|----|------|
| **Enter** | 提交。slash 弹层激活时：skill → 把正文注入输入框供审阅；命令 → 补齐到输入框并提交 |
| **Esc** | 三级优先：关闭弹层 → 清空输入 → 退出 |
| **Tab** | slash 弹层激活时补齐当前选中的内置命令；skill 行无视。其他情况空操作 |
| **Ctrl-C** | streaming 时中断当前 turn；空闲时直接退出（raw mode 下不会收到 SIGINT，所以这条必须手工处理） |
| **↑ / ↓** | 弹层激活时：导航；否则：浏览历史 prompt |
| **PgUp / PgDn** | 滚动 transcript（无论输入焦点状态） |
| **Home / End** | 输入框光标行首 / 行尾 |
| **F1 / F2 / F3** | help / doctor / skills 弹层 |

---

## Slash 命令

输入 `/` 立即弹出下拉补齐面板，边打边筛选；↑↓ 选择，Enter 执行 highlighted 项。

| 命令 | 作用 |
|------|------|
| `/help`, `/?` | 显示快捷键 + slash 参考 |
| `/clear`, `/reset` | 清空 transcript |
| `/doctor` | **带搜索框**的诊断报告 |
| `/skills` | 显示已加载技能 |
| `/theme` | 主题选择器 |
| `/provider` | provider profile 选择器（见 [providers.md](./providers.md)） |
| `/log` | 显示最近的请求体捕获（需要 `--debug-log`） |
| `/resume` | 打开会话恢复选择器（从 `--sessions-dir` 加载历史会话） |
| `/exit`, `/quit`, `/q` | 退出 |

### `/doctor` 搜索

弹层打开后直接打字：按 case-insensitive 子串过滤每一行（章节标题 `=== … ===` 永远保留方便定位）。PgUp/PgDn/Home/End 翻页，Backspace 缩窄，Esc 关闭。

常用过滤：`ANTHROPIC` / `OPENAI` / `DEEPSEEK` 查找某个 env key；`branch` / `commit` 跳到 git 块。

### `/resume` 会话恢复

打开居中弹层，列出 `--sessions-dir`（默认 `<workspace>/.grain/sessions/`）中的历史会话。每行显示最后一条用户 prompt（标题）、模型、消息数、修改时间。↑↓ 导航；Enter 向 transcript 输出一条重启提示：

```
(to resume: relaunch with `grain-tui --session /path/to/<uuid>.jsonl` — in-place /resume coming in Phase 4)
```

Esc 关闭。会话列表按最新优先排序；无法解析的文件以 `[warn]` 跳过。

### `/` skill 面板

输入 `/` 后，面板中除了内置 slash 命令，还会显示 pi 兼容 skill 目录中加载的 skill。每个 skill 以 `/skill:<名称>` 展示，附带描述。

- **在 skill 上按 Enter** → 将 skill 的完整正文注入输入框，供审阅后提交给 LLM。这会替换当前输入内容。
- **在命令上按 Enter** → 派发命令（原有行为）。
- **Tab** 只补齐内置命令，不补齐 skill 名称。
- `disable_model_invocation: true` 的 skill 不会出现在面板中。

---

## 主题

九个内置主题（参考 [ratatui-themes](https://github.com/ricardodantas/ratatui-themes)）：

`default`、`dracula`、`nord`、`gruvbox-dark`、`gruvbox-light`、`tokyo-night`、`catppuccin-mocha`、`solarized-dark`、`one-dark-pro`。

`/theme` 打开选择器：↑↓ + Enter。每行有 6 色 swatch 预览。

### 自定义主题

`<workspace>/.grain/themes/<name>.toml`（或 `--themes-dir <path>` 覆盖）：

```toml
name = "vaporwave"
[palette]
accent = "#ff71ce"
secondary = "#01cdfe"
fg = "#ffffff"
muted = "#7e7e7e"
error = "#ff6e6e"
warning = "#fff85e"
success = "#05ffa1"
info = "#b967ff"
surface = "#1a0033"
```

`surface` 可选，缺省回落到 `muted`。文件格式有误 `[warn]` 跳过、不阻断启动。`--theme <name>` 指定启动主题（默认 `default`）。

---

## Provider

`/provider` 打开 profile 选择器，运行时切换不需要重启。schema + 文件查找路径见 [providers.md](./providers.md)。

启动 profile 用 `--provider <name>`，覆盖文件路径用 `--providers-file <path>`。

---

## CLI flag 速查

| Flag | 默认 | 说明 |
|------|------|------|
| `-C, --workspace <DIR>` | `.` | 工作区根 |
| `-m, --model <ID>` | `deepseek/deepseek-chat` | 模型 id |
| `--system-prompt-file <PATH>` | 内置 | 自定义 system prompt |
| `--headroom-tokens <N>` | `4096` | context guard 保留量 |
| `--openai-compat <PRESET>` | `common` | `none` / `common` |
| `--show-thinking` | off | 把 thinking delta 显示到 transcript |
| `--allow-write` | off | Write / Edit 工具 |
| `--allow-bash` | off | Bash 工具（显式 opt-in） |
| `--allow-web` | off | WebFetch（显式 opt-in） |
| `--allow-semantic-search` | off | 需要 headless `--features rig` |
| `--session <FILE>` | 无 | JSONL 会话恢复。会覆盖 `--sessions-dir` 自动创建 |
| `--sessions-dir <DIR>` | `<workspace>/.grain/sessions` | 不传 `--session` 时在此目录自动创建 `<uuidv7>.jsonl`，确保每次运行都可通过 `/resume` 恢复 |
| `--skills-dir <DIR>` | pi 兼容默认目录 | 用单个显式文件/目录覆盖默认 skill 发现 |
| `--telemetry-file <FILE>` | 无 | 一行一个 `AgentEvent` JSON |
| `--tick-ms <MS>` | `100` | 渲染 tick 间隔 |
| `--theme <NAME>` | `default` | 启动主题 |
| `--themes-dir <DIR>` | `<workspace>/.grain/themes` | 自定义主题目录 |
| `--provider <NAME>` | 无 | 启动 provider profile |
| `--providers-file <FILE>` | 自动查找 | 覆盖 providers.toml 路径 |
| `--scripts-dir <DIR>` | `<workspace>/.grain/scripts` | JS 脚本扩展 [scripting.md](./scripting.md) —— 需要 `--features scripts-boa` |

---

## 架构

- **`AppState`** (`src/app.rs`) —— 纯 UI 状态机。每个按键 → 0 个或多个 `Command` + 状态变更。不依赖 ratatui / tokio，单元测试很轻。
- **`TuiEvent`** (`src/event.rs`) —— 单一事件信封：键盘、tick、resize、worker 转发的 `AgentEvent`、worker 回复（`OverlayDoctor` / `OverlaySkills` / `ProviderApplied` 等）。
- **`agent_worker`** (`src/agent_worker.rs`) —— 独立 tokio task 持有 `Agent`，通过 mpsc 桥接 `Command` 和 `TuiEvent`。`/provider` 切换走 `agent.set_model(...)`，不重启。同时通过 [`session_discovery::new_session_path`](./headless-session-discovery.md) 管理 session 自动创建，通过 [`session_discovery::list_sessions`](./headless-session-discovery.md) 支持 `/resume` 选择器。
- **`ui`** (`src/ui.rs`) —— 纯渲染函数，输入 `&AppState`。
- **`run`** (`src/run.rs`) —— 终端生命周期（raw mode + alt screen）+ event 轮询 + 渲染循环。

UI 线程从不直接碰 `Agent` —— 所有交互都走 channel。
