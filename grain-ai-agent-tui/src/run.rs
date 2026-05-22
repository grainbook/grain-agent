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
        DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyEventKind, MouseEventKind,
        poll, read,
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
use crate::persist::{self, PersistedState};
use crate::theme::{Theme, builtin_themes, load_user_themes};
use crate::ui;
use grain_llm_genai::{ProviderProfile, load_profiles, resolve_providers_file};

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
    // Resolve provider profiles before spawning the worker — the worker
    // needs them to register OpenAI-compat endpoints up front and to
    // honor `--provider <name>` at boot.
    let (profiles, initial_profile_idx) = resolve_profiles(
        args.providers_file.as_deref(),
        &args.workspace,
        args.provider.as_deref(),
    );

    let mut cfg = WorkerConfig::from(&args);
    cfg.profiles = profiles.clone();
    cfg.initial_profile_idx = initial_profile_idx;

    let Worker {
        cmd_tx,
        mut evt_rx,
        handles,
        join: _,
    } = spawn(cfg).await?;

    // Resolve themes before grabbing the terminal so any disk-load
    // warnings get a chance to print to stderr before the alt screen
    // hides them.
    let themes_dir = args
        .themes_dir
        .clone()
        .unwrap_or_else(|| args.workspace.join(".grain").join("themes"));
    // Load persisted preferences (last theme). When the user passed
    // `--theme` explicitly (i.e. not the literal default `"default"`),
    // the CLI wins; otherwise we honor the persisted choice so the
    // theme survives restarts.
    let persist_path = persist::default_path(&args.workspace);
    let persisted = PersistedState::load(&persist_path);
    let requested_theme: String = if args.theme != "default" {
        args.theme.clone()
    } else {
        persisted
            .last_theme
            .clone()
            .unwrap_or_else(|| "default".into())
    };
    // Phase A plugin contribution to themes: each plugin can ship a
    // `themes/` subdirectory that gets folded into the catalog
    // alongside `--themes-dir`. Discovery here is duplicated from the
    // worker (cheap — one shallow read_dir) so theme resolution can
    // happen before the worker spawns and the alt-screen takes over.
    let plugins_dir = args
        .plugins_dir
        .clone()
        .unwrap_or_else(|| grain_ai_agent_headless::default_plugins_dir(&args.workspace));
    let plugin_theme_dirs: Vec<std::path::PathBuf> = grain_ai_agent_headless::discover_plugins(&plugins_dir)
        .into_iter()
        .filter_map(|p| p.themes_dir())
        .collect();
    let (themes, initial_idx) =
        resolve_themes(&themes_dir, &plugin_theme_dirs, &requested_theme);

    let mut terminal = init_terminal()?;
    let result = event_loop(
        &mut terminal,
        &args,
        &handles,
        &mut evt_rx,
        &cmd_tx,
        themes,
        initial_idx,
        profiles,
        initial_profile_idx,
        persist_path,
        persisted,
    )
    .await;
    restore_terminal(&mut terminal)?;
    result
}

/// Load profiles from:
/// 1. `config.toml`'s `[[provider]]` blocks (authoritative).
/// 2. The legacy `providers.toml` (workspace or user XDG), unioned
///    in by name. Config wins on collision.
///
/// Resolves `--provider <name>` to an index in the merged list.
/// Disk-load warnings go to stderr.
fn resolve_profiles(
    cli_override: Option<&std::path::Path>,
    workspace_root: &std::path::Path,
    requested: Option<&str>,
) -> (Vec<ProviderProfile>, Option<usize>) {
    let mut profiles: Vec<ProviderProfile> = Vec::new();

    // 1. config.toml `[[provider]]` — authoritative.
    if let Ok(cfg) = grain_ai_agent_headless::ConfigFile::load(workspace_root) {
        for entry in cfg.providers {
            match grain_llm_genai::profile_from_entry(entry) {
                Ok(p) => profiles.push(p),
                Err(e) => eprintln!("[warn] config.toml provider: {e}"),
            }
        }
    }

    // 2. Legacy providers.toml — fill in any names config didn't
    // already cover.
    let legacy_path = resolve_providers_file(cli_override, workspace_root);
    if let Some(p) = legacy_path {
        let (legacy_profiles, warnings) = load_profiles(&p);
        for w in warnings {
            eprintln!("[warn] {w}");
        }
        let mut migration_count = 0usize;
        for legacy in legacy_profiles {
            if profiles.iter().any(|e| e.name == legacy.name) {
                continue;
            }
            migration_count += 1;
            profiles.push(legacy);
        }
        if migration_count > 0 {
            eprintln!(
                "[warn] {migration_count} entries in legacy {}; consider migrating to config.toml [[provider]] blocks",
                p.display()
            );
        }
    }

    let initial_idx = match requested {
        None => None,
        Some(name) => match profiles.iter().position(|p| p.name == name) {
            Some(i) => Some(i),
            None => {
                eprintln!(
                    "[warn] provider '{name}' not found in config.toml or providers.toml \
                     ({} profiles loaded)",
                    profiles.len()
                );
                None
            }
        },
    };
    (profiles, initial_idx)
}

/// Merge built-ins with user themes (and Phase-A plugin themes) and
/// pick the starting index by name. Unknown name → fall back to
/// `default` (always index 0 in `builtin_themes()`). Disk warnings go
/// to stderr.
fn resolve_themes(
    themes_dir: &std::path::Path,
    extra_theme_dirs: &[std::path::PathBuf],
    requested: &str,
) -> (Vec<Theme>, usize) {
    let mut all = builtin_themes();
    let (user, warnings) = load_user_themes(themes_dir);
    for w in warnings {
        eprintln!("[warn] {w}");
    }
    all.extend(user);
    for d in extra_theme_dirs {
        let (extra, warnings) = load_user_themes(d);
        for w in warnings {
            eprintln!("[warn] {w}");
        }
        all.extend(extra);
    }
    let idx = all
        .iter()
        .position(|t| t.name == requested)
        .unwrap_or_else(|| {
            if !requested.is_empty() && requested != "default" {
                eprintln!("[warn] theme '{requested}' not found; falling back to 'default'");
            }
            0
        });
    (all, idx)
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture is ON so the scroll wheel produces real events.
    // Tradeoff: terminals capture left-click drag too, so native
    // text selection requires holding Option/Alt (most macOS terms)
    // or Shift (most Linux terms) to bypass capture. See
    // docs/headless-tui.md for the user-facing note.
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
    providers: Vec<ProviderProfile>,
    initial_provider_idx: Option<usize>,
    persist_path: std::path::PathBuf,
    mut persisted: PersistedState,
) -> Result<(), TuiError> {
    // Resolve CNY rate once at startup: explicit `--cny-rate` wins,
    // otherwise auto-detect from `$LANG`. Pure: see
    // `app::resolve_cny_rate` for the truth table.
    let lang_env = std::env::var("LANG").ok();
    let cny_rate = crate::app::resolve_cny_rate(args.cny_rate, lang_env.as_deref());
    let mut state = AppState::new(
        handles.model_id.clone(),
        handles.model_cost.clone(),
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
        providers,
        initial_provider_idx,
        cny_rate,
    );

    // Crossterm reads on a blocking thread; forward into a tokio channel
    // so we can `select!` with the worker.
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let term_tx_clone = term_tx.clone();
    std::thread::spawn(move || forward_terminal_events(term_tx_clone));

    let mut ticker = interval(Duration::from_millis(args.tick_ms.max(16)));

    // Track the last mouse-capture state we applied to the terminal so
    // we can re-issue Enable/Disable when the user toggles via F6.
    // `init_terminal` started capture ON, so we mirror that here.
    let mut mouse_capture_applied = true;

    // Tracks the theme idx that's currently reflected in
    // `persisted.last_theme`. After each `on_event`, divergence here
    // means the user just picked a new theme via the picker — save
    // the choice so the next launch resumes it.
    let mut last_persisted_theme_idx = initial_theme_idx;

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

        // Coalesce consecutive same-direction scroll events. Fast wheel
        // spins can stuff dozens of `ScrollUp` / `ScrollDown` events
        // into `term_rx` faster than we redraw; without coalescing the
        // transcript keeps scrolling for several frames after the
        // wheel physically stops, because each iteration consumes
        // exactly one notch and renders. Draining the contiguous run
        // of same-direction scrolls into one summed event makes the
        // scroll snap to a stop as soon as the user lifts their
        // finger. Any non-scroll event that gets pulled out during
        // the drain is stashed in `leftover` so we still dispatch it
        // (next, in the same iteration) and don't lose key presses.
        let mut leftover: Option<TuiEvent> = None;
        let event = match event {
            TuiEvent::ScrollUp { mut amount } => {
                loop {
                    match term_rx.try_recv() {
                        Ok(TuiEvent::ScrollUp { amount: a }) => {
                            amount = amount.saturating_add(a);
                        }
                        Ok(other) => {
                            leftover = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                TuiEvent::ScrollUp { amount }
            }
            TuiEvent::ScrollDown { mut amount } => {
                loop {
                    match term_rx.try_recv() {
                        Ok(TuiEvent::ScrollDown { amount: a }) => {
                            amount = amount.saturating_add(a);
                        }
                        Ok(other) => {
                            leftover = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                TuiEvent::ScrollDown { amount }
            }
            other => other,
        };

        let commands = state.on_event(event);
        for cmd in commands {
            if cmd_tx.send(cmd).is_err() {
                // worker is gone — wind down gracefully.
                return Ok(());
            }
        }
        if let Some(ev) = leftover {
            let commands = state.on_event(ev);
            for cmd in commands {
                if cmd_tx.send(cmd).is_err() {
                    return Ok(());
                }
            }
        }
        if state.current_theme_idx != last_persisted_theme_idx {
            last_persisted_theme_idx = state.current_theme_idx;
            if let Some(theme) = state.themes.get(state.current_theme_idx) {
                persisted.last_theme = Some(theme.name.clone());
                if let Err(e) = persisted.save(&persist_path) {
                    eprintln!("[warn] tui-state save {}: {e}", persist_path.display());
                }
            }
        }
        if state.should_quit {
            return Ok(());
        }
        if state.mouse_capture_on != mouse_capture_applied {
            apply_mouse_capture(terminal, state.mouse_capture_on)?;
            mouse_capture_applied = state.mouse_capture_on;
        }
        terminal.draw(|f| ui::draw(f, &state))?;
    }
}

/// Switch terminal mouse capture on or off without touching the rest
/// of the alt-screen / raw-mode setup.
fn apply_mouse_capture(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    enable: bool,
) -> io::Result<()> {
    if enable {
        execute!(terminal.backend_mut(), EnableMouseCapture)?;
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture)?;
    }
    Ok(())
}

/// Blocking loop on a dedicated OS thread: read crossterm events and
/// forward them through the mpsc channel. Exits when the receiver is
/// dropped (channel send fails).
fn forward_terminal_events(tx: mpsc::UnboundedSender<TuiEvent>) {
    // Step size for scroll-wheel events. Tuned for "scroll feels
    // natural on a notch wheel" — 3 rows per click matches most
    // editor / terminal conventions.
    const WHEEL_STEP: u16 = 3;
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
                Ok(CtEvent::Mouse(m)) => {
                    // Scroll wheel + left-button drag for in-app text
                    // selection. Other mouse activity (move-without-
                    // drag, right-click, middle-click) is ignored —
                    // those still pass through to the terminal when
                    // F6 disables capture entirely.
                    use crossterm::event::MouseButton;
                    let evt = match m.kind {
                        MouseEventKind::ScrollUp => Some(TuiEvent::ScrollUp { amount: WHEEL_STEP }),
                        MouseEventKind::ScrollDown => {
                            Some(TuiEvent::ScrollDown { amount: WHEEL_STEP })
                        }
                        MouseEventKind::Down(MouseButton::Left) => Some(TuiEvent::MouseDown {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Drag(MouseButton::Left) => Some(TuiEvent::MouseDrag {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Up(MouseButton::Left) => Some(TuiEvent::MouseUp),
                        _ => None,
                    };
                    if let Some(e) = evt
                        && tx.send(e).is_err()
                    {
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
