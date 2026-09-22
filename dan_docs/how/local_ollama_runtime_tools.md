# How To Create Or Edit A Local Agent Tool

Date: 2026-09-22

This document explains how to add or edit the tools a local model can call when Warp is configured
to use Ollama or any OpenAI-compatible endpoint.

## Where The Agent Lives

The agent is not part of the Warp binary. It is a separate process, `warp-local-agent`, that speaks
the same protobuf protocol Warp uses to talk to its cloud backend: the app POSTs a
`warp_multi_agent_api::Request` and reads an SSE stream of base64 `ResponseEvent`s.

```
Warp app ──POST /ai/multi-agent (proto)──▶ warp-local-agent ──▶ your model endpoint
         ◀──SSE base64(ResponseEvent) ───
```

The practical consequence is the point of the whole design: changing a prompt, a tool schema or a
provider means restarting one small crate, not rebuilding Warp.

```sh
make agent          # 127.0.0.1:9377, override with LOCAL_AGENT_LISTEN
make agent-release
```

Warp finds the service through Settings › AI › "Local agent service URL", the
`WARP_LOCAL_AGENT_URL` environment override, or the `127.0.0.1:9377` default. If the service is not
running, an agent turn fails with an error naming the URL, because there is no in-process fallback.

## Two Kinds Of Tool

Every tool the model can call is one of these, and the difference decides where you implement it.

**Client-executed.** The service cannot run these: it has no terminal, no file system access to the
user's session and no UI. It emits a `ToolCall` message and ends the run. Warp executes the call
through the same path it uses for cloud agents, including the permission card, and sends the result
back on the next request. `run_shell_command`, `read_files`, `grep`, `file_glob_v2`,
`search_codebase`, `edit_files`, the document and computer-use tools, `ask_user_question`,
`run_agents`, `read_skill` and MCP calls all work this way.

**In-process.** The service runs these itself and the stream keeps going, so they never produce a
permission card. `web_search`, `web_fetch`, `git_status`, `draft_commit_message_context`,
`draft_pr_summary_context`, `update_todos`, `mark_todos_completed` and `list_skills`.

A tool is in-process precisely when its registry route says so; see
`LocalRuntimeToolRouteKind::is_in_process`.

## Main Files, All In `crates/warp_local_agent`

- `src/tool_proto.rs` — the schemas advertised to the model, the conversion from a model tool call
  into a protobuf tool, the reverse conversion used when replaying history, and the argument
  validators. Most tool work happens here.
- `src/registry.rs` — which tools exist for a given request, built from `Settings.supported_tools`,
  `web_search_enabled`, `mcp_context` and the skills in `InputContext`. Also decides plan mode and
  holds the todo state.
- `src/executor.rs` — runs the in-process tools and tells the runtime which site a call belongs to.
- `src/tool_results.rs` — renders a client-executed result as text for the model, and builds the
  message echoed back so the result is persisted.
- `src/prompt.rs` — the system prompt, including the per-request context section.
- `src/event_mapper.rs` — turns runtime events into the `ResponseEvent`s the client understands.
- `src/web/`, `src/git.rs`, `src/todos.rs` — the in-process tool implementations.

On the app side, only two things still know about local tools:
`app/src/ai/agent/api/impl.rs` points the request at the service, and
`app/src/ai/agent/api/convert_from.rs` renders the transcript envelope for tools that have no
protobuf form.

## Adding A Client-Executed Tool

1. Add its schema in `build_tool_schemas` (or a gated `*_schema` fn) in `src/tool_proto.rs`.
2. Register it in `src/registry.rs`, with a safety class and, if it should only appear when the
   client supports it, a `Settings.supported_tools` check.
3. Map the call to its protobuf form in `tool_call_to_proto_tool`, and back in
   `proto_tool_call_to_runtime_with_registry` so a replayed conversation restores it.
4. Render its result in `src/tool_results.rs`, both the text the model sees and the `is_error` flag.
5. Add tests beside each of those. `src/test_support.rs` builds request fixtures.

Warp must already know how to execute the protobuf tool. If it does not, that is app work first.

## Adding An In-Process Tool

Steps 1 and 2 above, then implement it in its own module and dispatch to it from
`ServiceToolExecutor::execute`, and give it a `LocalRuntimeToolRouteKind` that reports
`is_in_process`. It needs no protobuf tool form; its call and result travel in the transcript
envelope so the conversation still replays.

## Editing A Tool

Change the schema and the conversion together, then restart the service. A live check that the
model sees the change:

```sh
make agent
LOCAL_AGENT_PROVIDER_URL=http://host:4000/v1 \
LOCAL_AGENT_PROVIDER_KEY=sk-... \
LOCAL_AGENT_MODEL=qwen3-coder:latest \
  cargo run -p warp_local_agent --example smoke -- "list the files here"
```

That example drives the service exactly as Warp does, including answering a deferred tool call, so
it is the fastest way to see a protocol change without launching the app.

## Common Failure Modes

- **The turn produces nothing.** A tool call whose arguments the schema rejects used to vanish.
  It now comes back to the model as a tool error, so check the service log for
  "could not be expressed as proto".
- **The model invents an argument.** The validators reject unknown keys deliberately. Widen the
  schema or the allow-list in the conversion; do not silently drop the argument.
- **A result reads as `{:?}` debug output.** The transcript envelope was missing, so the replayed
  conversation fell back to formatting the protobuf. Check that the result message carries
  `server_message_data`.
- **The service is unreachable.** Warp reports the URL it tried. Start it with `make agent`.

## Minimal Review Checklist

- Schema, forward conversion, reverse conversion and result rendering all agree on the tool's name
  and arguments.
- The tool is gated the same way the cloud gates it, if the cloud gates it at all.
- Tests cover a valid call, a rejected argument, and the replay path.
- `cargo test -p warp_local_agent` passes, and a real prompt through `--example smoke` behaves.
