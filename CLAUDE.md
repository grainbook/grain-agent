# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

Rust port of `@earendil-works/pi-agent-core` (TypeScript reference at <https://github.com/earendil-works/pi>, package path `packages/agent`). Files in this repo intentionally mirror TS modules one-to-one — keep that mapping when adding code. Each Rust module's top-of-file docs name the TS file it ports.

Cargo workspace, edition 2024, `resolver = "3"`. All shared deps go through `[workspace.dependencies]` in the root `Cargo.toml`; member crates reference them via `workspace = true`.

## Build / test / lint

```
cargo build              # whole workspace
cargo test               # whole workspace (integration tests under tests/smoke.rs in each crate)
cargo test -p grain-agent-core <name>     # single test by name in core
cargo test -p grain-agent-harness <name>  # single test in harness
cargo clippy --workspace --all-targets    # lint
```

The async tests use `#[tokio::test]`; `tokio` is declared with `features = ["full", "test-util"]` only under `[dev-dependencies]`.

## Crate layout and boundary

- `grain-agent-core` — provider-agnostic agent runtime. **Must not depend on any concrete LLM SDK.** New provider integrations belong in a separate crate that implements `LlmStream`.
- `grain-agent-harness` — engineering plumbing on top of core: session tree + storage, harness-aware `convert_to_llm`, system-prompt skill block, output truncation. Depends on `grain-agent-core`.

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

`SessionStorage` is the async trait that backs `Session`; `InMemorySessionStorage` is the only impl today. JSONL persistence is planned to live under the same trait — when adding it, do not change the trait shape without checking the harness `lib.rs` plan first.

`messages.rs` defines the three harness-aware custom-message variants (`branchSummary`, `compactionSummary`, `custom`) and the harness `convert_to_llm`. New custom variants should follow the same pattern: a typed constructor that serializes into `AgentMessage::Custom`, plus a branch in `convert_custom`.

`system_prompt.rs::format_skills_for_system_prompt` renders the `<available_skills>` XML block; `disable_model_invocation = true` filters a skill out of the rendered block but keeps it in the in-memory list.

`truncate.rs` is byte-counted on UTF-8 (no surrogate handling needed unlike the TS source). Defaults: `DEFAULT_MAX_LINES = 2000`, `DEFAULT_MAX_BYTES = 50 KiB`, `GREP_MAX_LINE_LENGTH = 500`.

## Conventions worth keeping

- Serde wire format matches the TS reference: `#[serde(rename_all = "camelCase")]` on structs, `#[serde(tag = "type", rename_all = "camelCase")]` (or `"snake_case"` for events) on enums. Don't change casing without checking the TS counterpart.
- IDs are UUIDv7 (`uuid` crate with `features = ["v7"]`) so they sort by creation time.
- The custom RFC3339 formatter and `days_from_civil` in `session.rs` exist so we don't pull `chrono`. Keep that constraint unless you have a strong reason.
- Hooks (`BeforeToolCallFn`, `AfterToolCallFn`, etc.) are `Arc<dyn Fn(...) -> BoxFuture<'static, ...> + Send + Sync>` aliases. Match the existing alias rather than inventing a new closure shape.
- The loop logs/emits via an injected `EventSink` (`Arc<dyn Fn(AgentEvent) -> BoxFuture + Send + Sync>`). All emissions inside the loop go through `emit_now`; preserve emission ordering when refactoring (the smoke test asserts ordering).
