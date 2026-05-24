# MCP Adapter

A grain WASM plugin that provides lazy, on-demand access to Model Context
Protocol (MCP) servers.

## Purpose

MCP tool definitions are verbose. A server with 30 tools can easily
burn 10k+ tokens in the system prompt — paid whether or not the tools
are used. This adapter replaces all those definitions with a single
~200 token proxy tool named `mcp`.

The agent discovers tools via `search`, inspects parameters via
`describe`, and invokes via `tool` (+ `args` as JSON string). Servers
connect automatically on first use.

## Build

```bash
cd grain-plugin-wasm/examples/mcp-adapter
cargo component build --release
cp target/wasm32-wasip1/release/mcp_adapter.wasm plugin.wasm
```

## Configuration

Set the `MCP_SERVERS` environment variable to a JSON array:

```json
[
  {
    "name": "github",
    "url": "https://api.githubcopilot.com/mcp/",
    "headers": {
      "Authorization": "Bearer ${GITHUB_TOKEN}"
    },
    "description": "GitHub API tools"
  }
]
```

| Field         | Required | Notes                                |
|---------------|----------|--------------------------------------|
| `name`        | yes      | Used as a prefix for tool names.     |
| `url`         | yes      | StreamableHTTP MCP endpoint.         |
| `headers`     | no       | `${VAR}` interpolation supported.    |
| `description` | no       | Shown in the status listing.         |

## Usage (by the LLM)

| Mode     | `mcp` call                                                              |
|----------|-------------------------------------------------------------------------|
| Status   | `mcp({})`                                                               |
| Search   | `mcp({ search: "screenshot navigate" })`                                |
| Describe | `mcp({ describe: "github_search_repositories" })`                       |
| Call     | `mcp({ tool: "github_search_repositories", args: '{"q":"rust"}' })`     |
| Server   | `mcp({ server: "github" })`                                             |
| Connect  | `mcp({ connect: "github" })`                                            |

Tool names can be bare when there is no ambiguity; prefix with
`server_` (e.g. `github_search_repositories`) to disambiguate.

## Why "lazy"?

Ordinary MCP integration in Grain would register every remote tool as
a first-class `AgentTool`, bloating the system prompt. This adapter
registers **one** tool. The LLM discovers and calls specific MCP
tools through it, keeping the prompt lean.

## Ported from

[pi-mcp-adapter](https://github.com/nicobailon/pi-mcp-adapter) by
nicobailon, adapted for the grain-agent Component Model plugin system.
