//! Slash commands for the interactive loop.
//!
//! Pure data + pure parser; the CLI driver in [`crate::cli`] decides what to
//! do with each parsed command. Keeping the dispatch table here makes it
//! easy to unit-test without spinning up an Agent.

use std::fmt;

/// Parsed slash command. Anything that doesn't match a known prefix is
/// returned as [`SlashCommand::Unknown`] so the interactive loop can print
/// a helpful error instead of silently sending the line to the LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Exit the interactive loop.
    Exit,
    /// Print built-in help.
    Help,
    /// Reset the in-memory transcript (session file is untouched).
    Clear,
    /// Print the registered skills + their descriptions.
    Skills,
    /// Run an inline workspace diagnostic.
    Doctor,
    /// Show the workspace's git source info (branch, dirty status).
    Source,
    /// Compaction placeholder — implemented in a follow-up.
    Compact,
    /// Unrecognized — preserves the raw input for diagnostic output.
    Unknown(String),
}

impl fmt::Display for SlashCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlashCommand::Exit => write!(f, "/exit"),
            SlashCommand::Help => write!(f, "/help"),
            SlashCommand::Clear => write!(f, "/clear"),
            SlashCommand::Skills => write!(f, "/skills"),
            SlashCommand::Doctor => write!(f, "/doctor"),
            SlashCommand::Source => write!(f, "/source"),
            SlashCommand::Compact => write!(f, "/compact"),
            SlashCommand::Unknown(raw) => write!(f, "{raw}"),
        }
    }
}

/// Try to parse a line as a slash command. Returns `None` when the line
/// doesn't start with `/` so the caller can route it to the LLM instead.
pub fn parse(line: &str) -> Option<SlashCommand> {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    // Allow `/cmd  extra args` — currently no command takes args, but
    // future commands like `/save <path>` can extend this parser.
    let head = trimmed.split_whitespace().next().unwrap_or("/");
    let cmd = match head {
        "/exit" | "/quit" | "/q" => SlashCommand::Exit,
        "/help" | "/?" => SlashCommand::Help,
        "/clear" | "/reset" => SlashCommand::Clear,
        "/skills" => SlashCommand::Skills,
        "/doctor" => SlashCommand::Doctor,
        "/source" | "/git" => SlashCommand::Source,
        "/compact" => SlashCommand::Compact,
        other => SlashCommand::Unknown(other.to_string()),
    };
    Some(cmd)
}

/// Text rendered by `/help` — kept here so it stays in sync with the parser.
pub const HELP_TEXT: &str = "\
Built-in slash commands:
  /help                Show this help text
  /clear  /reset       Reset the in-memory transcript (session file untouched)
  /skills              List loaded skills
  /doctor              Run a workspace + provider diagnostic
  /source  /git        Show workspace git source info
  /compact             (placeholder, no-op until context compaction lands)
  /exit  /quit  /q     Leave the interactive loop

Anything not starting with `/` is sent to the LLM as your next prompt.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_slash_returns_none() {
        assert!(parse("hello world").is_none());
        assert!(parse("    just text").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn parses_exit_aliases() {
        assert_eq!(parse("/exit"), Some(SlashCommand::Exit));
        assert_eq!(parse("/quit"), Some(SlashCommand::Exit));
        assert_eq!(parse("/q"), Some(SlashCommand::Exit));
    }

    #[test]
    fn parses_help_and_clear() {
        assert_eq!(parse("/help"), Some(SlashCommand::Help));
        assert_eq!(parse("/?"), Some(SlashCommand::Help));
        assert_eq!(parse("/clear"), Some(SlashCommand::Clear));
        assert_eq!(parse("/reset"), Some(SlashCommand::Clear));
    }

    #[test]
    fn parses_diagnostic_commands() {
        assert_eq!(parse("/doctor"), Some(SlashCommand::Doctor));
        assert_eq!(parse("/source"), Some(SlashCommand::Source));
        assert_eq!(parse("/git"), Some(SlashCommand::Source));
        assert_eq!(parse("/skills"), Some(SlashCommand::Skills));
        assert_eq!(parse("/compact"), Some(SlashCommand::Compact));
    }

    #[test]
    fn unknown_slash_preserves_original_token() {
        match parse("/bogus") {
            Some(SlashCommand::Unknown(s)) => assert_eq!(s, "/bogus"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn extra_args_ignored_for_now() {
        // `/skills verbose` still parses as Skills; the arg is silently dropped.
        assert_eq!(parse("/skills verbose"), Some(SlashCommand::Skills));
    }

    #[test]
    fn surrounding_whitespace_tolerated() {
        assert_eq!(parse("   /help   "), Some(SlashCommand::Help));
    }
}
