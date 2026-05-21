# JavaScript 脚本扩展（`grain-script-boa`）

基于 Boa 的 JS 层，用户在目录里丢 `.js` 文件即可在 agent 运行时注册工具（以及更多）—— 无需重新编译。

English: [../scripting.md](../scripting.md).

走 pi.dev 风格扩展（`export default (pi) => {...}`、`pi.registerTool` 等）请见 [pi-compat.md](./pi-compat.md)，那是这一层之上的薄源码转换 shim。

---

## 是什么

- 新增 workspace crate：`grain-script-boa`。
- 内嵌 [`boa_engine`](https://github.com/boa-dev/boa)（纯 Rust ES2022+ 解释器，无 JIT）。
- 起一个专用 worker 线程独占一个 `boa_engine::Context` —— 引擎是 `!Send`，不能跨 tokio task 共享。
- 通过 `mpsc` channel 与 agent 其余部分通信。
- 输出为 `Vec<Arc<dyn AgentTool>>`，可直接合并到 agent 的工具列表。

---

## 快速上手

启用 `scripts-boa` cargo feature（headless 或 TUI 任一）：

```bash
cargo install --path grain-ai-agent-headless --features scripts-boa
# 或 TUI
cargo build --release -p grain-ai-agent-tui --bin grain-tui --features scripts-boa
```

往 `<workspace>/.grain/scripts/` 丢一个脚本：

```js
// .grain/scripts/shout.js
grain.register_tool({
  name: "shout",
  description: "Uppercases the input text",
  schema: { type: "object", properties: { text: { type: "string" }}, required: ["text"] },
  run: (args) => args.text.toUpperCase(),
});
```

跑 agent：

```bash
DEEPSEEK_API_KEY=... grain-headless --prompt "use the shout tool to scream 'hello'"
```

启动时会打印 `[info] loaded 1 JS tool(s) from .grain/scripts`，`shout` 工具会和内置 read/write/bash 一样可被调用。

---

## CLI flag

| Flag | 默认 | 备注 |
|------|------|------|
| `--scripts-dir <DIR>` | `<workspace>/.grain/scripts` | 覆盖发现路径 |
| `--features scripts-boa`（build 时） | off | 总开关。不开 feature 时 `--scripts-dir` 仍可接收，但会打 warn 然后忽略脚本 |

headless 和 TUI 的 flag 一致。

---

## JS API（`grain` 全局）

worker 注册一个 `grain` 全局对象，方法：

| 方法 | 用途 |
|------|------|
| `grain.register_tool({ name, description, schema, run })` | 注册一个 `AgentTool`。`run(args)` 返回字符串或 `{ content, is_error? }`。 |
| `grain.register_callback(name, fn)` | 通用命名回调槽位。高层（如 pi-compat 事件）用它。 |
| `grain.register_meta(kind, name, attrs)` | 通用 `(kind, name) → attrs` 描述符存储。pi-compat 用它给 commands / shortcuts。 |
| `grain.push_notification(payload)` | 即发即忘；host 通过 `BoaExtension::drain_notifications()` 拉取。 |
| `grain.modal_request(kind, payload)` | **同步**往返。阻塞 worker 直到 host 调 `resolve_modal`。返回 host 给的值。 |

Boa 接受 ES2022+：顶层 `let`/`const`、箭头函数、解构、扩展、可选链、modules。没 JIT、没 Node 标准库、没 npm —— 纯 JS 语言。

---

## Rust API（`BoaExtension`）

```rust
use grain_script_boa::BoaExtension;

// 构造。目录不存在 → 空 extension。.js 语法错 → BoaExtensionError::ScriptLoad（带诊断）。
let ext = BoaExtension::from_scripts_dir("./.grain/scripts")?;

// 脚本注册的工具，可直接喂给 AgentOptions::tools。
let tools: Vec<Arc<dyn AgentTool>> = ext.tools();

// 派发命名回调到 JS。
let res: Result<(), String> = ext.invoke_callback("on:tool_call", json).await;

// 拉取即发即忘通知（如每个 UI tick 一次）。
let queued: Vec<serde_json::Value> = ext.drain_notifications();

// 取 kind 维度的描述符快照（pi-compat 用它枚举命令）。
let metas: Vec<(String, Value)> = ext.list_metas("command");

// 解决一个之前由 grain.modal_request 发起的同步 modal。
ext.resolve_modal(request_id, serde_json::json!(true))?;
```

`BoaExtension: Send + Sync`，给 task / listener 交互时用 `Arc<BoaExtension>` 是惯用形态。

---

## 并发模型

- Boa context 跑在一个专用 `std::thread`。
- 父端 ↔ worker 用两个 `mpsc` channel：
  - `cmd_tx`：LoadScript / ListTools / InvokeTool / InvokeCallback / ListMetas
  - `modal_tx`：`resolve_modal` 的响应
- 通知用一个**父端持有的** channel（`notify_rx`），所以即便 worker 在同步 modal 里阻塞，`drain_notifications()` 调用也能正常返回。

**Modal 坑：** `grain.modal_request(...)` 会阻塞 worker 线程直到 host 解决它。意味着：

- **工具调用 / 命令 handler 里** — 没问题，用户本来就在等 agent。
- **事件 listener 里** — 危险。agent 在 await listener 的 BoxFuture；如果你不及时 resolve modal，agent 会卡住。**别**在 `pi.on(...)` handler 里调 `pi.ui.confirm/input/select`，除非你保证 host 很快解决。

---

## 这个 crate 不包括什么

- **没有 file / HTTP / Node 标准库**：脚本不能直接读文件、fetch URL、`require`。沙箱保持紧。要扩展靠 Rust 加工具、JS 调它们。
- **不支持 TypeScript**：今天 worker 直接读 `*.js`。swc 转译被 ratatui 0.29 的 `unicode-width` pin 卡住了。详见 pi-compat。
- **没有 async JS**：`grain.modal_request` 是同步阻塞 worker。JS 里 Promise 当然能用但 host 这边没法 await。
- **没有热加载**：文件改动不自动重载，得重启。后续补 `notify`-based watcher。

---

## 测试

```bash
cargo test -p grain-script-boa
```

3 个单元测试覆盖 register / execute / 缺目录 / 语法错四条路径。modal / callback / meta 路径通过 `grain-pi-compat` 的 20+ 集成测试覆盖。
