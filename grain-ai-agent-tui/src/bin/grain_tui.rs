//! `grain-tui` binary entry point. Parses [`Args`] and hands off to
//! [`grain_ai_agent_tui::run_tui`].

use clap::Parser;
use grain_ai_agent_tui::{Args, run_tui};

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(e) = run_tui(args).await {
        eprintln!("grain-tui exited with error: {e}");
        std::process::exit(1);
    }
}
