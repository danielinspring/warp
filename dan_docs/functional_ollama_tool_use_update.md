# Functional Ollama Tool Use Update

Date: 2026-05-02

## What Was Asked

The follow-up task was to implement functional Ollama tool use from the existing plan in a fresh context.

The requested behavior was:

1. Add a new feature flag, `local_ollama_runtime_tool_use` / `FeatureFlag::LocalOllamaRuntimeToolUse`, off by default.
2. When the flag is enabled and an Ollama config is present, route Ollama agent requests through the local runtime instead of the legacy single-turn Ollama loop.
3. Make `local_agent_runtime::AgentRuntime::run` execute the real provider/tool loop and stream runtime events.
4. Replace the placeholder `WarpToolExecutor` with an app-side bridge into Warp's normal action model and permission UI.
5. Prevent duplicate backend-style tool-result follow-up requests while the local runtime owns the loop.
6. Advertise only the V1 supported tools: `run_shell_command`, `read_files`, `grep`, `file_glob_v2`, and `search_codebase`.
7. Improve Ollama provider parsing so tool-call arguments can be JSON strings or JSON objects, while preserving tool call IDs through tool results.

## What I Asked For

During cleanup, the formatter had touched many unrelated files. I asked for approval to run a targeted `git restore` on formatting-only edits outside the intended change set, preserving the actual runtime, bridge, feature flag, and test changes.

That cleanup left the worktree focused on the intended implementation files plus this documentation.

## What Was Done

### Feature Flag

Added compile-time and runtime wiring for:

- `local_ollama_runtime_tool_use` in `app/Cargo.toml`
- `FeatureFlag::LocalOllamaRuntimeToolUse` in `crates/warp_features/src/lib.rs`
- `enabled_features()` wiring in `app/src/lib.rs`

### Local Runtime Loop

Updated `crates/local_agent_runtime/src/runtime.rs` so `AgentRuntime::run` now:

- Spawns the runtime loop.
- Streams `RuntimeEvent`s over an async channel.
- Sends tool schemas to the provider.
- Executes LLM-requested tools and feeds results back to the next LLM turn.
- Preserves original `tool_call_id`s.
- Checks cancellation before provider calls, before tool execution, and while awaiting provider/tool futures.
- Enforces LLM and tool timeouts.
- Keeps max-turn protection.

`run_to_completion` now uses the same loop and remains useful for tests and non-interactive callers.

### Ollama Provider

Updated `crates/local_agent_runtime/src/provider/ollama.rs` so Ollama tool-call arguments are accepted as either:

- JSON-encoded strings
- JSON objects

Assistant tool-call messages and `role=tool` result messages preserve the same call IDs for the next model turn.

### Tool Schemas

Tightened local runtime schemas in `crates/local_agent_runtime/src/tools/schema.rs` and `app/src/ai/local_runtime_bridge.rs`.

Only these V1 tools are advertised:

- `run_shell_command`
- `read_files`
- `grep`
- `file_glob_v2`
- `search_codebase`

Unsupported tools such as `edit_files` and `create_file` are not advertised and are not silently mapped.

### Warp Tool Execution Bridge

Reworked `app/src/ai/local_runtime_bridge.rs` so `WarpToolExecutor` no longer returns placeholder results.

The bridge now:

- Converts local runtime tool calls into Warp proto tools and then into `AIAgentAction`s.
- Queues those actions through `BlocklistAIActionModel`.
- Waits for the matching `FinishedAction`.
- Removes the consumed action result from the normal finished-result queue.
- Converts real Warp action results into concise tool-result content for Ollama.

Warp's existing permission and auto-execution behavior remains authoritative because the bridge routes through the same action model used by normal agent tools.

### Response Stream Integration

Updated `app/src/ai/blocklist/controller/response_stream.rs` and `app/src/ai/blocklist/controller.rs` so local runtime streams:

- Are selected only when Ollama config is present and `FeatureFlag::LocalOllamaRuntimeToolUse` is enabled.
- Emit normal `Init`, `ClientActions`, and `Finished` response events for transcript rendering.
- Skip normal controller action queuing after stream completion because the runtime already owns tool execution and continuation turns.
- Suppress retry/resume behavior that assumes backend-managed tool follow-up.

`BlocklistAIActionModel` now has `take_finished_action_result`, which removes only the finished action consumed by the local runtime.

### Tests Added

Runtime tests now cover:

- `run` streaming events.
- Tool schemas sent to the provider.
- Tool results fed back with the original call ID.
- Cancellation stopping the loop.
- Max-turn protection.
- Provider parsing for JSON-string and JSON-object arguments.

App bridge tests now cover:

- V1 tool schemas only.
- Mapping each supported tool to the expected Warp action.
- Unsupported tools returning an error.
- Real action result content being returned to the runtime.

## Verification

Passed:

```bash
cargo test -p local_agent_runtime
cargo check -p local_agent_runtime
cargo test -p warp local_runtime_bridge --lib
cargo check -p warp --lib
git diff --check
rustfmt --edition 2021 --check --config skip_children=true app/src/ai/local_runtime_integration.rs app/src/ai/local_runtime_bridge.rs
```

Known note:

```bash
cargo fmt --check
```

still reports pre-existing formatting drift in files unchanged by this implementation pass, including `app/src/ai/mod.rs`, `app/src/settings_view/ai_page.rs`, `crates/local_agent_runtime/src/events.rs`, `crates/local_agent_runtime/src/lib.rs`, `crates/local_agent_runtime/src/provider/mod.rs`, `crates/local_agent_runtime/src/tools/mod.rs`, and `crates/managed_secrets/src/manager.rs`.

The functional changed files pass targeted formatting and the build/test checks above.

## Manual Verification Still To Do

With `local_ollama_runtime_tool_use` enabled, manually ask Ollama to:

1. Run `pwd`.
2. Read a small file.
3. Grep for a known string.
4. Glob for a known file pattern.
5. Search the codebase.

Confirm the transcript renders normally and the model continues after each real tool result.
