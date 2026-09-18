# Local Agent Runtime — Task Summary

## What Was Asked

You asked whether to build an Ollama agent runtime as a **separate backend service** (like Warp's own backend) or keep it **local inside the app**. After discussing the tradeoffs, you chose to keep it local but requested it be a **clean, separate folder/module** so it can easily be moved to a backend later.

You then asked me to **create a plan and implement** this separable local agent runtime module, using the **Claude Code codebase** (`/Users/lt-018/codish/open_soures/claude-code-main`) as architectural reference.

### Specific Requirements

1. Build a self-contained agent runtime that handles the full LLM → tool_calls → execute → results → loop cycle
2. Keep it in a **separate crate** (not just a folder) so Cargo enforces dependency isolation
3. Design clean **trait boundaries** so the module can be extracted to a standalone service later with minimal effort
4. Support multiple local LLM providers (Ollama now, others later)
5. Reuse Warp's existing tool execution pipeline (don't reinvent shell commands, file reads, etc.)
6. Don't break the existing `agent_loop.rs` path — new code is opt-in

---

## How It Was Done

### Phase 1: Research

Explored two codebases in parallel:

1. **Warp codebase** — Understood the current Ollama agent loop (`app/src/ai/ollama/agent_loop.rs`), tool execution pipeline (`action_model/execute/`), permission system (`BlocklistAIPermissions`), and the `ResponseEvent` proto stream format.

2. **Claude Code codebase** — Studied its architecture for design inspiration: async generator-based agent loop, dependency injection, tool registry with schemas, permission engine as middleware, message normalization boundary, and result budgeting.

### Phase 2: Implementation

Created the following structure:

#### New Crate: `crates/local_agent_runtime/`

| File | Purpose |
|------|---------|
| `Cargo.toml` | Standalone crate with minimal deps (no app/proto dependencies) |
| `src/lib.rs` | Public API surface with re-exports |
| `src/runtime.rs` | `AgentRuntime` — the core event-driven agent loop |
| `src/config.rs` | `RuntimeConfig` — max_turns, timeouts, system prompt |
| `src/events.rs` | `RuntimeEvent` enum — what the runtime yields to callers |
| `src/error.rs` | Error types (ProviderError, ToolExecutionError, RuntimeError) |
| `src/provider/mod.rs` | `LLMProvider` trait — the LLM abstraction |
| `src/provider/ollama.rs` | `OllamaProvider` — talks to Ollama's OpenAI-compatible API |
| `src/tools/mod.rs` | `ToolExecutor` trait + `ToolCall`/`ToolCallResult` types |
| `src/tools/schema.rs` | `ToolSchema` + builder (OpenAI function-calling format) |
| `src/messages/mod.rs` | `ConversationHistory` — provider-agnostic message types |
| `src/messages/normalize.rs` | Truncation utilities for large tool results |
| `tests/runtime_tests.rs` | 6 integration tests |

#### App-Side Bridge: `app/src/ai/`

| File | Purpose |
|------|---------|
| `local_runtime_bridge.rs` | `WarpToolExecutor` (implements `ToolExecutor` for Warp) + `EventMapper` (maps `RuntimeEvent` → proto `ResponseEvent`) |
| `local_runtime_integration.rs` | `run_with_local_runtime()` — drop-in replacement for `agent_loop::run_request()` |

### Phase 3: Verification

- All 6 unit tests pass (`cargo test -p local_agent_runtime`)
- Full app builds cleanly (`cargo check -p warp --lib`)
- Existing `agent_loop.rs` path untouched and still works

---

## Key Architecture Decisions

### Why a separate crate (not just a module)?

Cargo enforces the dependency boundary at compile time. The runtime crate physically **cannot** import from `app/` or access `warpui`, `AppContext`, proto types, etc. This guarantees clean extraction later.

### Three trait boundaries (the "seams")

```
┌─────────────────────────────────────────────────┐
│              AgentRuntime                        │
│  (drives the LLM → tool → result loop)          │
└─────────┬──────────────────────┬────────────────┘
          │                      │
   ┌──────▼──────┐       ┌───────▼───────┐
   │ LLMProvider │       │ ToolExecutor  │
   │  (trait)    │       │  (trait)      │
   └──────┬──────┘       └───────┬───────┘
          │                      │
   ┌──────▼──────┐       ┌───────▼───────┐
   │   Ollama    │       │  Warp Bridge  │
   │  Provider   │       │ (app layer)   │
   └─────────────┘       └───────────────┘
```

- **`LLMProvider`** — Swap LLM backends without touching the loop
- **`ToolExecutor`** — Swap tool execution without touching the loop
- **`RuntimeEvent` stream** — Caller maps to whatever output format they need

### How to extract to a separate backend later

1. `crates/local_agent_runtime/` becomes its own repo/service
2. Add gRPC/HTTP transport in front of `AgentRuntime::run()`
3. Replace app-side bridge with a network client
4. Move `ToolExecutor` impl server-side
5. **Zero logic changes** in the runtime itself

---

## Related Commit

```
Commit: e125bc1
Branch: cla-dev
Message: feat: add local_agent_runtime crate — separable agent loop module
Files:  19 changed, 2170 insertions
```

### Files Changed

```
modified:   Cargo.lock
modified:   Cargo.toml                          (added workspace dep)
modified:   app/Cargo.toml                      (added local_agent_runtime dep)
modified:   app/src/ai/mod.rs                   (registered new modules)
new file:   app/src/ai/local_runtime_bridge.rs
new file:   app/src/ai/local_runtime_integration.rs
new file:   crates/local_agent_runtime/Cargo.toml
new file:   crates/local_agent_runtime/src/config.rs
new file:   crates/local_agent_runtime/src/error.rs
new file:   crates/local_agent_runtime/src/events.rs
new file:   crates/local_agent_runtime/src/lib.rs
new file:   crates/local_agent_runtime/src/messages/mod.rs
new file:   crates/local_agent_runtime/src/messages/normalize.rs
new file:   crates/local_agent_runtime/src/provider/mod.rs
new file:   crates/local_agent_runtime/src/provider/ollama.rs
new file:   crates/local_agent_runtime/src/runtime.rs
new file:   crates/local_agent_runtime/src/tools/mod.rs
new file:   crates/local_agent_runtime/src/tools/schema.rs
new file:   crates/local_agent_runtime/tests/runtime_tests.rs
```

---

## How to Activate the New Runtime

In `app/src/ai/agent/api/impl.rs`, replace:

```rust
let stream = crate::ai::ollama::agent_loop::run_request(ollama_cfg, params);
```

with:

```rust
let stream = crate::ai::local_runtime_integration::run_with_local_runtime(ollama_cfg, params);
```

This switches from the single-shot v1 agent loop to the full multi-turn runtime with tool schema advertisement and proper tool_call_id round-tripping.

---

## What's Left (Future Work)

1. **Wire real tool execution** — `WarpToolExecutor::execute()` currently returns a placeholder. Needs `AppContext`/`ModelContext` to call into Warp's existing action executors.
2. **Streaming** — `OllamaProvider` currently buffers the full response. Add SSE streaming support.
3. **Permission UI integration** — Wire `PermissionRequired` events to Warp's confirmation dialog.
4. **Context compaction** — Add hooks for auto-compact when conversation gets too long.
5. **More providers** — Add OpenAI-compatible, Anthropic direct, etc.
