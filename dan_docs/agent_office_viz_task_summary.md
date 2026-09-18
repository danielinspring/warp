# Agent Office Visualization — Task Summary

## What Was Asked

You asked for a new feature:

> "create new feature visualize agent into 2d dot office which move arround rooms(stage) where there are working, also it should show all defautl prompt given to agent like tools, system prompt … etc"

A **2D agent "office"** view: rooms represent each phase the agent can be in (Thinking, one room per tool, Awaiting Permission, Idle/Done), with the agent rendered as a dot that moves between rooms as the local Ollama runtime emits `RuntimeEvent`s. The view also surfaces the agent's **default configuration** — system prompt, tool schemas, MCP servers, and skills — so you can see exactly what the agent is running with.

### Refined requirements (from clarifying questions)

- **Display surface**: a new pane modeled after `NetworkLogPane`.
- **Multi-agent**: data-model headroom only — render one dot today, but key the model by `AgentId` so adding a second tracked agent later is a render change, not a model change.
- **Permission UX**: mirror-only — the visualization shows the parked state; the existing chat UI handles accept/deny.
- **MCP & Skills**: display-only for now. They are not wired into the local Ollama runtime today (they belong to the cloud-agent stack), so each entry is tagged `not connected to local runtime` instead of pretending it's plumbed through.

---

## How It Was Done

### Approach

Tap the existing `RuntimeEvent` stream via a process-wide broadcast bus, fold each event into a small state model that maps it to a room, and render the rooms grid + dot + four configuration sections as a text snapshot inside a `CodeEditorView` pane (mirroring the `NetworkLogPane` pattern).

### Why a broadcast bus (not fanning out from `EventMapper`)

`EventMapper::map_event` already drops most variants (`_ => vec![]`) — `ToolExecutionStarted`, `ToolResult`, `PermissionRequired`, `TextDelta` would not survive a fanout from there, and those are exactly the events the viz needs. `RuntimeEvent` derives `Clone`, so a `tokio::sync::broadcast::Sender<RunScopedEvent>` is cheap and decouples the viz from proto mapping. Future panels and tests can subscribe without touching the integration code.

### Components

**Foundation (no UI):**
- `app/src/ai/local_runtime_spec.rs` — single source of truth for the agent's "configuration": `system_prompt()`, `local_tools()`, `local_mcp_servers()`, `local_skills()`, plus a shared `LocalRuntimeAttachment { Active, NotConnectedToLocalRuntime }` enum.
- `app/src/ai/local_runtime_event_bus.rs` — `OnceLock<broadcast::Sender<RunScopedEvent>>` with `publish`, `subscribe`, and `subscribe_local` (which pumps broadcast → `async_channel` on a dedicated std thread so warpui's `spawn_stream_local` can drive UI updates from the main thread without a Tokio reactor).

**State model (pure data, unit-tested):**
- `app/src/ai/agent_viz/model.rs` — `Room` enum (Idle, Thinking, Tool(name), Permission, Done), `AgentMarker`, `AgentVizModel::apply(run_id, event)` reducer.
- `app/src/ai/agent_viz/render.rs` — text-snapshot renderer producing the ASCII rooms grid (box-drawing chars + `*` for the active room) plus four sections: System prompt, Tools, MCP servers, Skills.

**View / pane layer (warpui plumbing):**
- `app/src/ai/agent_viz/view.rs` — `AgentVizView`: subscribes to the bus on construction, folds each event into the model, re-seeds a read-only `CodeEditorView` with the new snapshot. Implements `Entity / View / TypedActionView / BackingView`. Exposes a refresh button in the pane header.
- `app/src/ai/agent_viz/pane_manager.rs` — `AgentVizPaneManager` singleton mapping `WindowId → PaneViewLocator` so we open at most one pane per window.
- `app/src/pane_group/pane/agent_viz_pane.rs` — `AgentVizPane` (the `PaneContent` wrapper, mirrors `NetworkLogPane`).

**Integration / wiring:**
- `local_runtime_integration.rs` — replaced the inline system prompt with `local_runtime_spec::system_prompt()` and added a single `local_runtime_event_bus::publish(&run_id, event.clone())` call in the event loop, just before mapping to proto events.
- `IPaneType::AgentViz`, `LeafContents::AgentViz`, `PaneId::from_agent_viz_pane_{ctx,view}`, render dispatch in `pane_group/pane/mod.rs`.
- `WorkspaceAction::OpenAgentVizPane` + `Workspace::open_agent_viz_pane` (right-split, focuses existing pane on re-open).
- Static slash command `AGENT_VIZ` (`/agent-viz`) + dispatch arm in `terminal/input/slash_commands/mod.rs`.
- Singleton registration in `lib.rs`.
- Non-persistence branches in `app_state.rs`, `persistence/sqlite.rs`, `launch_configs/launch_config.rs`, and `pane_group/mod.rs` (the pane is derived from in-process events; nothing to restore).

### Bug fixed mid-implementation

First open of `/agent-viz` panicked with *"there is no reactor running, must be called from the context of a Tokio 1.x runtime."* `subscribe_local()` was using `tokio::spawn` to pump the broadcast receiver, but the UI thread that calls it has no Tokio reactor in scope. Switched the pump to a dedicated std thread using `broadcast::Receiver::blocking_recv` and `async_channel::Sender::send_blocking` — no runtime needed.

### Verification

- `cargo check -p warp` clean.
- `cargo test -p warp --lib agent_viz` — 5 tests pass (4 model state-transition tests + 1 render snapshot test).
- Open-the-pane verification is manual: type `/agent-viz`, confirm the rooms grid + four config sections render; run `/agent` in another pane, watch the dot move between rooms.

### What is NOT done

- The dot doesn't animate — it jumps between rooms. The plan called for a 300ms ease-out tween, but the text-rendered approach (room labels in a `CodeEditorView`) doesn't support sub-character interpolation. A real graphical pane would, but is a larger lift.
- MCP servers and skills are display-only with `not connected to local runtime` badges. When MCP/skills are eventually plumbed into `local_agent_runtime`, only `local_mcp_servers` / `local_skills` in `local_runtime_spec.rs` need to flip entries to `Active`.
- The pane is gated only by the existence of the slash command — no feature flag. If you want the legacy fallback path to also be the default surface, gate `/agent-viz` behind `FeatureFlag::LocalOllamaRuntimeToolUse` or similar.
