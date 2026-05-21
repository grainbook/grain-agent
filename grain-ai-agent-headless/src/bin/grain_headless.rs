//! `grain-headless` — single-prompt coding-agent binary.
//!
//! ```bash
//! cargo run -p grain-ai-agent-headless --bin grain-headless -- \
//!     -C ./my-repo \
//!     --model anthropic/claude-sonnet-4-5 \
//!     --prompt "What does src/main.rs do?"
//! ```
//!
//! Reads the prompt from `--prompt`, or from stdin if omitted. Writes a
//! human-readable event log to stdout, exits 0 on a clean `AgentEnd`, or 1
//! if the loop produced an error. Required env vars depend on the provider
//! the model id resolves to (e.g. `ANTHROPIC_API_KEY` for Claude).

use clap::Parser;
use grain_ai_agent_headless::cli::{Args, CliError, run};

#[tokio::main]
async fn main() -> Result<(), CliError> {
    let args = Args::parse();
    run(args).await
}
