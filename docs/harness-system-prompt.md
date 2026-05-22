# `grain_agent_harness::system_prompt`

Renders an in-memory `Skill` list into the `<available_skills>` XML block that gets appended to a system prompt. Corresponds to `packages/agent/src/harness/system-prompt.ts` in the TS reference.

> Disk-based skill loading (TS's `harness/skills.ts`) is **not yet ported** — this module only renders an in-memory list.

中文版：[zh/harness-system-prompt.md](./zh/harness-system-prompt.md).

## `Skill`

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,                  // absolute SKILL.md path the model sees
    pub disable_model_invocation: bool,     // true → omit from the rendered block
    pub body: String,                       // full content after the frontmatter `---` fence
}
```

Serialized as `name` / `description` / `filePath` / `disableModelInvocation` / `body` (camelCase).

`body` is the entire text after the second `---` fence in the SKILL.md file. It's used by TUI slash-palette skill injection (Enter on a skill pastes its body into the input) and by any future "read skill on demand" mechanism. The system-prompt renderer ignores `body` — only `name`, `description`, `file_path`, and `disable_model_invocation` appear in the `<available_skills>` block.

## `format_skills_for_system_prompt`

```rust
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String;
```

Behavior:

- Drops entries with `disable_model_invocation = true`.
- Empty list returns **empty string** — safe to unconditionally `system_prompt.push_str(&format_skills_for_system_prompt(...))`.
- Non-empty starts with a fixed human-readable preamble:
  ```
  The following skills provide specialized instructions for specific tasks.
  Read the full skill file when the task matches its description.
  When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.
  ```
  Followed by an `<available_skills>` block with one `<skill>` per entry:
  ```xml
  <skill>
    <name>Bash</name>
    <description>Runs shell commands</description>
    <location>/skills/bash/SKILL.md</location>
  </skill>
  ```
- `&` / `<` / `>` / `"` / `'` in the text are XML-escaped.

## Usage

```rust
use grain_agent_harness::{format_skills_for_system_prompt, system_prompt::Skill};

let skills = vec![
    Skill {
        name: "Bash".into(),
        description: "Runs shell commands".into(),
        file_path: "/skills/bash/SKILL.md".into(),
        disable_model_invocation: false,
        body: String::new(),  // system prompt renderer ignores this field
    },
    Skill {
        name: "Internal".into(),
        description: "Internal-only helper".into(),
        file_path: "/skills/internal/SKILL.md".into(),
        disable_model_invocation: true,
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

agent.set_system_prompt(system_prompt).await;
```

## Notes

- This is pure string rendering — re-render and `Agent::set_system_prompt` when skills change.
- `Skill.file_path` should be an absolute path the model can read directly. The preamble explicitly tells the model to resolve relative paths against SKILL.md's directory.
- If you add more prompt fragments (e.g. system time, working directory), append them **before** the `<available_skills>` block, not inside it.
