# echo-plugin

Minimal grain WASM plugin example. Exports one tool ("echo") that returns its arguments verbatim.

## Prerequisites

```sh
# Install cargo-component
cargo install cargo-component

# Add the wasm32-wasip2 target
rustup target add wasm32-wasip2
```

## Build

```sh
cargo component build --release
```

The compiled component is at `target/wasm32-wasip2/release/echo_plugin.wasm`.

## Install

Copy the `.wasm` into a grain plugin directory:

```sh
mkdir -p <workspace>/.grain/plugins/echo/
cp target/wasm32-wasip2/release/echo_plugin.wasm <workspace>/.grain/plugins/echo/plugin.wasm

cat > <workspace>/.grain/plugins/echo/plugin.toml <<TOML
name = "echo"
version = "0.1.0"
description = "Echo tool — returns arguments verbatim"

[wasm]
module = "plugin.wasm"
capabilities = ["log"]
TOML
```

Launch grain with `--features wasm-plugins` and the echo tool will appear in the agent's tool list.
