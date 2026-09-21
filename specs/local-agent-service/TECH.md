# Local Ollama agent → standalone service (`warp-local-agent`)

## Context

The local Ollama agent (commit `86df5c54`) runs the whole LLM→tool loop *inside* the Warp app:
`app/src/ai/local_runtime_bridge.rs` (5.4k lines: tool registry, schemas, proto mapping, event mapper),
`local_runtime_integration.rs`, `local_runtime_spec.rs` (system prompt), model packs, local web/git/todo
tools, plus a private in-memory tool loop in `blocklist/controller/response_stream.rs`. Every prompt,
schema or provider tweak therefore forces a full `warp` rebuild.

Oz cloud agent avoids this because the agent lives behind a wire protocol: the app POSTs a
`warp_multi_agent_api::Request` (protobuf) to `/ai/multi-agent`, reads an SSE stream of base64
`ResponseEvent`s (`crates/warp_multi_agent_client/src/lib.rs`), executes tool calls itself, and sends
results back as `UserInputs[ToolCallResult]` in the next request. The server is stateless per request
(full task history travels in `task_context.tasks`, server-private state in `Message.server_message_data`).

**Goal:** make the local agent a separate process that speaks that exact protocol, so Warp only knows
a URL, and iterating on the agent means rebuilding/restarting one small crate. Decisions already made
with the user: Warp proto over HTTP+SSE; in-process path removed entirely; service started manually
(`make` target); Ollama settings stay in Warp and are forwarded per request.

Everything the service needs is already in the proto request: cwd/shell/OS/images/files
(`InputContext`), skills (`updated_skills_context`), MCP (`mcp_context`), plan mode (`UserQuery.mode`),
autonomy (`autonomy_level`), capabilities (`supported_tools`, `web_search_enabled`), context window
(`base_model_context_window_limit`), and BYO endpoints (`Settings.custom_model_providers`).

## Target architecture

```
Warp app ──POST /ai/multi-agent (proto Request)──▶ warp-local-agent (axum, :9277) ──▶ Ollama
         ◀──SSE base64(ResponseEvent) ────────────                │
  executes ToolCall messages with the                              runs local_agent_runtime;
  existing cloud tool loop, re-requests                            executes in-process tools
  with UserInputs[ToolCallResult]                                  (web, git, todos, list_skills);
                                                                    *defers* Warp tools to client
```

One request = one "runtime run" that ends either with `StreamFinished{Done}` (text answer) or with
`ToolCall` message(s) persisted + `StreamFinished{Done}` (client executes, comes back). In-process tools
never end the stream; the runtime keeps looping until it hits a client tool or a final answer.

## Work breakdown

### A. `crates/local_agent_runtime` — client-deferred tools (small)

- `tools/mod.rs`: add `enum ExecutionSite { InProcess, Client }` and a defaulted trait method
  `ToolExecutor::execution_site(&self, call) -> ExecutionSite` (default `InProcess`).
- `events.rs`: add `RuntimeEvent::ToolCallsDeferred { calls }` and
  `FinishReason::AwaitingClientToolResults`.
- `runtime.rs` `run_loop` (after `history.push_assistant(response_text, calls)` at ~L415): partition
  `calls` by site. Execute `InProcess` ones exactly as today (batches, hooks, `ToolResult` events,
  `push_tool_result`). If any `Client` calls exist, emit `ToolCallsDeferred`, `emit_finished(...
  AwaitingClientToolResults)`, `return Ok(history)`. Skip the "Tool results are above…" grounding
  cue on that exit (the service adds it when the results arrive on the next request).
- Move the transcript envelope (`LocalRuntimeTranscriptData`, `encode_/decode_local_runtime_tool_*_data`
  from `local_runtime_bridge.rs:60-122`) into a new `local_agent_runtime::transcript` module — it is
  the only piece the app still needs to decode (`app/src/ai/agent/api/convert_from.rs` renders it).
- Tests in `tests/runtime_tests.rs`: mixed batch [in-process, client] → in-process result recorded,
  deferred event emitted, loop ends with `AwaitingClientToolResults`; all-client batch; all-in-process
  batch unchanged.

### B. New crate `crates/warp_local_agent` (lib + bin `warp-local-agent`)

Deps: `axum`, `tokio`, `prost`, `base64`, `futures`, `async-stream`, `serde_json`, `tracing`,
`warp_multi_agent_api`, `local_agent_runtime`, `warp_util` (git helpers). No `warpui`/`app`.
Add to workspace `Cargo.toml` members. Modules (mostly *moves* from `app/src/ai/`):

| Service module | Source | Notes |
|---|---|---|
| `server.rs` | new | axum router: `POST /ai/multi-agent` (decode proto body, `Content-Type: application/x-protobuf`), `POST /ai/passive-suggestions` (reply `Init`+`Finished{Done}` — today this errors "No user query"), `GET /health`, `GET /debug/spec` (system prompt + tool schemas JSON, for the agent-viz pane). SSE `data:` = `BASE64_URL_SAFE(ResponseEvent.encode_to_vec())`, matching `decode_response_event`; `Sse::keep_alive` so slow model loads don't idle-out. Dropping the response body drops the run future → `CancelHandle::cancel()` via a drop guard. |
| `request.rs` | new (+ `local_runtime_integration.rs` helpers) | proto `Request` → `TurnPlan`: ids (`metadata.conversation_id` or new UUID, `run_id` = `ambient_agent_task_id` or UUID, `task_id` = last task or new), history via existing `build_initial_messages`/`translate_proto_to_runtime_message`/`retain_paired_tool_messages`, provider config from `custom_model_providers` (provider whose `models[].config_key == model_config.base`; error `StreamFinished{InternalError}` if absent), user message from `UserInputs[UserQuery]` + `InputContext.images` (port of `build_user_message`/`image_to_content_part` — image resize currently uses `crate::util::image`; move the small `process_image_for_agent` logic or skip resizing), tool results from `UserInputs[ToolCallResult]` (see `tool_results.rs`) + the grounding cue user message from `runtime.rs:511-520`. |
| `registry.rs` | `local_runtime_bridge.rs` `LocalRuntimeToolRegistry`, `build_tool_schemas`, routes | Rebuild `from_request_with_available_skills` on proto: `supported_tools` contains `AskUserQuestion`/`RunAgents`/`UseComputer`… → add those tools; `web_search_enabled`; `mcp_context`; skills from `input.context.updated_skills_context` + `InputContext` skill attachments; permission mode from last `UserQuery.mode == Plan` else `autonomy_level == Unsupervised → AcceptEdits`; `working_directory` from `input.context.directory.pwd`; todos `hydrate_from_tasks`; parent gating via `metadata.parent_agent_id`. |
| `tool_proto.rs` | bridge `tool_call_to_proto_tool[_with_registry]`, `proto_tool_call_to_runtime_with_registry`, `edit_files_tool_call_to_proto`, `*_tool_call_to_proto`, JSON↔prost helpers, `shell_command_is_read_only`, argument validators | Pure proto/JSON code — moves as-is. **Drop** the app-typed twins (`tool_call_to_ai_action*`, `*_tool_call_to_ai_action`, `parse_file_edit`, `action_result_to_tool_result`) — the app converts proto `ToolCall` messages with its existing `convert_from.rs` path. |
| `tool_results.rs` | **new**, port of `action_result_to_content` (bridge L1909-2359) + `action_result_to_tool_call_result_client_actions` | Input is now proto `request::input::ToolCallResult` instead of `AIAgentActionResultType`; same text rendering per variant (RunShellCommand, ReadFiles, Grep, FileGlobV2, ApplyFileDiffs, MCP, ReadSkill, AskUserQuestion, RunAgents, ReadShellCommandOutput, WriteToLongRunningShellCommand, Cancel…). Produces (a) `ToolResultMessage` for history and (b) the echoed `AddMessagesToTask(ToolCallResult message, server_message_data = transcript envelope)` — the client never persists tool results itself (verified: no `Message::ToolCallResult(` construction in the cloud path). |
| `event_mapper.rs` | bridge `event_mapper` module | Moves as-is; `ToolCallsRequested` already emits `ToolCall` messages; map `ToolCallsDeferred` to nothing (calls were already emitted) and `Finished{AwaitingClientToolResults}` → `StreamFinished{Done}`. |
| `executor.rs` | bridge `WarpToolExecutor` | Becomes `ServiceToolExecutor`: keeps the in-process branches (`list_skills`, `web_*`, `git_*`, todos); `execution_site` returns `Client` for everything else; `execute` for a client tool is unreachable. |
| `prompt.rs` | `local_runtime_spec.rs` | `PromptBuildInput::from_request` reads `InputContext` (pwd/shell/OS), `Settings` (`rules_enabled`, `warp_drive_context_enabled`), registry, `mcp_context`; `render_request_context` ports `AIAgentContext` rendering to `InputContext.files/selected_text/images`. Keep `local_mcp_servers`/`local_skills` (AppContext helpers) in the app — only `agent_viz` uses them. |
| `model_packs.rs`, `web/`, `git.rs`, `todos.rs` | `local_runtime_model_packs.rs`, `local_web/*`, `local_git.rs`, `local_todos.rs` | Already app-free except `local_git.rs` → replace `crate::util::git::{get_repo_git_summary, get_file_change_entries, get_diff_for_commit_message, detect_current_branch}` with copies built on `warp_util::git::run_git_command` (those helpers only use `warp_util` + `safe_warn`). |
| `turn.rs` | `local_runtime_integration.rs::run_runtime` | Same shape: build provider/executor/hooks/telemetry, `runtime.run(...)`, pump `RuntimeEvent` → `EventMapper` → SSE. Telemetry: keep `LoggingHooks` + tracing; drop `local_runtime_telemetry.rs` (needs `warp_core::telemetry`) or keep as structured log only. |
| `main.rs` | new | `--listen 127.0.0.1:9277` (env `WARP_LOCAL_AGENT_LISTEN`), tracing subscriber, request/turn logging. |

Tests: move bridge unit tests (bridge `mod tests`, `local_runtime_integration_tests.rs`,
`local_runtime_model_packs_tests.rs`, `local_git`/`local_todos`/`local_web` tests) alongside their
modules; add `server_tests.rs` driving the axum router in-process with a fake `LLMProvider`
(`test-util` feature already exists on the runtime) for: text-only turn, tool-call turn ends the stream
with `ToolCall` + `Finished`, `ToolCallResult` continuation echoes the result message and continues,
passive-suggestion no-op, missing provider → `InternalError`, client disconnect cancels the run.

### C. `crates/warp_multi_agent_client` — local transport

Add `pub async fn generate_local_agent_output(client: &http_client::Client, base_url: &str, request: &Request) -> Result<OutputStream, Error>`:
`POST {base_url}/ai/multi-agent` (or `/ai/passive-suggestions` via the existing
`is_passive_suggestion_request`), `.proto(request).prevent_sleep(..).eventsource()`, then the same
`filter_map`/`decode_response_event`/tracing-span code as `generate_multi_agent_output` (factor the
stream-decoding tail into a shared helper). No auth token, no ambient headers, no IAP wrapper.
Unit test in `lib_tests.rs` for the URL.

### D. `app/` — route to the service, delete the in-process loop

1. **Settings** — `crates/ai/src/api_keys.rs`: add `local_agent_url: Option<String>` next to
   `ollama_*` (default `http://127.0.0.1:9277`; env `WARP_LOCAL_AGENT_URL` overrides for dev).
   Expose an input in the existing Ollama section of the AI settings page (follow
   `set_ollama_base_url` / its settings widget). `OllamaConfig` (`app/src/ai/agent/api.rs:134`) gains
   `service_url`.
2. **Request building** — `app/src/ai/agent/api/impl.rs::generate_multi_agent_output`: remove the
   early-return at L26-33. Build the `api::Request` once (unchanged), then if
   `params.ollama_config` is `Some`: set `settings.custom_model_providers` to one
   `CustomModelProvider { base_url, api_key, schema: OPENAI_CHAT_COMPLETIONS, models: [CustomModel { slug: model, config_key: "local-ollama" }] }`,
   set `model_config.base = "local-ollama"`, clear `api_keys`, and call
   `generate_local_agent_output(server_api.http_client(), &cfg.service_url, &request)`; else the cloud
   call. Error mapping reuses `convert_multi_agent_client_error`.
3. **Controller** — `app/src/ai/blocklist/controller/response_stream.rs`: delete
   `local_runtime_tool_loop`, `action_model`, `pending_local_runtime_tool_results`,
   `handle_local_runtime_tool_request`, `handle_local_runtime_action_event`, `owns_tool_loop`, the
   `spawn_stream_local(tool_request_rx …)`/`subscribe_to_model` wiring, and the cancel drain at
   L836-849; `new()` always takes the `spawn_request` branch. Remove `owns_tool_loop()` callers in
   `controller.rs`. The standard cloud tool loop (`AIAgentInput::ActionResult` →
   `UserInputs[ToolCallResult]`, `controller.rs:262/791`) now drives local tools.
4. **Delete** `app/src/ai/local_runtime_bridge.rs`, `local_runtime_integration(.rs|_tests.rs)`,
   `local_runtime_model_packs(.rs|_tests.rs)`, `local_web/`, `local_git.rs`, `local_todos.rs`,
   `local_runtime_event_bus.rs`, `local_runtime_telemetry.rs`, `ollama/agent_loop(.rs|_tests.rs)`;
   trim `local_runtime_spec.rs` to the AppContext helpers used by `agent_viz` (or fold them into
   `agent_viz`). Keep `ollama/mod.rs` (`OllamaClient` is used by `ai_assistant/requests.rs` and the
   settings connection test). Update `app/src/ai/mod.rs`.
5. **Transcript rendering** — `convert_from.rs` `format_local_runtime_tool_{call,result}_message`:
   import the envelope from `local_agent_runtime::transcript`. `orchestration/snapshots_tests.rs`
   and `blocklist/action_model/execute/request_file_edits.rs::pending_candidate_diffs` referenced the
   bridge race — the race no longer exists (single ToolCall-message path); remove
   `pending_candidate_diffs` if nothing else uses it.
6. **Flags** — remove `FeatureFlag::LocalOllamaRuntimeToolUse` (`crates/warp_features/src/lib.rs:597`,
   `app/src/features.rs:358`, `app/src/bin/oss.rs:32`) and cargo feature
   `local_ollama_runtime_tool_use` (`app/Cargo.toml:621,793`). Routing is simply "Ollama configured".
7. **Agent-viz pane** (`app/src/ai/agent_viz/`) — it consumed `RuntimeEvent`s from the in-process bus.
   Re-feed it from what the app already sees: in `ResponseStream::handle_response_stream_event`
   publish a small `AgentVizEvent` (TurnStarted ← `StreamInit`; ToolRequested ← `AddMessagesToTask`
   with `ToolCall`; ToolStarted/ToolFinished/PermissionRequired ← `BlocklistAIActionEvent`;
   TextDelta ← `AppendToMessageContent`; Finished ← `StreamFinished`) and map those in
   `agent_viz/model.rs` instead of `RuntimeEvent`. System prompt/tool list in `render.rs` come from
   `GET /debug/spec` (fetch once per pane open; fall back to "service offline"). This also makes the
   pane work for cloud runs.
8. **Failure UX** — when the service is unreachable, `AIApiError::from_stream_error` already surfaces
   a connection error; add a one-line hint in the error text: "Start it with `make agent`".

### E. Dev loop

- `Makefile`: `make agent` → `cargo run -p warp_local_agent -- --listen 127.0.0.1:9277`;
  `make agent-release`. Document in `dan_docs/how/local_ollama_runtime_tools.md` (replace the
  in-process activation section) and `AGENTS.md`.
- `script/macos/bundle`: no change now (manual lifecycle); note sidecar bundling as follow-up.

## Verification

1. `cargo test -p local_agent_runtime` (new deferral tests) and `cargo test -p warp_local_agent`
   (moved unit tests + router tests with the fake provider).
2. `cargo check -p warp --lib` and `cargo test -p warp -- convert_from response_stream` after the
   deletions; `cargo clippy --workspace` for dead code left behind.
3. Protocol smoke without Warp: run `make agent`, then a small script POSTs a proto `Request`
   (encode with `prost` in a `#[test]`/example under `crates/warp_local_agent/examples/`) with a
   `UserQuery` and asserts the SSE sequence `Init → ClientActions(CreateTask, AddMessagesToTask…) →
   Finished`; then a second request carrying the returned `ToolCall` message and a
   `ToolCallResult` and asserts the echoed result message + final text.
4. End-to-end with Ollama: `make agent` + `make oss`, configure Ollama in Settings › AI, then in a
   terminal run the parity prompts in `dan_docs/how/local_ollama_manual_parity_prompts.md`:
   text-only answer; `run_shell_command` (confirm the permission card appears, then the result is
   sent back and the model answers from it); parallel read-only tools; `edit_files` diff card;
   `web_search`/`git_status` (in-process, no card, still transcript-visible); todos persist across
   turns; plan mode blocks mutating tools; cancel mid-generation stops the service run (check its
   log); kill the service mid-turn → Warp shows the connection error and recovers on retry.
5. Iteration check (the actual goal): change a tool description in `registry.rs`, `make agent`
   only, re-run a prompt — new schema is live without rebuilding Warp.

## Follow-ups (not in this change)

- Auto-spawn/health-check the sidecar from Warp and bundle it in `script/macos/bundle`.
- Support non-Ollama OpenAI-compatible providers via the same `custom_model_providers` field.
