# Plan: MapleStory-Style Agent Office Visualization

## Objective

Replace the current ASCII-text Agent Office (rendered into a `CodeEditorView` pane) with a **MapleStory-inspired 2D platformer scene**: floating wooden platforms connected by ladders, a sprite character that walks/jumps between platforms as the agent moves between rooms, mushroom-NPC tools, wooden room signs, parallax background, mini-map, and inventory bar.

The reference image shows the visual target: floating tiered platforms, a player sprite, mushroom enemies, ladders, mini-map (top-left), HP/MP/EXP bars (bottom), inventory hotbar (bottom-right), wooden signs labeling locations.

## What already exists (and stays)

These pieces are graphics-agnostic and feed any future renderer unchanged:

- `crates/local_agent_runtime` → emits `RuntimeEvent` (TurnStarted, ToolExecutionStarted, ToolResult, PermissionRequired, TextDelta, TextCompleted, TurnCompleted, Warning, Finished).
- `app/src/ai/local_runtime_event_bus.rs` → process-wide `broadcast::Sender<RunScopedEvent>`.
- `app/src/ai/local_runtime_spec.rs` → `system_prompt()`, `local_tools()`, `local_mcp_servers()`, `local_skills()`.
- `app/src/ai/agent_viz/model.rs` → `Room`, `AgentMarker`, `AgentVizModel::apply()` reducer (pure data).

The text renderer (`agent_viz/render.rs`) and `AgentVizView`/`AgentVizPane` become **legacy fallback** behind a feature flag — keep them around so the new renderer can fail gracefully.

## Visual mapping (reference image → our domain)

| MapleStory element                  | Our concept                                          |
|-------------------------------------|------------------------------------------------------|
| Floating platforms                  | Rooms (Thinking, Tool: grep, Tool: read_files, …)    |
| Wooden signs                        | Room labels                                          |
| Player sprite                       | Agent dot — animated walk/jump                       |
| Mushroom NPC on platform            | Tool icon sitting in its room                        |
| Speech bubble above sprite          | `TextDelta` / `TextCompleted` content                |
| Floating damage number ("26")       | `TurnCompleted { reason }` flash                     |
| Red `!` over a platform             | `PermissionRequired` parking                         |
| Golden glow / fanfare               | `Finished { Done }`                                  |
| Mini-map (top-left)                 | Whole-office overview, current room highlighted      |
| HP bar                              | Turns-remaining (`max_turns - current`)              |
| MP bar                              | Tokens-remaining (if/when surfaced)                  |
| EXP bar                             | Progress through current turn                        |
| Inventory hotbar (bottom-right)     | Tools roster with last-used highlight                |
| Top-right party list                | MCP servers + Skills with attachment status badges   |
| Background parallax mountains/sky   | Decoration; tints by current `Room`                  |

## Approach comparison

| Approach                                   | Visual ceiling | Effort  | Process model              | Crash isolation | Decision |
|--------------------------------------------|----------------|---------|----------------------------|-----------------|----------|
| Custom drawing inside warpui scene/Element | Low            | High    | Same process               | Tied to warp    | ❌       |
| Embed Bevy as a library inside warp        | High           | High    | Same process, two render loops fighting | Tied to warp | ❌ |
| **Separate Bevy binary + WebSocket**       | High           | Medium  | Sibling process            | Isolated        | ✅       |
| Web app (Phaser/PixiJS) + WebSocket        | Very high      | Medium  | Browser tab or WebView     | Isolated        | ⚠️ alt   |
| Tauri (Rust shell + web frontend)          | Very high      | High    | Sibling process            | Isolated        | ⚠️       |

### Why separate Bevy binary

- **One toolchain.** No JS/asset-pipeline split in a Rust monorepo.
- **Idiomatic 2D in Rust.** Bevy's ECS, sprite/tilemap support (`bevy_ecs_tilemap`), and tween crates (`bevy_tweening`) cover everything we need.
- **Crash isolation.** A buggy renderer can't take warp down. Restart the viewer, the WS reconnects.
- **API-first.** Forces a clean event contract — exactly the boundary the user mentioned ("communicate through API").
- **Future-friendly.** A separate viewer can ship as its own binary, embed in a website later, or be replaced by a web frontend speaking the same WS protocol.

### Why not Phaser/Pixi (yet)

Lower-effort *if* you already have the JS/asset pipeline running. We don't, and adding one to a Rust workspace is a tax we don't need to pay this round. The WS protocol below is renderer-agnostic — switching to Phaser later is a viewer swap, not a warp change.

### Why not embed in warpui

warpui's `Element` system is designed for terminal-style widgets, not sprite blitting / tweening / parallax. The wgpu render loop is also already attached to warp's window — running a Bevy `App` inside the same window means contended event loops and stolen focus. Cleanly impossible without forking warpui.

## Architecture

```
+----------------------- warp (existing) ---------------------+
| local_agent_runtime → RuntimeEvent                          |
|                            ↓                                |
|                  local_runtime_event_bus                    |
|                            ↓                                |
|              [NEW] agent_office_ws_server                   |
|       (axum + ws, broadcasts JSON to all clients)           |
+-------------------------------|-----------------------------+
                                |   ws://127.0.0.1:<port>/agent-events
                                ↓
+--------------- crates/agent_office_viewer (NEW) -------------+
|  Bevy 2D app: tilemap rooms, sprite character, parallax,    |
|  mini-map, inventory bar, HP/MP/EXP                         |
|  Subscribes to events; replays into local scene state.      |
+-------------------------------------------------------------+
```

`/agent-viz` slash command becomes:
1. Ensure the WS server is running (lazy on first open, port chosen via `pick_port()`).
2. Spawn the viewer subprocess `agent-office-viewer --ws ws://127.0.0.1:<port>/agent-events`.
3. Existing pane stays as a thin status surface: "viewer running (pid X)" + a "Reopen" button + the legacy text view if `--legacy` is forced.

## API / event contract

WebSocket endpoint: `ws://127.0.0.1:<port>/agent-events`

Server pushes one JSON object per event. Schema (Rust side, `serde`-derived; mirrored in viewer):

```jsonc
// Sent on connect: snapshot of static config
{
  "type": "Hello",
  "system_prompt": "...",
  "tools":  [{ "name": "grep", "description": "...", "schema": { ... } }, ...],
  "mcp":    [{ "name": "github", "status": "NotConnectedToLocalRuntime" }, ...],
  "skills": [{ "name": "review-pr", "source": "Bundled", "status": "..." }, ...]
}

// Then one message per RuntimeEvent
{ "type": "TurnStarted",          "run_id": "…", "turn": 1 }
{ "type": "ToolExecutionStarted", "run_id": "…", "call_id": "…", "tool_name": "grep" }
{ "type": "ToolResult",           "run_id": "…", "call_id": "…", "is_error": false, "content_preview": "..." }
{ "type": "PermissionRequired",   "run_id": "…", "call": { "id": "…", "name": "run_shell_command", "arguments": { … } } }
{ "type": "TextDelta",            "run_id": "…", "text": "…" }
{ "type": "TextCompleted",        "run_id": "…", "text": "…" }
{ "type": "TurnCompleted",        "run_id": "…", "reason": "EndTurn|ToolUse|MaxTokens" }
{ "type": "Finished",             "run_id": "…", "reason": "Done|Cancelled|…" }
{ "type": "Warning",              "run_id": "…", "message": "…" }

// Optional control channel (viewer → warp), kept tiny
{ "type": "RequestReplay" }   // viewer reconnected, ask for a Hello + recent state
```

`run_id` plumbs through so the viewer can render multiple agents on multiple "floors" (future).

## Warp-side changes

### New files
- `app/src/ai/agent_office_ws_server.rs`
  - `pub struct AgentOfficeWsServer { port: u16 }` — singleton, lazy-started.
  - On start: bind on `127.0.0.1:0`, store the assigned port, run an axum router with one `ws` route.
  - Each ws connection: send `Hello` with current `local_runtime_spec` snapshot, then forward bus events translated to the wire schema.
  - Drop semantics: server lives for the warp process; reconnects are cheap.
- `app/src/ai/agent_office_launcher.rs`
  - `pub fn launch_viewer(ctx: &mut AppContext)`:
    - Ensure server is up; get port.
    - Resolve viewer binary path (build-time `cargo metadata` lookup → `target/debug/agent-office-viewer` for dev; bundled path in release).
    - Spawn subprocess with `Command::new(...).arg("--ws").arg(format!("ws://127.0.0.1:{port}/agent-events"))`.
    - Track child handle in a singleton so a second `/agent-viz` brings the existing window forward instead of spawning a duplicate.

### Modified files
- `app/src/ai/agent_viz/view.rs` — when `FeatureFlag::AgentOfficeViewer` is on, the pane shows a thin status panel ("Viewer running on port X" + buttons). When off, current ASCII fallback stays.
- `app/src/workspace/view.rs` — `OpenAgentVizPane` action also calls `agent_office_launcher::launch_viewer` (idempotent).
- `app/Cargo.toml` — add `tokio-tungstenite` (or reuse the in-repo `websocket` crate) and `axum` for the embedded server.
- `Cargo.toml` (workspace) — register the new `agent_office_viewer` member.
- `crates/local_agent_runtime/src/events.rs` — already `Clone + Serialize`-able? Audit; add `serde::{Serialize, Deserialize}` derives if missing. (Spot-check now: `RuntimeEvent` derives `Debug + Clone`; we'll likely need to add `Serialize`. Behind `#[cfg_attr(feature = "serde", derive(...))]` to keep the runtime crate dependency-light, then turn the feature on for warp.)

### Feature flag
- `FeatureFlag::AgentOfficeViewer` — gates the launcher and the status panel. When off, `/agent-viz` falls through to the legacy ASCII pane. This protects everyone who doesn't have the viewer binary built/installed.

## Viewer-side: `crates/agent_office_viewer`

### Crate skeleton

```
crates/agent_office_viewer/
├── Cargo.toml          # bin = "agent-office-viewer"
├── src/
│   ├── main.rs         # CLI parsing, Bevy App::new()
│   ├── ws.rs           # tokio-tungstenite client → channel<ViewerEvent>
│   ├── protocol.rs     # mirror of the wire schema, serde-derived
│   ├── world/
│   │   ├── mod.rs
│   │   ├── rooms.rs    # platform layout from Hello payload
│   │   ├── agent.rs    # sprite, walk-cycle, tween between rooms
│   │   ├── tools.rs    # mushroom NPCs per Tool room
│   │   └── effects.rs  # damage numbers, !, golden glow
│   ├── ui/
│   │   ├── minimap.rs
│   │   ├── hotbar.rs   # tools roster
│   │   ├── party.rs    # MCP + Skills list (top-right)
│   │   └── bars.rs     # HP/MP/EXP
│   └── assets.rs       # asset paths, embed_assets! macro
├── assets/
│   ├── sprites/        # placeholder Kenney CC0 → custom later
│   ├── tilesets/
│   ├── ui/
│   └── fonts/
└── tests/
    ├── protocol_roundtrip.rs
    └── room_layout.rs
```

### Bevy plugin layout

- `WsPlugin` — owns a `tokio` runtime + tungstenite client, pushes `ViewerEvent`s into a `mpsc::UnboundedSender<ViewerEvent>` mirrored as a Bevy `Resource`.
- `WorldPlugin` — spawns rooms (one entity per `Room`), agents, tools.
- `UiPlugin` — minimap / hotbar / party / bars on the egui or Bevy UI layer.
- `EffectsPlugin` — `bevy_tweening` for sprite movement; particle bursts for `Finished`.

### Room layout algorithm

Given the `Hello.tools` list (N tools) plus four fixed rooms (Idle, Thinking, Permission, Done):

- Top tier: Thinking platform centered.
- Middle tier: one platform per tool, evenly spaced. Ladders connecting to Thinking.
- Bottom tier: Idle (left), Permission (center), Done (right).

`RoomLayout` is a pure function in `world/rooms.rs` that takes a tool list and returns a `HashMap<RoomKey, Vec2>` of platform centers. Re-runs whenever the tool list changes (i.e. on every `Hello`).

### Sprite movement

State machine on the agent entity:

- `Idle(room)` — sprite idle anim on the room's platform.
- `Walking(from, to, t)` — `bevy_tweening` ease-out cubic over 300 ms; if `to` is on a different tier, insert a `Climbing(ladder, t)` segment.
- `ToolUsing(room)` — sprite faces the mushroom and plays attack/cast anim.
- `Parked(Permission)` — `!` over the head, sprite shaking.
- `Celebrating(Done)` — confetti + golden tint.

Transitions driven by the WS event stream.

### Asset plan (phased)

1. **Programmer art (Day 1)** — colored rectangles for platforms, a circle for the agent, text labels. Confirm protocol + scene wiring before spending art time.
2. **Kenney.nl CC0 packs (Week 1)** — `Platformer Pack Redux`, `Platformer Characters`, `Generic Items`. Free, attribution-light, looks fine.
3. **Custom MapleStory-inspired pixel art (later)** — commission or build with Aseprite. Keep tile size 32×32 to match Kenney so swap is incremental.

Sounds: skip until Phase 3. The pane is in the user's foreground; ambient music would be hostile.

## Phasing

### Phase 0 — protocol + skeleton (1 sitting)
- Add `Serialize` to `RuntimeEvent` (gated behind a `serde` feature on `local_agent_runtime`).
- Create `agent_office_viewer` crate; depend on `bevy = "0.14"`, `tokio-tungstenite`, `serde`, `serde_json`.
- Bevy "hello world": clear color, FPS counter, prints WS messages to stdout.
- Add `agent_office_ws_server.rs`: serves `Hello` + bus event stream on `/agent-events`.
- `/agent-viz` launches viewer subprocess.
- **Done when**: running `/agent` in warp causes the viewer window to print live event JSON.

### Phase 1 — programmer-art office (1–2 sittings)
- `RoomLayout` + room entities (rectangles).
- Agent entity (circle) tweens between rooms on every event.
- Tool roster (text labels) at the bottom of each tool room.
- **Done when**: dot visibly walks across the Office as `/agent` does its thing.

### Phase 2 — MapleStory aesthetics (Kenney assets) (2–3 sittings)
- Replace platform rects with tilemap chunks; ladders between tiers.
- Animated agent sprite (walk-cycle, idle).
- Mushroom NPC sprite per tool room with bobbing idle.
- Wooden room sign sprites.
- Parallax background (sky + 2 mountain layers).
- **Done when**: it looks like a recognizable MapleStory tribute.

### Phase 3 — UI overlays (1–2 sittings)
- Minimap (top-left).
- Inventory hotbar (bottom-right) with last-used tool highlight.
- HP/MP/EXP bars.
- Party list (top-right) for MCP / Skills with attachment-status badges.
- Speech bubbles above agent for `TextDelta` (with debounce).
- Damage-number popup on `TurnCompleted`; `!` parking on `PermissionRequired`; golden glow on `Finished { Done }`.
- **Done when**: the screenshot in `dan_docs/task/maple_office_visualization_plan.md` could plausibly be from our viewer.

### Phase 4 — polish (optional)
- Custom pixel art, idle ambient anims (clouds drift, leaves fall), audio pings on tool calls, viewer settings (zoom, speed).
- Multi-agent: stack two sprites with name tags.
- "Replay" mode: scrub the last N events.

## Tests

- **Protocol round-trip** (viewer side): build every variant of the wire enum, encode → decode, expect equality.
- **Room layout**: `room_layout(tool_list).len() == tool_list.len() + 4`; positions are non-overlapping; varying tool count produces stable Y-coords for fixed rooms.
- **Server smoke** (warp side): start the WS server in a unit test, connect with `tokio-tungstenite`, expect `Hello` first.
- **Manual end-to-end**: `/agent` in warp, watch viewer; cancel mid-run, expect dot return to Idle; trigger `PermissionRequired`, expect `!` parking.

## Risks / open questions

- **Port collisions** — using `127.0.0.1:0` (kernel-assigned) avoids static port clashes. The chosen port is published only over the local IPC the launcher already uses, so no firewall surface.
- **Serde on `RuntimeEvent`** — adding `serde::Serialize` to the runtime crate is a small but real public API change. Gate behind a `serde` feature so non-warp consumers aren't pulled in.
- **Subprocess lifecycle** — kill the viewer on warp shutdown? My take: no. Treat the viewer like an external monitor app — if warp crashes, the viewer just sits idle showing the last state until the user closes it. The launcher de-dupes so `/agent-viz` always brings the existing window forward.
- **Asset licensing** — Kenney.nl is CC0; safe to ship. Custom art is later and out of scope for this plan.
- **Cross-platform binaries** — the viewer is a separate binary. Distribution beyond developer machines means cargo-bundling it the same way warp itself ships, which is a separate workstream. For now, dev-only via `cargo build --bin agent-office-viewer`.
- **WebSocket vs IPC sockets** — chose WS because the in-repo `websocket` crate exists and a WS endpoint also opens the door to a future browser-based viewer with zero protocol change. Unix domain sockets / named pipes would be marginally faster but lose that property and don't help on Windows.
- **Bevy version churn** — pin to `bevy = "0.14"` (current LTS-ish); `bevy_tweening` has a matching version. Plan budget for one upgrade across the project's lifetime.

## Out of scope

- Multi-agent rendering beyond a "you could now" data model.
- Live editing of system prompt / tools from the viewer.
- Mobile / web build of the viewer.
- Replacing warpui's renderer or moving the viewer into the warp main window.

## Critical paths

- `app/src/ai/local_runtime_event_bus.rs` — add a tap that forwards into the WS server.
- `app/src/ai/agent_office_ws_server.rs` (new) — axum + ws.
- `app/src/ai/agent_office_launcher.rs` (new) — subprocess management.
- `crates/local_agent_runtime/src/events.rs` — `serde::Serialize` (feature-gated).
- `crates/agent_office_viewer/` (new crate) — Bevy app.
- `Cargo.toml` (workspace) — add new member.
- Existing `agent_viz/{view,model,render}.rs` — kept as legacy fallback.
