//! `grain-tui` binary entry point. Parses [`Args`], overlays config
//! from `<workspace>/.grain/config.toml` + `~/.config/grain/config.toml`
//! (via `grain_ai_agent_tui::config_apply::load_and_apply`), and hands
//! off to [`grain_ai_agent_tui::run_tui`].

use clap::Parser;
use grain_ai_agent_tui::{Args, config_apply, run_tui};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let mut args = Args::parse_from(&argv);
    config_apply::load_and_apply(&mut args, &argv);
    if let Err(e) = run_tui(args).await {
        eprintln!("grain-tui exited with error: {e}");
        std::process::exit(1);
    }
}
