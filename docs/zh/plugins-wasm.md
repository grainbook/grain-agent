# WebAssembly 插件指南

Grain 支持编译为 WebAssembly Component Model 模块的插件。将 `.wasm` 文件与 `plugin.toml` 放在一起，grain 会在运行时加载它——无需重新编译 grain 即可添加插件。

英文版：[../plugins-wasm.md](../plugins-wasm.md)。

---

## 前置要求

- grain 构建时启用 `--features wasm-plugins`（`grain-ai-agent-headless` 和 `grain-ai-agent-tui` 都需要）
- 编写插件需要：[cargo-component](https://github.com/bytecodealliance/cargo-component) + `rustup target add wasm32-wasip2`

---

## 工作原理

```text
.grain/plugins/my-tool/
  plugin.toml         # 清单，可选 [wasm] 段
  plugin.wasm         # 编译后的 Component Model 模块
```

启动时，插件引擎会：
1. 扫描 `<workspace>/.grain/plugins/` 中含 `plugin.toml` 的目录
2. 对于有 `.wasm` 模块的插件（默认为 `plugin.wasm`，或 `[wasm].module` 指定的路径），通过 [wasmtime](https://wasmtime.dev/) 加载
3. 调用插件的 `init` 导出获取元数据
4. 调用 `list-tools` 枚举插件提供的工具
5. 将每个工具包装为 `AgentTool` 并追加到代理的工具列表中

---

## 编写插件

### 1. 创建项目

```sh
cargo component new my-plugin --lib
cd my-plugin
```

### 2. 复制 WIT 合约

将 `grain-plugin-wasm/wit/grain-plugin.wit` 复制到项目的 `wit/` 目录。该文件定义了插件必须实现的接口。

### 3. 实现导出

```rust
// src/lib.rs
wit_bindgen::generate!({
    world: "grain-plugin",
    path: "wit",
});

struct MyPlugin;

impl Guest for MyPlugin {
    fn init() -> Result<PluginInfo, String> {
        Ok(PluginInfo {
            name: "my-plugin".to_string(),
            version: "0.1.0".to_string(),
        })
    }

    fn list_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "my_tool".to_string(),
            label: "My Tool".to_string(),
            description: "做一些有用的事情".to_string(),
            parameters_json: r#"{
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "输入文本"
                    }
                },
                "required": ["input"]
            }"#.to_string(),
        }]
    }

    fn call_tool(name: String, args_json: String) -> ToolResult {
        match name.as_str() {
            "my_tool" => {
                ToolResult {
                    content_json: format!(r#"{{"result": "processed"}}"#),
                    is_error: false,
                }
            }
            _ => ToolResult {
                content_json: format!(r#"{{"error": "unknown tool: {name}"}}"#),
                is_error: true,
            },
        }
    }
}

export!(MyPlugin);
```

### 4. 构建

```sh
cargo component build --release
```

编译后的组件位于 `target/wasm32-wasip2/release/my_plugin.wasm`。

### 5. 安装

```sh
mkdir -p <workspace>/.grain/plugins/my-plugin/
cp target/wasm32-wasip2/release/my_plugin.wasm \
   <workspace>/.grain/plugins/my-plugin/plugin.wasm
```

创建清单文件：

```toml
# <workspace>/.grain/plugins/my-plugin/plugin.toml
name = "my-plugin"
version = "0.1.0"
description = "我的自定义工具"

[wasm]
module = "plugin.wasm"
capabilities = ["log"]
```

---

## 清单：`[wasm]` 段

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `module` | string | `"plugin.wasm"` | `.wasm` 文件相对于插件根目录的路径 |
| `capabilities` | list | `["log"]` | 插件需要的宿主能力 |

当省略 `[wasm]` 段但磁盘上存在 `<root>/plugin.wasm` 时，引擎会自动检测并使用默认能力（仅 `["log"]`）。

---

## 宿主能力

每个宿主函数都受插件声明的能力约束。调用未授权的能力会向客户端返回错误。

| 能力 | 函数 | 说明 |
|---|---|---|
| `log` | `log(level, msg)` | 将日志行输出到 stderr：`[level] wasm plugin 'name': msg` |
| `env` | `env-get(key)` | 读取环境变量。未授权时返回 `none`。 |
| `http` | `http-get(...)`、`http-post(...)` | HTTP 请求。未授权时返回错误字符串。 |

### 安全模型

- 插件在沙箱化的 wasmtime 实例中运行，**无文件系统访问**（WASI 已链接但上下文为空）
- 每次工具调用都获得全新的 `Store`——调用之间没有共享的可变状态
- 宿主 HTTP 调用通过 grain 进程的网络栈（受相同的代理/防火墙规则约束）
- 环境变量访问是可选的——需要在 capabilities 中列出 `env`
- 插件无法访问宿主接口未明确提供的任何资源

---

## WIT 合约

完整合约位于 `grain-plugin-wasm/wit/grain-plugin.wit`。关键类型：

```wit
// 插件导出（你需要实现的）
interface plugin {
    record tool-def {
        name: string,
        label: string,
        description: string,
        parameters-json: string,   // JSON Schema
    }
    record tool-result {
        content-json: string,
        is-error: bool,
    }
    record plugin-info {
        name: string,
        version: string,
    }
    init: func() -> result<plugin-info, string>;
    list-tools: func() -> list<tool-def>;
    call-tool: func(name: string, args-json: string) -> tool-result;
}

// 宿主导入（grain 提供的）
interface host {
    log: func(level: log-level, msg: string);
    env-get: func(key: string) -> option<string>;
    http-get: func(...) -> result<http-response, string>;
    http-post: func(...) -> result<http-response, string>;
}
```

---

## 工具执行路径

```text
Agent 调用工具 "my_tool"
  -> WasmTool::execute(args)
       -> 将参数序列化为 JSON 字符串
       -> tokio::task::spawn_blocking
            -> 创建新的 wasmtime Store
            -> 实例化 Component
            -> 调用客户端 init()
            -> 调用客户端 call-tool("my_tool", args_json)
            -> 反序列化结果
       -> 将 AgentToolResult 返回给代理循环
```

`spawn_blocking` 确保同步的 wasmtime 执行不会阻塞异步代理循环。

---

## 限制

- **客户端无异步**：插件在 wasmtime 内同步运行。长时间运行的操作会阻塞 `spawn_blocking` 线程。
- **每个插件单线程**：每次工具调用都有自己的 Store；没有对插件状态的并发访问。
- **无流式传输**：工具结果作为单个 JSON 字符串返回，不会流式传输。
- **JSON 输入/JSON 输出**：参数和结果序列化为 JSON 字符串。没有二进制协议。
- **每次调用全新状态**：每次调用都创建新的 Store 并重新调用 `init`。插件无法在调用之间保持状态。
- **wasmtime 版本**：grain 使用 wasmtime 40.0.4（MSRV：rustc 1.89.0）。插件必须与此版本的 Component Model 兼容。

---

## 故障排除

| 症状 | 原因 | 解决方法 |
|---|---|---|
| `wasm plugin(s) found but binary was built without --features wasm-plugins` | 未启用特性 | 使用 `--features wasm-plugins` 重新构建 |
| `wasmtime runtime init failed` | 引擎配置问题 | 检查 wasmtime 与平台的兼容性 |
| `plugin init failed: ...` | 插件的 `init()` 返回了 `Err` | 修复插件的 init 函数 |
| `http capability not granted` | 插件在 capabilities 中未声明 `"http"` 就调用了 `http-get`/`http-post` | 在 `plugin.toml` 的 `capabilities` 中添加 `"http"` |

---

## 模块布局

| Crate | 职责 |
|---|---|
| `grain-plugin-wasm` | Wasmtime 运行时、宿主函数、`WasmTool` 适配器 |
| `grain-plugin-wasm/wit/` | WIT 合约（`grain:plugin` world） |
| `grain-ai-agent-headless::plugins` | 清单中的 `WasmConfig`、`Plugin::wasm_module()` |
| `grain-ai-agent-tui::agent_worker` | 发现 + 加载接线（在 `wasm-plugins` 特性后面） |
