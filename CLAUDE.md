# CLAUDE.md

## Language

All answers **must** be in **Chinese**.

## What this repo is

Rust port of `@earendil-works/pi-agent-core` (TypeScript reference at <https://github.com/earendil-works/pi>, package path `packages/agent`). Files in this repo intentionally mirror TS modules one-to-one — keep that mapping when adding code. Each Rust module's top-of-file docs name the TS file it ports.

Cargo workspace, edition 2024, `resolver = "3"`. All shared deps go through `[workspace.dependencies]` in the root `Cargo.toml`; member crates reference them via `workspace = true`.

## Build / test / lint

```
cargo build              # whole workspace
cargo test               # whole workspace (integration tests under tests/smoke.rs in each crate)
cargo test -p grain-agent-core <name>     # single test by name in core
cargo test -p grain-agent-harness <name>  # single test in harness
cargo test -p grain-ai-agent-tui <name>   # single test in the TUI crate
cargo clippy --workspace --all-targets    # lint
```

Runnable binaries:

- `cargo run -p grain-ai-agent-tui --bin grain-tui` — interactive TUI (ratatui).
- `cargo run -p grain-ai-agent-headless --bin grain-headless` — one-shot CLI driver.
- `cargo run -p grain-llm-models --bin refresh-models --features fetch` — pull a fresh models.dev snapshot into `data/models-dev.json`.

The async tests use `#[tokio::test]`; `tokio` is declared with `features = ["full", "test-util"]` only under `[dev-dependencies]`.

## Crate layout and boundary

- `grain-agent-core` — provider-agnostic agent runtime. **Must not depend on any concrete LLM SDK.** New provider integrations belong in a separate crate that implements `LlmStream`.
- `grain-agent-harness` — engineering plumbing on top of core: session tree + storage, harness-aware `convert_to_llm`, system-prompt skill block, output truncation, `context_guard`. Depends on `grain-agent-core` and `grain-llm-models`.
- `grain-llm-models` — standardized model registry (descriptor / `Registry` / vendored models.dev snapshot). Pure data — no LLM SDK dependency. Optional `fetch` feature pulls live data from `models.dev/api.json`; the `refresh-models` bin writes a deterministic snapshot back to `data/models-dev.json`.
- `grain-llm-genai` — `LlmStream` implementation backed by `genai 0.5`. Builder configures env-key resolver, OpenAI-compat preset (kimi / siliconflow), provider router (grain id → genai namespace). Depends on `grain-agent-core` and `grain-llm-models`.
- `grain-ai-agent-headless` — application-level toolkit on top of core + harness + genai: workspace resolution, config / profiles, JSONL session persistence (`SessionWriter`, `load_messages`, `is_session_locked`, `list_sessions`), skills + plugins discovery, slash commands, DeepSeek scavenge pack, the four read-only coding tools, and the `grain-headless` CLI bin.
- `grain-ai-agent-tui` — ratatui TUI app that consumes `grain-ai-agent-headless`. Worker / UI split (see "TUI architecture"). Ships the `grain-tui` bin.
- `grain-deepseek-pack` — DeepSeek-specific behavior pack (reasoning scavenge + `subagent.done` detection) wired in by `grain-ai-agent-headless` when its model id resolves to a DeepSeek model.
- `grain-pi-compat` — compatibility shims for the upstream `pi` (TS reference) wire format.
- `grain-script-boa` / `grain-script-rhai` — optional scripting layers; gated by features `scripts-boa` / `scripts-rhai` on headless / TUI.
- `grain-plugin-wasm` — optional WebAssembly Component Model plugin runtime (feature `wasm-plugins`).
- `lazy-gagent` — placeholder crate for a future Neovim/lazy.nvim-style plugin manager. The plugin **engine** (manifest types, discovery, integration helpers) lives in `grain-ai-agent-headless::plugins`; this crate only re-exports those types today and will gain install/update/remove UX in Phase C. TUI does **not** depend on `lazy-gagent` — it consumes the engine via headless. See [docs/plugins.md](docs/plugins.md).

The harness `lib.rs` lists what is deliberately **not** ported yet (compaction, on-disk skills loading, execution environment, top-level `AgentHarness`, JSONL session storage). Before adding new harness surface, check that file — those modules are the next-slice work and may already have a planned shape on the TS side.

## Core architecture (read before changing the loop)

The loop separates the agent transcript from what the LLM sees:

- `AgentMessage` is the transcript type: either `Standard(Message)` (user / assistant / tool result) or `Custom(serde_json::Value)` (opaque app payload). Custom payloads carry a `role` discriminator inside the JSON; they are stashed in the transcript and only converted to LLM messages at the boundary.
- `ConvertToLlmFn` runs at every turn to project `AgentMessage[]` → `Message[]`. `Agent`'s default drops custom messages; `grain_agent_harness::convert_to_llm` knows how to project `branchSummary`, `compactionSummary`, and `custom` into wrapped user messages.
- `TransformContextFn` runs **before** `ConvertToLlmFn` and rewrites the `AgentMessage[]` snapshot (use this for compaction / context surgery rather than mutating state).

`LlmStream` is the only LLM provider seam. Contract (enforced by the loop, mirror in any new impl):

- `stream()` must not return `Err` for request/model failures. Surface failures as a **terminal** `AssistantMessageEvent::Error` (or `Done`) on the returned stream, with `StopReason::Error` / `StopReason::Aborted` and a populated `error_message` on the final `AssistantMessage`.
- The stream MUST end with exactly one terminal event. The loop synthesizes a placeholder error if it doesn't.

Two entry points in `agent_loop.rs`:

- `run_agent_loop(prompts, ...)` — start with new user messages.
- `run_agent_loop_continue(context, ...)` — continue an existing transcript; the last message must convert to a `user` / `toolResult` LLM message (assistant tails are rejected).

Turn lifecycle (`run_loop`): drain steering queue → stream assistant turn → execute tool calls → emit `TurnEnd` → run `prepare_next_turn` hook (may swap context/model/thinking) → run `should_stop_after_turn` → loop. When the assistant produces no more tool calls and steering is empty, drain the follow-up queue once more; if that's also empty, emit `AgentEnd`.

Tool execution: `ToolExecutionMode::Parallel` is the default, but the loop falls back to sequential if **any** invoked tool's `definition().execution_mode` is `Sequential`. Parallel mode preserves source order in the resulting `ToolResultMessage[]` via `FuturesOrdered`. A batch where every finalized call has `result.terminate == Some(true)` ends the loop (`should_terminate`).

`Agent` (high-level wrapper) owns mutable state behind a `tokio::sync::Mutex<Inner>` and adds:

- `prompt` / `prompt_text` / `continue_` with single-active-run enforcement (`AgentError::AlreadyRunning`).
- `steer(msg)` / `follow_up(msg)` queues, each with `QueueMode::All | OneAtATime`. Calling `prompt` while idle skips the **initial** steering poll (the `skip_initial_steering_poll` flag) so the queued steer doesn't get prepended to the new prompt.
- `subscribe` → `Unsubscribe`, `abort` via shared `CancellationToken`, `signal` returning the active token.
- On loop error, `finish_run` synthesizes the same three events the TS implementation emits on failure (`MessageStart` / `MessageEnd` / `TurnEnd` / `AgentEnd`) so subscribers always see a coherent terminal sequence.

## Harness architecture

`session.rs` models a per-session **tree** of `SessionTreeEntry { id, parent_id, ... }` with a `leaf_id` cursor that marks the tip of the active branch. `get_path_to_root(leaf_id)` walks parent links to materialize a branch in chronological order. `SessionTreeEntryKind` covers `Message`, `ThinkingLevelChange`, `ModelChange`, `Compaction`, `Custom`, `CustomMessage`, `Label`, `SessionInfo`, `BranchSummary`. Forking (`SessionRepo::fork`) and moving the leaf (`Session::move_to`) are how branches are created/switched.

`build_session_context(path_entries)` is the canonical reduction from a branch into the `SessionContext` (`messages`, `thinking_level`, `model`) the agent run consumes. Key behavior: if a `Compaction` entry exists, only entries from `first_kept_entry_id` onward are kept and the compaction summary message is prepended.

`SessionStorage` is the async trait that backs `Session`; `InMemorySessionStorage` is the only in-harness impl. JSONL persistence today lives one layer up in `grain-ai-agent-headless::session` (`SessionWriter` / `load_messages` — see "Session persistence" below); a harness-native `SessionStorage` impl backed by the same files is still planned. When adding it, do not change the trait shape without checking the harness `lib.rs` plan first.

`messages.rs` defines the three harness-aware custom-message variants (`branchSummary`, `compactionSummary`, `custom`) and the harness `convert_to_llm`. New custom variants should follow the same pattern: a typed constructor that serializes into `AgentMessage::Custom`, plus a branch in `convert_custom`.

`system_prompt.rs::format_skills_for_system_prompt` renders the `<available_skills>` XML block; `disable_model_invocation = true` filters a skill out of the rendered block but keeps it in the in-memory list.

`truncate.rs` is byte-counted on UTF-8 (no surrogate handling needed unlike the TS source). Defaults: `DEFAULT_MAX_LINES = 2000`, `DEFAULT_MAX_BYTES = 50 KiB`, `GREP_MAX_LINE_LENGTH = 500`.

## Session persistence (headless)

JSONL persistence lives in `grain-ai-agent-headless::session`, not in the harness. One file per session under `<workspace>/.grain/sessions/<uuidv7>.jsonl`; one `AgentMessage` per line. The contract:

- `SessionWriter::open(path)` opens with `create + append` **and** takes an `fs2::FileExt::try_lock_exclusive` advisory lock on the fd. A second open from another process returns `SessionError::Locked { path }`; the lock auto-releases when the inner `File` drops (process exit, including crash). This is what makes "two TUIs on the same session" safe.
- `is_session_locked(path)` is a non-destructive probe used by `list_sessions` to fill `SessionMeta::locked` so the `/resume` picker can render `[locked]` rows. The probe opens read-only and tries+releases; do not use it to gate writes (TOCTOU), always trust `SessionWriter::open`'s `Locked` result.
- The TUI handles lock conflicts at two points: boot auto-resume (worker silently mints a fresh path and emits `TuiEvent::SessionLockedAtBoot` so the UI can offer fresh / fork / quit) and `/resume` Enter on a locked row (opens the same dialog with fresh / fork / cancel). Both flows route the "fork" choice through `Command::ForkSession(src)`, which copies the locked jsonl to a new uuidv7 and resumes the copy.
- `load_messages(path)` reads the whole file; malformed lines are skipped with a `[warn]` and don't abort. Missing file → `Ok(vec![])` (so callers can treat the path as "create on first save").

When adding new persisted-state surfaces (compaction, branch fork records, etc.), keep them inside the same jsonl rather than introducing a sidecar — the lock only covers the jsonl, and sidecars would need their own coordination story.

## TUI architecture

`grain-ai-agent-tui` separates the agent worker from the UI render loop. They communicate over two `mpsc::unbounded` channels and never share state:

- **Worker** (`agent_worker.rs::run_command_loop`): owns the `Arc<AgentHarness>` + `SessionWriter` + telemetry. Processes `Command` (sent by UI on key events / slash commands), drives `harness.prompt_text(...)`, and pushes `TuiEvent` back. Holds `active_session_path: Option<PathBuf>` to refuse `DeleteSession` on the live writer and to swap correctly on `/clear` (`Reset`) and `/resume` / `Fork`.
- **UI** (`app.rs::AppState`): pure state machine. Every input event (`TuiEvent::Key`, `Tick`, `Resize`, all `Agent(...)` forwards, etc.) goes through `on_event`, which returns a `Vec<Command>` for the worker. Rendering (`ui.rs::draw`) is a function of `AppState` only.
- **Overlays** (`Overlay` enum in `app.rs`) gate input: the key dispatcher in `on_key` checks `self.overlay` first and routes to an `on_key_*` handler per variant. Esc closes the active overlay before falling through to global handlers. New overlays must register in three places: the variant itself, the size+draw match in `ui.rs::draw_overlay`, and a `matches!` branch in `on_key`.
- **Boot** is single-pass: `init_worker` resolves the session path, takes the lock (with the fresh-fallback above), builds the harness with `prior_messages`, spawns the worker task, and only then does the UI loop start. Lock-conflict UX is deferred: the worker emits `SessionLockedAtBoot` after subscriptions install, and the UI overlay paints on the first frame.

## Conventions worth keeping

- Serde wire format matches the TS reference: `#[serde(rename_all = "camelCase")]` on structs, `#[serde(tag = "type", rename_all = "camelCase")]` (or `"snake_case"` for events) on enums. Don't change casing without checking the TS counterpart.
- IDs are UUIDv7 (`uuid` crate with `features = ["v7"]`) so they sort by creation time.
- The custom RFC3339 formatter and `days_from_civil` in `session.rs` exist so we don't pull `chrono`. Keep that constraint unless you have a strong reason.
- Hooks (`BeforeToolCallFn`, `AfterToolCallFn`, etc.) are `Arc<dyn Fn(...) -> BoxFuture<'static, ...> + Send + Sync>` aliases. Match the existing alias rather than inventing a new closure shape.
- The loop logs/emits via an injected `EventSink` (`Arc<dyn Fn(AgentEvent) -> BoxFuture + Send + Sync>`). All emissions inside the loop go through `emit_now`; preserve emission ordering when refactoring (the smoke test asserts ordering).
