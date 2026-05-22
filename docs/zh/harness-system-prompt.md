# `grain_agent_harness::system_prompt`

把已加载的 skill 列表渲染成 system prompt 里的 `<available_skills>` XML 块。对应 TS 参考实现 `packages/agent/src/harness/system-prompt.ts`。

> 注意：从磁盘读取 skill 文件的逻辑（TS 端的 `harness/skills.ts`）**尚未移植**——这个模块只负责把内存中已有的 `Skill[]` 转成可注入的 prompt 文本。

## `Skill`

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,                  // 模型看到的 SKILL.md 绝对路径
    pub disable_model_invocation: bool,     // true → 不出现在渲染结果中
    pub body: String,                       // frontmatter `---` 之后的完整正文
}
```

`serde(rename_all = "camelCase")`，所以序列化字段是 `name` / `description` / `filePath` / `disableModelInvocation` / `body`。

`body` 是 SKILL.md 中第二个 `---` 分隔符之后的全部文本。TUI 的 slash 面板 skill 注入（在 skill 上按 Enter 把正文贴到输入框）和未来的「按需读取 skill」机制都会用到它。system prompt 渲染器忽略 `body`——只有 `name`、`description`、`file_path`、`disable_model_invocation` 会出现在 `<available_skills>` 块中。

## `format_skills_for_system_prompt`

```rust
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String;
```

行为：

- 过滤掉 `disable_model_invocation = true` 的项。
- 列表为空时返回**空字符串**——可以无条件 `system_prompt.push_str(&format_skills_for_system_prompt(...))`。
- 非空时先输出一段固定的人类说明：
  ```
  The following skills provide specialized instructions for specific tasks.
  Read the full skill file when the task matches its description.
  When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.
  ```
  紧接着是 `<available_skills>` ... `</available_skills>` 块，每个 skill 形如：
  ```xml
  <skill>
    <name>Bash</name>
    <description>Runs shell commands</description>
    <location>/skills/bash/SKILL.md</location>
  </skill>
  ```
- 文本内的 `&` / `<` / `>` / `"` / `'` 自动 XML 转义。

## 使用

```rust
use grain_agent_harness::{format_skills_for_system_prompt, system_prompt::Skill};

let skills = vec![
    Skill {
        name: "Bash".into(),
        description: "Runs shell commands".into(),
        file_path: "/skills/bash/SKILL.md".into(),
        disable_model_invocation: false,
        body: String::new(),  // system prompt 渲染器忽略此字段
    },
    Skill {
        name: "Internal".into(),
        description: "Internal-only helper".into(),
        file_path: "/skills/internal/SKILL.md".into(),
        disable_model_invocation: true,  // 不进入 prompt
        body: String::new(),
    },
];

let base_prompt = "You are a helpful agent.";
let skills_block = format_skills_for_system_prompt(&skills);

let system_prompt = if skills_block.is_empty() {
    base_prompt.to_string()
} else {
    format!("{base_prompt}\n\n{skills_block}")
};

// 接下来把 system_prompt 设到 Agent：
agent.set_system_prompt(system_prompt).await;
```

## 提示

- 这里只是字符串渲染——是否在合适的时机重新生成、是否在 skill 变更后通过 `Agent::set_system_prompt` 同步给运行中的 agent，是调用方的责任。
- `Skill.file_path` 应该是模型能直接拿去读的绝对路径；fragment 里专门提醒了“引用相对路径时按 SKILL.md 所在目录解析”。
- 如果你要在 prompt 里加更多模板片段（比如系统时间、工作目录），把它们拼到 `format_skills_for_system_prompt` 的结果**前面**而不是中间——避免污染 `<available_skills>` 块。
