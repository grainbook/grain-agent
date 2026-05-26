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
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::Alignment,
    widgets::{Block, Borders, Paragraph},
};
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
    // Load persisted preferences early so they can influence the
    // initial provider / model selection before spawning the worker.
    let persist_path = persist::default_path(&args.workspace);
    let persisted = PersistedState::load(&persist_path);

    // Resolve provider profiles before spawning the worker — the worker
    // needs them to register OpenAI-compat endpoints up front and to
    // honor `--provider <name>` at boot.
    //
    // Fallback chain for initial provider:
    //   1. `--provider <name>` CLI flag (explicit user intent)
    //   2. `persisted.last_provider` (previous session's choice)
    //   3. None (use `--model` verbatim)
    let requested_provider = args
        .provider
        .as_deref()
        .or(persisted.last_provider.as_deref());
    let (profiles, initial_profile_idx) = resolve_profiles(
        args.providers_file.as_deref(),
        &args.workspace,
        requested_provider,
    );

    let mut cfg = WorkerConfig::from(&args);
    cfg.profiles = profiles.clone();
    cfg.initial_profile_idx = initial_profile_idx;

    // Fallback chain for initial model:
    //   1. `--model <id>` CLI flag (explicit user intent)
    //   2. `persisted.last_model` (previous session's choice)
    //   3. Keep the WorkerConfig default (deepseek/deepseek-chat)
    if cfg.model == "deepseek/deepseek-chat"
        && let Some(ref m) = persisted.last_model
    {
        cfg.model = m.clone();
    }

    // Resolve themes before grabbing the terminal so any disk-load
    // warnings get a chance to print to stderr before the alt screen
    // hides them.
    let themes_dir = args
        .themes_dir
        .clone()
        .unwrap_or_else(|| args.workspace.join(".grain").join("themes"));
    // Theme fallback chain:
    //   1. `--theme <name>` CLI flag (explicit user intent)
    //   2. `ctx.persisted.last_theme` (previous session's choice)
    //   3. "default"
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
    let plugin_theme_dirs: Vec<std::path::PathBuf> =
        grain_ai_agent_headless::discover_plugins(&plugins_dir)
            .into_iter()
            .filter_map(|p| p.themes_dir())
            .collect();
    let (themes, initial_idx) = resolve_themes(&themes_dir, &plugin_theme_dirs, &requested_theme);

    install_panic_hook();
    let mut terminal = init_terminal()?;
    draw_startup_screen(&mut terminal, &args.workspace)?;

    let worker = match spawn(cfg).await {
        Ok(worker) => worker,
        Err(e) => {
            restore_terminal(&mut terminal)?;
            return Err(e.into());
        }
    };
    let Worker {
        cmd_tx,
        mut evt_rx,
        handles,
        join: _,
    } = worker;

    let mut ctx = EventLoopCtx {
        terminal: &mut terminal,
        tick_ms: args.tick_ms,
        cmd_tx: &cmd_tx,
        persist_path,
        persisted,
    };
    let result = event_loop(
        &mut ctx,
        &args,
        &handles,
        &mut evt_rx,
        themes,
        initial_idx,
        profiles,
        initial_profile_idx,
    )
    .await;
    restore_terminal(&mut terminal)?;
    result
}

fn draw_startup_screen(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    workspace: &std::path::Path,
) -> io::Result<()> {
    let workspace = workspace.display().to_string();
    terminal.draw(|f| {
        let body = format!("Starting grain TUI\n{workspace}");
        let widget = Paragraph::new(body)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::NONE));
        f.render_widget(widget, f.area());
    })?;
    Ok(())
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

/// Restore the terminal from inside a panic handler **before** the
/// default panic printer runs, so the stack trace lands on a normal
/// (non-alt) screen with raw mode disabled. Without this the alt
/// screen swallows the trace and the user sees "process exits silently"
/// — which is exactly what was masking the markdown-render panics.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default(info);
    }));
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

/// Bundles the mutable terminal / config / channels carried across
/// iterations of the event loop. Extracted from the old 11-parameter
/// signature for readability.
struct EventLoopCtx<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<Stdout>>,
    tick_ms: u64,
    cmd_tx: &'a mpsc::UnboundedSender<crate::app::Command>,
    persist_path: std::path::PathBuf,
    persisted: PersistedState,
}

async fn event_loop(
    ctx: &mut EventLoopCtx<'_>,
    args: &Args,
    handles: &crate::agent_worker::WorkerHandles,
    evt_rx: &mut mpsc::UnboundedReceiver<TuiEvent>,
    themes: Vec<Theme>,
    initial_theme_idx: usize,
    providers: Vec<ProviderProfile>,
    initial_provider_idx: Option<usize>,
) -> Result<(), TuiError> {
    // Resolve CNY rate once at startup: explicit `--cny-rate` wins,
    // otherwise auto-detect from `$LANG`. Pure: see
    // `app::resolve_cny_rate` for the truth table.
    let lang_env = std::env::var("LANG").ok();
    let cny_rate = crate::app::resolve_cny_rate(args.cny_rate, lang_env.as_deref());
    let mut state = AppState::new(
        handles.model_id.clone(),
        handles.model_cost.clone(),
        handles.context_window,
        handles.preflight_context_tokens,
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
        ctx.persisted.prompt_history.clone(),
    );

    // Crossterm reads on a blocking thread; forward into a tokio channel
    // so we can `select!` with the worker.
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let term_tx_clone = term_tx.clone();
    std::thread::spawn(move || forward_terminal_events(term_tx_clone));

    let mut ticker = interval(Duration::from_millis(ctx.tick_ms.max(16)));

    // Track the last mouse-capture state we applied to the terminal so
    // we can re-issue Enable/Disable when the user toggles via F6.
    // `init_terminal` started capture ON, so we mirror that here.
    let mut mouse_capture_applied = true;

    // Tracks the theme idx that's currently reflected in
    // `ctx.persisted.last_theme`. After each `on_event`, divergence here
    // means the user just picked a new theme via the picker — save
    // the choice so the next launch resumes it.
    let mut last_persisted_theme_idx = initial_theme_idx;

    // Track wall-clock between frames so tachyonfx effects advance
    // proportionally to real time.
    let mut last_tick = std::time::Instant::now();

    // Initial draw.
    ctx.terminal.draw(|f| {
        ui::draw(f, &mut state, crate::anim::FxDuration::from_millis(0));
    })?;

    loop {
        let event = tokio::select! {
            biased;
            // Check terminal (user) events first — during streaming / thinking,
            // agent events can flood `evt_rx` at frame-cadence frequency.  If
            // those were polled first, a `biased` select would never yield to
            // `term_rx`, freezing scroll and mouse capture.
            ev = term_rx.recv() => match ev {
                Some(e) => e,
                None => return Ok(()),
            },
            ev = evt_rx.recv() => match ev {
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
            if ctx.cmd_tx.send(cmd).is_err() {
                // worker is gone — wind down gracefully.
                return Ok(());
            }
        }
        if let Some(ev) = leftover {
            let commands = state.on_event(ev);
            for cmd in commands {
                if ctx.cmd_tx.send(cmd).is_err() {
                    return Ok(());
                }
            }
        }
        if state.current_theme_idx != last_persisted_theme_idx {
            last_persisted_theme_idx = state.current_theme_idx;
            if let Some(theme) = state.themes.get(state.current_theme_idx) {
                ctx.persisted.last_theme = Some(theme.name.clone());
                if let Err(e) = ctx.persisted.save(&ctx.persist_path) {
                    eprintln!("[warn] tui-state save {}: {e}", ctx.persist_path.display());
                }
            }
        }
        // Persist provider / model changes so they survive restarts.
        // Only write when they actually changed to avoid churning
        // the file on every unrelated event.
        let current_provider = state
            .current_provider_idx
            .and_then(|i| state.providers.get(i))
            .map(|p| p.name.clone());
        if current_provider != ctx.persisted.last_provider {
            ctx.persisted.last_provider = current_provider;
            if let Err(e) = ctx.persisted.save(&ctx.persist_path) {
                eprintln!("[warn] tui-state save {}: {e}", ctx.persist_path.display());
            }
        }
        if ctx.persisted.last_model.as_deref() != Some(&state.model_id) {
            ctx.persisted.last_model = Some(state.model_id.clone());
            if let Err(e) = ctx.persisted.save(&ctx.persist_path) {
                eprintln!("[warn] tui-state save {}: {e}", ctx.persist_path.display());
            }
        }
        // Persist prompt history so Up/Down recall survives
        // restarts. Only write when it actually changed.
        if ctx.persisted.prompt_history != state.history {
            ctx.persisted.prompt_history = state.history.clone();
            if let Err(e) = ctx.persisted.save(&ctx.persist_path) {
                eprintln!("[warn] tui-state save {}: {e}", ctx.persist_path.display());
            }
        }
        if state.should_quit {
            return Ok(());
        }
        if state.mouse_capture_on != mouse_capture_applied {
            apply_mouse_capture(ctx.terminal, state.mouse_capture_on)?;
            mouse_capture_applied = state.mouse_capture_on;
        }
        let now = std::time::Instant::now();
        let fx_elapsed = crate::anim::FxDuration::from_millis((now - last_tick).as_millis() as u32);
        last_tick = now;
        ctx.terminal.draw(|f| ui::draw(f, &mut state, fx_elapsed))?;

        // When effects are animating or the agent is streaming, run
        // at ~60 fps so transitions stay smooth. Otherwise block on
        // the event channel as before (the ticker still fires at the
        // configured `tick_ms` interval for time-based UI updates).
        if state.effects.is_active() || state.streaming {
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
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
