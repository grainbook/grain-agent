# `AgentHarness`

Top-level orchestrator that bundles `Agent` + `Session` + tools + skills + queues + compaction + UI hooks into one façade. Lives in `grain-agent-harness` (no new crate). Port of pi's `AgentHarness` — see [agent-harness-design.md](./agent-harness-design.md) for the porting plan.

中文版：[zh/agent-harness.md](./zh/agent-harness.md).

---

## Why it exists

Before `AgentHarness`, every binary that drives an agent had to manually:

```rust
let mut opts = AgentOptions::new(model, stream);
opts.tools = build_tools(...);
opts.transform_context = Some(context_guard);
// ...
let agent = Agent::new(opts);
agent.subscribe(SessionWriter::open(path)?).await;     // mirror to disk
agent.subscribe(telemetry_sink).await;
agent.subscribe(event_printer).await;
agent.prompt_text("hello").await?;
```

That ceremony lives in `grain-headless::cli::run` and `grain-tui::agent_worker::spawn` today. `AgentHarness::new(...)` collapses it to one line + a typed event listener:

```rust
let harness = AgentHarness::new(opts).await;
harness.subscribe(my_listener).await;
harness.prompt_text("hello").await?;
```

It also **owns** the `Session`, mirrors every `MessageEnd` back to it automatically, and exposes pi-style operations (`navigate_tree`, `compact`, `append_entry`, `prompt_from_template`, `skill`) that the bare `Agent` doesn't.

---

## Quickstart

```rust
use grain_agent_harness::{AgentHarness, AgentHarnessOptions, Resources, SystemPrompt};
use grain_agent_harness::session::{InMemorySessionStorage, Session, SessionMetadata};
use grain_agent_core::Model;
use std::sync::Arc;

let session = Session::new(Arc::new(InMemorySessionStorage::new(SessionMetadata::new())));
let mut opts = AgentHarnessOptions::new(session, Model::unknown(), my_stream_fn());
opts.system_prompt = SystemPrompt::Static("You are a helpful coding agent.".into());
opts.tools = my_tools;
let harness = AgentHarness::new(opts).await;

harness.subscribe(Arc::new(|event, _signal| Box::pin(async move {
    println!("{:?}", event);
}))).await;

harness.prompt_text("Read main.rs and tell me what it does.").await?;
harness.wait_for_idle().await;
```

---

## Constructor

```rust
pub struct AgentHarnessOptions {
    pub session: Session,
    pub model: Model,
    pub stream_fn: StreamFn,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub resources: Resources,                 // skills + prompt templates
    pub system_prompt: SystemPrompt,          // Static(String) | Dynamic(closure)
    pub thinking_level: ThinkingLevel,
    pub active_tool_names: Option<Vec<String>>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub get_api_key: Option<GetApiKeyFn>,
    pub transform_context: Option<TransformContextFn>,
    pub tool_execution: ToolExecutionMode,
    pub session_id: Option<String>,
    pub transport: Option<String>,
    pub max_retry_delay_ms: Option<u64>,
}
```

`AgentHarnessOptions::new(session, model, stream_fn)` gives sane defaults; you set the rest. `AgentHarness::new(opts).await` seeds the agent transcript from `session.build_context()` and installs two internal listeners (session-mirror + harness event broadcaster).

---

## Public methods

### Turn triggers

| Method | Behavior |
|--------|----------|
| `prompt_text(text)` | Submit a fresh user prompt as a string. Fires `BeforeAgentStart`. |
| `prompt(Vec<AgentMessage>)` | Submit a batch of messages (e.g. user + attachments). |
| `continue_()` | Resume from the current transcript. |
| `prompt_from_template(name, args)` | Look up the named `PromptTemplate` in `Resources`, render with `args` JSON, submit. Errors `UnknownTemplate` if missing. |
| `skill(name, args)` | Synthesize a prompt like `"Use the <name> skill with arguments: <json>"` and submit. Phase 5+ will tighten to validated invocation. |

### Queues

| Method | Behavior |
|--------|----------|
| `steer(msg)` | Queue a steer message (delivered before the next assistant turn begins). |
| `follow_up(msg)` | Queue a follow-up (delivered after the current turn's tool calls). |
| `next_turn(msg)` | Aliased to `follow_up` in Phase 2. |

Each fires `QueueUpdate { has_queued }`.

### Reconfiguration

| Method | Behavior |
|--------|----------|
| `set_model(model)` | Swap the active model. Fires `ModelSelect`. |
| `set_thinking_level(level)` | Swap thinking level. Fires `ThinkingLevelSelect`. |
| `set_active_tools(&[name])` | Restrict the LLM's tool list to the named subset. Names unknown to the full catalog return `UnknownTool`. Fires `ActiveToolsSelect`. |
| `set_resources(resources)` | Replace skills + templates atomically. Fires `ResourcesUpdate { skills, templates }`. |

### Session control

| Method | Behavior |
|--------|----------|
| `append_entry(type_tag, data)` | Append a `Custom` session entry (**extension state** — NOT projected to the LLM). Returns the entry id, fires `AppendEntry`. |
| `navigate_tree(target_leaf)` | Switch the active session leaf, then rewrite the agent transcript from the new branch's `build_context()`. Fires `SessionBeforeTree` then `SessionTree`. |
| `compact(keep_recent)` | Drive a compaction round-trip: summarize everything before the last `keep_recent` messages, replace transcript with summary + tail, write a `Compaction` session entry. Fires `SessionBeforeCompact` then `SessionCompact`. |
| `session()` | Clone-cheap handle on the owned `Session` (Arc-backed). |

### Subscription + control

| Method | Behavior |
|--------|----------|
| `subscribe(listener)` | Register a listener. Returns `HarnessUnsubscribe` whose `cancel().await` removes it. |
| `abort()` | Cancel the in-flight turn (if any). |
| `wait_for_idle()` | Poll until the agent's run signal is `None`. |
| `agent()` | Escape hatch — returns `&Arc<Agent>` for behavior not yet first-classed on the harness. Narrows over time. |

---

## Event reference

```rust
pub enum AgentHarnessEvent {
    // Pass-throughs from grain-agent-core::AgentEvent
    AgentStart,
    AgentEnd { messages: Vec<AgentMessage> },
    TurnStart,
    TurnEnd { message: AssistantMessage, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: AgentMessage },
    MessageUpdate { message: AssistantMessage, assistant_message_event: AssistantMessageEvent },
    MessageEnd { message: AgentMessage },
    ToolExecutionStart { tool_call_id, tool_name, args },
    ToolExecutionUpdate { tool_call_id, tool_name, args, partial_result },
    ToolExecutionEnd { tool_call_id, tool_name, result, is_error },

    // Harness-own
    Abort,
    Settled,                                                    // after AgentEnd
    BeforeAgentStart { system_prompt, messages, tool_names },   // before turn dispatch
    QueueUpdate { has_queued: bool },
    ModelSelect { model: Model },
    ThinkingLevelSelect { level: ThinkingLevel },
    ActiveToolsSelect { names: Vec<String> },
    AppendEntry { entry_id: String, type_tag: String },
    SessionBeforeCompact { messages: Vec<AgentMessage> },
    SessionCompact { kept_from: Option<String> },
    SessionBeforeTree { from: Option<String>, to: Option<String> },
    SessionTree { current_leaf: Option<String> },
    ResourcesUpdate { skills: usize, templates: usize },
}
```

All variants are `Serialize + Deserialize` (camelCase JSON via the existing `serde(tag = "type", rename_all = "snake_case")` convention).

---

## Migration from manual `Agent::new`

The old shape:

```rust
let mut opts = AgentOptions::new(model, stream);
opts.tools = tools;
let agent = Agent::new(opts);
let session_writer = Arc::new(SessionWriter::open(&path)?);
agent.subscribe(Arc::new(move |event, _| {
    let w = session_writer.clone();
    Box::pin(async move {
        if let AgentEvent::MessageEnd { message } = event {
            let _ = w.append(&message);
        }
    })
})).await;
```

The new shape — the session-mirror is built in:

```rust
let session = Session::new(JsonlSessionStorage::open(&path)?);
let mut opts = AgentHarnessOptions::new(session, model, stream);
opts.tools = tools;
let harness = AgentHarness::new(opts).await;
```

`grain-headless` and `grain-tui` aren't migrated yet — the design doc plans the callsite swap as the last step. Phases 1-4 of the harness are done; the consumer-side flip is small once you decide to do it.

---

## What's still deferred

| Item | Status |
|------|--------|
| Pi's `Context` event (post `transform_context`) | Needs an internal agent-loop hook; not yet exposed |
| Pi's `BeforeProviderRequest` / `BeforeProviderPayload` / `AfterProviderResponse` | Need richer stream hooks in `LlmStream` |
| `SessionCompact` carrying summary text | Currently emits `kept_from` only; summary lives inside the new `MessageEnd` |
| Pending-write batching (atomic per-turn commit) | Phase 5+; current writes go through immediately |
| Dynamic `SystemPrompt::Dynamic` re-rendering | Stored but not re-evaluated on state changes |
| Multi-bucket queues (`steering` vs `follow_up` vs `next_turn` as separate) | `next_turn` aliased to `follow_up` for now |
| `Resources::skills` first-class validation in `skill()` | Synthetic prompt only |
| `Agent::fork` (separate session) | Needs a `SessionRepo` reference; not on the harness yet |

These are tracked in [agent-harness-design.md](./agent-harness-design.md). Each is independently shippable.

---

## Testing

```bash
cargo test -p grain-agent-harness agent_harness
```

20 unit tests cover the constructor, all four phases of public surface, and the event broadcaster.
