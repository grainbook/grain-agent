# design-code-handoff

WASM v2 orchestration plugin for a two-role workflow:

- `designer` plans the implementation and calls `handoff_to_coder`.
- `coder` receives the structured plan as a new user message, then implements it.

Build:

```sh
cargo component build --release
cp target/wasm32-wasip2/release/design_code_handoff.wasm plugin.wasm
```

Install by copying or symlinking this directory into `<workspace>/.grain/plugins/design-code-handoff`.

Optional environment overrides:

```sh
DESIGNER_MODEL=openai/gpt-5.1-codex-mini
CODER_MODEL=deepseek/deepseek-v4-pro
DESIGNER_TOOLS=read,list,glob,grep,source_info,handoff_to_coder
CODER_TOOLS=read,list,glob,grep,source_info,write,edit,bash
```

The host validates models and tools before applying any switch. If write/bash tools are not enabled in the host, the `coder` role switch is rejected.
