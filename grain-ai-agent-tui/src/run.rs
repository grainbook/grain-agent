//! Terminal lifecycle + event loop. Pulls everything together:
//! [`crate::agent_worker::spawn`] for the agent task, crossterm for raw
//! input, ratatui for rendering.
//!
//! The loop is a `tokio::select!` between three signals: a key/resize
//! event from crossterm (read on a blocking thread), a worker event
//! (`mpsc`), and a tick timer. After each event the renderer redraws.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyEventKind, poll, read,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::agent_worker::{Worker, WorkerConfig, WorkerInitError, spawn};
use crate::app::{AppState, Capabilities};
use crate::cli::Args;
use crate::event::TuiEvent;
use crate::theme::{Theme, builtin_themes, load_user_themes};
use crate::ui;

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("worker init: {0}")]
    WorkerInit(#[from] WorkerInitError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Entry point: takes parsed [`Args`], owns the terminal for the
/// duration of the session, and returns once the user quits.
pub async fn run_tui(args: Args) -> Result<(), TuiError> {
    let cfg = WorkerConfig::from(&args);
    let Worker {
        cmd_tx,
        mut evt_rx,
        handles,
        join: _,
    } = spawn(cfg)?;

    // Resolve themes before grabbing the terminal so any disk-load
    // warnings get a chance to print to stderr before the alt screen
    // hides them.
    let themes_dir = args.themes_dir.clone().unwrap_or_else(|| {
        args.workspace.join(".grain").join("themes")
    });
    let (themes, initial_idx) = resolve_themes(&themes_dir, &args.theme);

    let mut terminal = init_terminal()?;
    let result = event_loop(
        &mut terminal,
        &args,
        &handles,
        &mut evt_rx,
        &cmd_tx,
        themes,
        initial_idx,
    )
    .await;
    restore_terminal(&mut terminal)?;
    result
}

/// Merge built-ins with user themes and pick the starting index by
/// name. Unknown name → fall back to `default` (always index 0 in
/// `builtin_themes()`). Disk warnings go to stderr.
fn resolve_themes(themes_dir: &std::path::Path, requested: &str) -> (Vec<Theme>, usize) {
    let mut all = builtin_themes();
    let (user, warnings) = load_user_themes(themes_dir);
    for w in warnings {
        eprintln!("[warn] {w}");
    }
    all.extend(user);
    let idx = all
        .iter()
        .position(|t| t.name == requested)
        .unwrap_or_else(|| {
            if !requested.is_empty() && requested != "default" {
                eprintln!(
                    "[warn] theme '{requested}' not found; falling back to 'default'"
                );
            }
            0
        });
    (all, idx)
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    args: &Args,
    handles: &crate::agent_worker::WorkerHandles,
    evt_rx: &mut mpsc::UnboundedReceiver<TuiEvent>,
    cmd_tx: &mpsc::UnboundedSender<crate::app::Command>,
    themes: Vec<Theme>,
    initial_theme_idx: usize,
) -> Result<(), TuiError> {
    let mut state = AppState::new(
        handles.model_id.clone(),
        handles.workspace_display.clone(),
        Capabilities {
            allow_write: handles.allow_write,
            allow_bash: handles.allow_bash,
            allow_web: handles.allow_web,
            allow_semantic_search: handles.allow_semantic_search,
        },
        args.show_thinking,
        themes,
        initial_theme_idx,
    );

    // Crossterm reads on a blocking thread; forward into a tokio channel
    // so we can `select!` with the worker.
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let term_tx_clone = term_tx.clone();
    std::thread::spawn(move || forward_terminal_events(term_tx_clone));

    let mut ticker = interval(Duration::from_millis(args.tick_ms.max(16)));

    // Initial draw.
    terminal.draw(|f| ui::draw(f, &state))?;

    loop {
        let event = tokio::select! {
            biased;
            ev = evt_rx.recv() => match ev {
                Some(e) => e,
                None => return Ok(()),
            },
            ev = term_rx.recv() => match ev {
                Some(e) => e,
                None => return Ok(()),
            },
            _ = ticker.tick() => TuiEvent::Tick,
        };

        let commands = state.on_event(event);
        for cmd in commands {
            if cmd_tx.send(cmd).is_err() {
                // worker is gone — wind down gracefully.
                return Ok(());
            }
        }
        if state.should_quit {
            return Ok(());
        }
        terminal.draw(|f| ui::draw(f, &state))?;
    }
}

/// Blocking loop on a dedicated OS thread: read crossterm events and
/// forward them through the mpsc channel. Exits when the receiver is
/// dropped (channel send fails).
fn forward_terminal_events(tx: mpsc::UnboundedSender<TuiEvent>) {
    loop {
        // Long poll keeps CPU low; tokio side gets ticks anyway.
        match poll(Duration::from_millis(250)) {
            Ok(true) => match read() {
                Ok(CtEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                    if tx.send(TuiEvent::Key(k)).is_err() {
                        return;
                    }
                }
                Ok(CtEvent::Resize(w, h)) => {
                    if tx.send(TuiEvent::Resize(w, h)).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            },
            Ok(false) => {}
            Err(_) => return,
        }
    }
}
