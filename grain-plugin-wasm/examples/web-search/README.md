# web-search-plugin

A grain WASM plugin that adds two tools to the agent:

| Tool         | What it does                                           |
|--------------|--------------------------------------------------------|
| `web_search` | Search the live web via [Exa](https://exa.ai)          |
| `web_fetch`  | HTTP GET an arbitrary URL (body truncated to 16 KiB)   |

Inspired by [pi-web-access](https://github.com/nicobailon/pi-web-access).
Anything that requires native deps (ffmpeg / browser cookies / PDF parsers /
YouTube transcript extraction) is out of scope here — the WASM Component
Model sandbox only has the host APIs we expose: `log`, `env-get`,
`http-get`, `http-post`.

## Prerequisites

```sh
cargo install cargo-component
rustup target add wasm32-wasip2
```

## Build

```sh
cd grain-plugin-wasm/examples/web-search
cargo component build --release
```

Output: `target/wasm32-wasip2/release/web_search_plugin.wasm`

## Install

```sh
mkdir -p .grain/plugins/web-search
cp target/wasm32-wasip2/release/web_search_plugin.wasm \
   .grain/plugins/web-search/plugin.wasm
cp plugin.toml.example .grain/plugins/web-search/plugin.toml
```

The manifest **must** grant `["http", "env", "log"]` capabilities — without
`http` the search call returns a capability error, without `env` the plugin
can't read `EXA_API_KEY`. Sample manifest:

```toml
name = "web-search"
version = "0.1.0"
description = "Exa search + URL fetch"

[wasm]
capabilities = ["http", "env", "log"]
```

Run grain with the `wasm-plugins` feature:

```sh
cargo build --features wasm-plugins
export EXA_API_KEY="your-exa-key"
./target/debug/grain-tui --workspace .
```

The agent should now have `web_search` and `web_fetch` in its tool list
(visible via `/skills`-style listings once the plugin loads).

## Notes

- Exa pricing: see https://exa.ai/pricing — free tier exists for testing.
- `web_fetch` truncates response bodies to 16 KiB to keep tool results
  bounded; for longer pages, ask the model to fetch ranges separately
  or implement a paginated variant.
- The plugin does no caching. If you re-query the same term, you pay
  Exa twice. A future revision could add a small in-host cache via a
  new host API.
