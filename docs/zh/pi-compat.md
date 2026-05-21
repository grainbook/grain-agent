# pi 扩展兼容（`grain-pi-compat`）

让 [pi.dev 风格的扩展](https://pi.dev/docs/latest/extensions)直接在 grain agent runtime 上跑。基于 [`grain-script-boa`](./scripting.md) 的源码级 shim —— 没有独立 JS 引擎。

English: [../pi-compat.md](../pi-compat.md).

---

## 现在可用

| pi API | 状态 |
|--------|------|
| `export default (pi) => {...}` 工厂入口 | ✅ |
| `pi.registerTool({ name, description, parameters, execute })` | ✅ |
| `pi.registerCommand(name, { description, handler })` | ✅ |
| `pi.registerShortcut(keys, { description, handler })` | ✅（lib 端；TUI 键盘派发是另一个 task） |
| `pi.on(event, handler)` | ✅ —— 见下"事件"小节 |
| `pi.ui.notify(text)` | ✅ —— 即发即忘 toast |
| `pi.ui.confirm(prompt)` | ✅ —— 同步 yes/no |
| `pi.ui.input(prompt)` | ✅ —— 同步文本输入 |
| `pi.ui.select(prompt, items)` | ✅ —— 同步列表选择 |
| TypeScript（`.ts`）源 | ⏸ swc 和 ratatui 的 `unicode-width` 版本冲突，先用 `.js` |
| `ctx.newSession / fork / switchSession` | ❌ 不在本层 |
| `pi.appendEntry`（session state） | ❌ |
| `session_start` / `session_shutdown` / `before_agent_start` / `input` 事件 | ❌ —— 没有 grain 对等物 |
| npm 包扩展 | ❌ —— 超出范围 |

---

## 发现路径

按顺序查（同目录内按字母序）：

1. `<workspace>/.pi/extensions/*.js` —— 项目级
2. `~/.pi/agent/extensions/*.js` —— 用户级

目录不存在不报错。需要显式路径用 `PiExtension::from_dirs(&[...])`。

---

## shim 工作原理

对每个 pi extension 文件，loader 在头部 prepend 一段 JS shim：定义 `pi` 对象，方法把 pi 的 camelCase + 字段名（`parameters`、`execute` …）翻译成 grain 的 snake_case + 字段名（`schema`、`run` …）。源码以 `export default <expr>` 开头时，shim 还会去掉 `export default` 关键字，把表达式包成 `(<expr>)(pi);`，让工厂被我们的 `pi` 对象调用。

变换后的源码写到 `tempfile::TempDir`，再交给 `BoaExtension::from_scripts_dir(...)` —— 所以重活（Boa worker、工具注册、回调派发）都和[脚本层](./scripting.md)共用。

意味着 pi.dev 文档里直接拷贝的 extension（支持范围内的 API）**改名 `.ts` → `.js` 即可不改一行就跑**。

---

## `pi.on(...)` 桥接的事件

| pi 事件名 | grain 来源 | payload 形状 |
|----------|-----------|-------------|
| `agent_start` | `AgentEvent::AgentStart` | `{}` |
| `agent_end` | `AgentEvent::AgentEnd { messages }` | `{ message_count }` |
| `message_start` | `AgentEvent::MessageStart` | `{ role }` |
| `message_end` | `AgentEvent::MessageEnd` | `{ role }` |
| `tool_call` | `AgentEvent::ToolExecutionStart` | `{ tool_call_id, tool_name, args }` |
| `tool_result` | `AgentEvent::ToolExecutionEnd` | `{ tool_call_id, tool_name, is_error, content }` |

不支持的事件名（`session_*` / `before_agent_start` / `input`）订阅时静默 no-op。

启动时挂一次 listener：

```rust
for listener in pi_ext.listeners() {
    agent.subscribe(listener).await;
}
```

---

## Rust API（`PiExtension`）

```rust
use grain_pi_compat::{PiExtension, PiNotification};

// 发现
let ext = PiExtension::from_pi_dirs(workspace_root)?;
// 或显式：
let ext = PiExtension::from_dirs(&[PathBuf::from("./.pi/extensions")])?;

// 工具喂给 AgentOptions::tools
let tools = ext.tools();

// 事件桥接 —— 挂到 Agent::subscribe
for listener in ext.listeners() { agent.subscribe(listener).await; }

// 命令（TUI：合并进 SLASH_CATALOG）
for cmd in ext.commands() { /* 在 palette 显示 */ }
ext.invoke_command("audit", serde_json::json!({})).await?;

// 快捷键（TUI：拿 KeyEvent 来匹配）
for sc in ext.shortcuts() { /* 注册到 key dispatcher */ }
ext.invoke_shortcut("ctrl+x").await?;

// UI 通知队列（每个 UI tick 拉一次）
for note in ext.drain_notifications() {
    match note {
        PiNotification::Notify { text } => { /* 渲染 toast */ }
        PiNotification::Confirm { request_id, prompt } => {
            // 显示 modal，然后：
            ext.resolve_modal(request_id, serde_json::json!(true))?;
        }
        PiNotification::Input { request_id, prompt } => { /* … */ }
        PiNotification::Select { request_id, prompt, items } => { /* … */ }
    }
}
```

---

## 示例 pi extension（原样可跑）

```js
// .pi/extensions/example.js
export default (pi) => {
  pi.registerTool({
    name: "shout",
    description: "Uppercases text",
    parameters: { type: "object", properties: { text: { type: "string" }}, required: ["text"] },
    execute: (a) => a.text.toUpperCase(),
  });

  pi.registerCommand("greet", {
    description: "Ask + greet",
    handler: () => {
      const who = pi.ui.input("Who are you?");
      const ok = pi.ui.confirm(`Hello, ${who}. Continue?`);
      if (!ok) { pi.ui.notify("aborted"); return; }
      const fruit = pi.ui.select("Favorite fruit?", ["apple", "banana", "cherry"]);
      pi.ui.notify(`${who} picked ${fruit}`);
    },
  });

  pi.registerShortcut("ctrl+x", {
    description: "Custom action",
    handler: () => { pi.ui.notify("ctrl+x pressed"); },
  });

  pi.on("agent_end", (event) => {
    pi.ui.notify(`agent finished after ${event.message_count} messages`);
  });
};
```

每一行都有测试覆盖。

---

## Modal 注意事项

`pi.ui.confirm / input / select` 会阻塞 Boa worker 直到 host 解决它。host **必须**最终调用 `PiExtension::resolve_modal(request_id, value)`，否则 worker 永远卡住。

- **命令 / 快捷键 handler 里安全**。用户主动触发命令，正在等弹层。
- **`pi.on(...)` listener 里危险**。agent 在 await 你 listener 的 BoxFuture；不及时 resolve modal 就 agent 停摆。除非保证 host 很快回，否则别在 listener 开 modal。

---

## 测试

```bash
cargo test -p grain-pi-compat
```

20 个单元 + 集成测试覆盖：工厂 + top-level 入口、JS 错误透传、所有事件类型、命令 + 快捷键的注册/派发、四种 `ui.*`（含 modal 完整往返）。

---

## 路线图（暂未做）

- **TypeScript 源** —— 要么找一个和 ratatui `unicode-width` pin 兼容的 swc dep tree，要么换 oxc / 手撸 type stripper。
- **`ctx.*` 参数传递** —— 当前 `pi.ui.notify(...)` 直接挂在 `pi` 上。pi extensions 如果显式用 handler 的 `ctx` 参数，需要小幅适配。
- **`pi.appendEntry` / session state** —— 依赖 `grain-agent-harness` 的 session-aware 扩展持久化。
- **热加载** —— 给 tempdir 加 `notify`-based watcher + 重建 Boa context。
