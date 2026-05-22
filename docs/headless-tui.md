# `grain-ai-agent-tui`

A ratatui-based terminal UI on top of [`grain-ai-agent-headless`](./headless-cli.md). Same coding-agent capabilities (read / write / bash / web tools, sessions, skills, slash commands) wrapped in a multi-pane terminal interface.

中文版：[zh/headless-tui.md](./zh/headless-tui.md).

---

## Install + run

```bash
cargo build --release -p grain-ai-agent-tui --bin grain-tui
export DEEPSEEK_API_KEY=...
./target/release/grain-tui -C ./my-project
```

`-C` sets the workspace root (file tools refuse paths outside it). Default model is `deepseek/deepseek-chat`; change with `--model anthropic/claude-sonnet-4-5` or via `--provider <name>` ([providers.md](./providers.md)).

---

## Layout

Borderless, four-row layout that re-flows when the slash-command palette is open:

```
HEADER         grain-tui  model · workspace · [caps] · theme:default
TRANSCRIPT     scrollable chat history
[PALETTE]      ← only shown while the input starts with '/'
PROMPT         › type here
FOOTER         shortcuts hint
```

Overlays (help, doctor, skills, theme picker, provider picker, log, session resume) render as fixed-size centered cards on top of the layout, with a distinct background color from the theme's `surface` palette slot.

**Mouse:** the scroll wheel scrolls the transcript (3 rows per click, same follow-bottom / catch-up-to-tail semantics as PgUp/PgDn). Mouse capture is **on** to make the wheel work, which means terminals also intercept left-click drag for selection — hold **Option** (macOS) or **Shift** (most Linux terms) to bypass capture and use the native drag-to-select / right-click-copy. Paste still works as usual (`⌘V` / `Ctrl-Shift-V`).

**Live status:** while the agent is running, the footer renders `✻ Marinating… (Xm Ys · ↑input · ↓output tokens · cache N%)` followed by a colored cost chip (`$0.012`). Rotating verb every 5s, wall-clock elapsed, cumulative LLM token usage for the current run, prefix-cache hit rate (`cache_read / input_total`), and live USD cost computed from the active model's pricing table in `models.dev`. Cost chip color: green <$0.05 / yellow $0.05–0.20 / red ≥$0.20. Disappears when idle. Pricing data is whatever `models.dev` reports — the chip is suppressed for any model with no pricing recorded.

---

## Keys

| Key | Behavior |
|-----|----------|
| **Enter** | Submit prompt (or, when the slash palette is open and a row is highlighted: skill → inject body into input for review; command → snap input to that command and submit) |
| **Esc** | Close overlay → else clear input → else quit (three-level precedence) |
| **Tab** | Complete the highlighted built-in slash command (palette open); no-op for skill rows and otherwise (used to toggle focus and silently dropped chars) |
| **Ctrl-C** | Abort current turn while streaming; quit when idle |
| **↑ / ↓** | Slash-palette nav (when open) · prompt-history nav otherwise |
| **PgUp / PgDn** | Scroll transcript (always available from input focus) |
| **Home / End** | Cursor home / end of input |
| **F1 / F2 / F3** | Help · doctor · skills overlays |

---

## Slash commands

Type `/` to open the autocomplete dropdown above the input. Narrows live as you type; ↑↓ navigates; Enter submits the highlighted command.

| Command | Action |
|---------|--------|
| `/help`, `/?` | Key map + slash reference (overlay) |
| `/clear`, `/reset` | Clear the transcript |
| `/doctor` | Diagnostic report with **inline search** and scroll |
| `/skills` | List discovered skills |
| `/theme` | Open the theme picker |
| `/provider` | Open the provider profile picker (see [providers.md](./providers.md)) |
| `/log` | Show recent request-body capture (needs `--debug-log`) |
| `/resume` | Open the session-resume picker (past transcripts from `--sessions-dir`) |
| `/exit`, `/quit`, `/q` | Quit |

### `/resume` picker

Opens a centered overlay listing past sessions from `--sessions-dir` (default: `<workspace>/.grain/sessions/`). Each row shows the first user prompt (title), model, message count, and mtime. ↑↓ navigates; Enter prints a relaunch hint to the transcript:

```
(to resume: relaunch with `grain-tui --session /path/to/<uuid>.jsonl` — in-place /resume coming in Phase 4)
```

Esc closes the picker. The session list is sorted newest-first; files that fail to parse are skipped with a `[warn]` line.

### `/` skill palette

Typing `/` also shows **loaded skills** from `.claude/skills/` alongside built-in slash commands. Each skill appears as `skill: <name>` with its description.

- **Enter on a skill** → injects the skill's full body content into the input for review before submitting to the LLM. This replaces the current input entirely.
- **Enter on a command** → dispatches the command (existing behavior).
- **Tab** completes only built-in commands, not skill names.
- Skills with `disable_model_invocation: true` are excluded from the palette.

### `/doctor` search

Once the overlay is open, just type — your keystrokes filter the report (case-insensitive substring, plus section headers always shown for orientation). PgUp/PgDn/Home/End scroll. Backspace narrows; Esc closes.

Useful filters: `ANTHROPIC` / `OPENAI` / `DEEPSEEK` to find a specific env-key row; `branch` / `commit` to jump to the git block.

---

## Themes

Nine built-in presets inspired by [ratatui-themes](https://github.com/ricardodantas/ratatui-themes):

`default`, `dracula`, `nord`, `gruvbox-dark`, `gruvbox-light`, `tokyo-night`, `catppuccin-mocha`, `solarized-dark`, `one-dark-pro`.

Open `/theme` and pick with ↑↓ + Enter. The picker shows a 6-color swatch per row so you can see palettes before applying.

### Custom themes

Drop a `<name>.toml` file under `<workspace>/.grain/themes/` (or `--themes-dir <path>`):

```toml
name = "vaporwave"
[palette]
accent = "#ff71ce"
secondary = "#01cdfe"
fg = "#ffffff"
muted = "#7e7e7e"
error = "#ff6e6e"
warning = "#fff85e"
success = "#05ffa1"
info = "#b967ff"
surface = "#1a0033"     # optional — background of overlay cards
```

`surface` is optional; when omitted it falls back to `muted`. Malformed files print a `[warn]` and are skipped — they don't block startup.

Set the initial theme with `--theme <name>` (default: `default`).

---

## Providers

`/provider` opens the profile picker. Profiles let you switch between vendor accounts / subscription paths at runtime without restarting — see [providers.md](./providers.md) for the TOML schema and search order.

Boot with a specific profile via `--provider <name>` and override the file path via `--providers-file <path>`.

---

## CLI flags

| Flag | Default | Meaning |
|------|---------|---------|
| `-C, --workspace <DIR>` | `.` | Workspace root |
| `-m, --model <ID>` | `deepseek/deepseek-chat` | Model id from the embedded registry |
| `--system-prompt-file <PATH>` | (built-in) | Replace the default coding-agent system prompt |
| `--headroom-tokens <N>` | `4096` | Context-guard headroom |
| `--openai-compat <PRESET>` | `common` | `none` / `common` |
| `--show-thinking` | off | Render thinking deltas inline |
| `--allow-write` | off | Enable Write / Edit tools |
| `--allow-bash` | off | Enable the Bash tool (explicit opt-in) |
| `--allow-web` | off | Enable WebFetch (explicit opt-in) |
| `--allow-semantic-search` | off | Requires `--features rig` on headless |
| `--session <FILE>` | none | JSONL session: prior messages load on start, new ones append. Overrides `--sessions-dir` auto-create. |
| `--sessions-dir <DIR>` | `<workspace>/.grain/sessions` | When `--session` isn't passed, auto-creates a fresh `<uuidv7>.jsonl` here so every run is recoverable via `/resume`. |
| `--skills-dir <DIR>` | `<workspace>/.claude/skills` | Where to scan for `<name>/SKILL.md` |
| `--telemetry-file <FILE>` | none | One JSON-serialized `AgentEvent` per line |
| `--tick-ms <MS>` | `100` | Render tick interval |
| `--theme <NAME>` | `default` | Initial theme |
| `--themes-dir <DIR>` | `<workspace>/.grain/themes` | User theme TOMLs |
| `--provider <NAME>` | none | Initial provider profile ([providers.md](./providers.md)) |
| `--providers-file <FILE>` | (search workspace + user) | Override providers.toml path |
| `--scripts-dir <DIR>` | `<workspace>/.grain/scripts` | JS scripting via [scripting.md](./scripting.md) — needs `--features scripts-boa` |

---

## Architecture

- **`AppState`** (`src/app.rs`) — pure UI state machine; every key event maps to zero-or-more `Command`s and zero-or-more state mutations. Unit-tested without ratatui or tokio.
- **`TuiEvent`** (`src/event.rs`) — single envelope for key presses, ticks, resizes, `AgentEvent`s from the worker, and worker replies (`OverlayDoctor`, `OverlaySkills`, `ProviderApplied`, etc.).
- **`agent_worker`** (`src/agent_worker.rs`) — dedicated tokio task that owns the `Agent`. Bridges `Command` ↔ `TuiEvent` over `mpsc` channels. Handles runtime provider switches via `agent.set_model(...)` without restarting. Also manages session auto-create via [`session_discovery::new_session_path`](./headless-session-discovery.md) and the `/resume` picker via [`session_discovery::list_sessions`](./headless-session-discovery.md).
- **`ui`** (`src/ui.rs`) — pure render functions over `&AppState`.
- **`run`** (`src/run.rs`) — terminal lifecycle (raw mode + alt screen), event polling, render loop.

The UI thread never touches the `Agent` directly — every interaction goes through the channels.
