# `grain-headless` CLI 参考

`grain-headless` 是 `grain-ai-agent-headless` crate 里附带的二进制。它是基于这个 workspace 其它部分搭起来的开箱即用 coding agent；这一页是它的权威 flag/行为参考。

English version: [../headless-cli.md](../headless-cli.md).

## 编译和安装

```bash
cargo build --release -p grain-ai-agent-headless --bin grain-headless
```

二进制在 `target/release/grain-headless`。喜欢的话往 `$PATH` 里建软链。

启用可选的 `semantic_search` 工具（rig + OpenAI embedding）：

```bash
cargo build --release -p grain-ai-agent-headless --bin grain-headless --features rig
```

## 一行命令上手

```bash
grain-headless -C ./my-project --prompt "main.rs 写了什么？"
```

默认只读运行，stdout 输出流式事件日志。

## 命令行参数

### 工作目录与 prompt

| Flag | 默认 | 说明 |
|------|------|------|
| `-C, --workspace <path>` | `.` | 工作目录根；文件工具拒绝读/写在它之外的位置 |
| `-p, --prompt <text>` | (stdin) | 用户消息；不传则从 stdin 读 |
| `--system-prompt-file <path>` | (内置) | 用自定义文件覆盖默认系统提示 |

### 模型 + LLM provider

| Flag | 默认 | 说明 |
|------|------|------|
| `-m, --model <id>` | `anthropic/claude-sonnet-4-5` | 任意内嵌 models.dev snapshot 里的 id（见 [llm-models.md](./llm-models.md)） |
| `--openai-compat <none\|common>` | `common` | 注册 Kimi / SiliconFlow 这种 OpenAI-compat 端点 |
| `--headroom-tokens <n>` | `4096` | context-guard 截断时给系统提示+回复预留 |
| `--show-thinking` | off | 把思考块的 delta 打成暗色字 |

### Capability 开关（默认全 off）

| Flag | 注册的工具 |
|------|-----------|
| `--allow-write` | `write` + `edit` |
| `--allow-bash` | `bash`（`/bin/sh -c`，带 kill-on-drop 和 timeout） |
| `--allow-web` | `web_fetch`（带 SSRF 防护 + redirect 上限） |
| `--allow-semantic-search` | `semantic_search`（需 `--features rig` 编译时 + `OPENAI_API_KEY`） |

### 交互 + 持久化

| Flag | 说明 |
|------|------|
| `-i, --interactive` | 多轮交互循环；`/help` 看 slash 命令；`/exit` 或 Ctrl-D 退出 |
| `--session <path>` | 跨次保留的 JSONL transcript；启动时载入，每条消息追加 |
| `--telemetry-file <path>` | 可选审计日志，每行一个事件 JSON。注意见 [telemetry.md](./telemetry.md) 里的敏感数据警告 |
| `--skills-dir <path>` | 覆盖默认 `<workspace>/.claude/skills` |

### 输出 / 诊断

| Flag | 说明 |
|------|------|
| `--output <text\|json>` | text 给人看；json 每行一个事件，方便管道到 `jq` |
| `--doctor` | 打印工作目录 + provider key + git 状态，**不调用 LLM**；退出 0 |

## Slash 命令（只在 `--interactive` 下）

| 命令 | 作用 |
|------|------|
| `/help` | 内置帮助 |
| `/clear`（或 `/reset`） | 清空内存 transcript **同时**截断 `--session` 文件（如果有） |
| `/skills` | 列出发现的 skills |
| `/doctor` | 同 `--doctor` 但 inline 输出 |
| `/source`（或 `/git`） | 显示工作目录 git 状态 |
| `/compact` | 占位（真正的 compaction 走 `compaction_prepare_next_turn` API） |
| `/exit`（或 `/quit`、`/q`） | 退出循环 |

不以 `/` 开头的输入会作为下一条 prompt 发给 LLM。

## Skills（磁盘加载）

在 `<workspace>/.claude/skills/<name>/SKILL.md` 放一份，最小 frontmatter：

```markdown
---
name: rust-helper
description: 帮助 Rust 代码；优先 cargo check 验证而不是直接改文件。
disable_model_invocation: false
---

（skill 正文——agent 按需读）
```

发现的 skills 会自动追加到系统提示，作为 `<available_skills>` 块（见 [harness-system-prompt.md](./harness-system-prompt.md)）。

symlink 形式的 skill 目录 / SKILL.md 出于安全考虑会被拒绝。

## 配置文件

可选 TOML，位于 `<workspace>/.grain/config.toml` 和/或 `~/.config/grain/config.toml`。Workspace 覆盖 user，CLI flag 覆盖两者。每个字段都可选。

```toml
model = "anthropic/claude-sonnet-4-5"
headroom_tokens = 4096
show_thinking = false
openai_compat = "common"
allow_write = false
allow_bash = false
allow_web = false
allow_semantic_search = false
skills_dir = ".claude/skills"
```

布尔字段双向生效——`allow_bash = false` 会真的让它 off，除非 CLI 上传了 `--allow-bash`。

## 环境变量

genai builder 按 provider 自动检测 key：

| Provider | 变量名 |
|---------|--------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google | `GEMINI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| xAI | `XAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| Cohere | `COHERE_API_KEY` |
| Kimi（Moonshot） | `MOONSHOT_API_KEY` |
| SiliconFlow | `SILICONFLOW_API_KEY` |
| Zhipu（BigModel） | `ZHIPU_API_KEY` |

跑 `grain-headless --doctor` 看你 shell 里检测到了哪些 key。
