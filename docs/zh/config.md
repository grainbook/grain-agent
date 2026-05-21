# 配置文件

`grain-headless` 从两处读可选 TOML 配置。加载顺序：workspace 覆盖 user，CLI flag 覆盖两者。

English version: [../config.md](../config.md).

## 位置

1. **CLI flag**（最高优先级；config 不能覆盖）。
2. **workspace**：`<workspace>/.grain/config.toml`。
3. **user XDG**：`~/.config/grain/config.toml`（或 `dirs` crate 给的对应平台路径）。
4. **内置默认**（clap `default_value_t`）。

任何一层文件不存在都没问题，自动落到下一层。

## Schema

每个字段都可选。**未知字段会报错**（让 typo 立刻浮上来）。

```toml
# 默认模型
model = "anthropic/claude-sonnet-4-5"

# context-guard 截断时预留给系统提示+回复的 token
headroom_tokens = 4096

# inline 显示 LLM 的思考 delta（暗色字）
show_thinking = false

# "none" 或 "common"（Kimi + SiliconFlow OpenAI-compat preset）
openai_compat = "common"

# Capability 开关——设 true 默认启用
allow_write = false
allow_bash = false
allow_web = false
allow_semantic_search = false

# 覆盖默认的 .claude/skills
skills_dir = ".claude/skills"
```

## explicit-vs-default 语义

CLI 用 clap 的 `value_source()` 区分"用户在命令行上明确传了"和"用户接受了默认值"：

- 用户传了 `--allow-bash`（即使值是 `false`），config 的 `allow_bash` **被忽略**。
- 用户没传，config 的值生效——包括 `allow_bash = false`，让它真的 off。

所以 config 的布尔字段是**双向**的：从 config 可以既开启也关闭，没有令人意外的回退。

## 例子：按项目配置

`~/code/risky-project/.grain/config.toml`：

```toml
# 敏感项目，收紧默认值
allow_write = false
allow_bash = false
allow_web = false
headroom_tokens = 8192
model = "anthropic/claude-haiku-4-5"  # 这里用便宜的模型够了
```

`~/code/sandbox/.grain/config.toml`：

```toml
# 实验场——放开
allow_write = true
allow_bash = true
allow_web = true
```

`~/.config/grain/config.toml`：

```toml
# 全局默认
show_thinking = true
openai_compat = "common"
```
