# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-09-22  
**Active Feature:** (none — feat-048 and feat-049 complete; stopped for review before Sections C–E)  
**Status:** Awaiting review  

## What's Done

- feat-048 (TECH.md §A): `local_agent_runtime` can end a run when the model requests client-executed tools.
  - `ExecutionSite { InProcess, Client }` + defaulted `ToolExecutor::execution_site`.
  - `RuntimeEvent::ToolCallsDeferred { calls }` and `FinishReason::AwaitingClientToolResults`.
  - `run_loop` partitions calls by site, still applies trusted `pre_tool` hooks to client calls (deny → in-process error result), pairs cancelled results for deferred calls, and returns without the grounding cue once client calls are pending.
  - `local_agent_runtime::transcript` owns the `server_message_data` envelope; the bridge keeps its copy until Section D.
  - Two approved app edits only: one new match arm each in `app/src/ai/agent_viz/model.rs` and the bridge `event_mapper`.
- feat-049 (TECH.md §B): new crate `crates/warp_local_agent` (lib + bin `warp-local-agent`) speaking the cloud multi-agent protobuf over HTTP+SSE.
  - Endpoints: `POST /ai/multi-agent`, `POST /ai/passive-suggestions`, `GET /health`, `GET /debug/spec`. Default listen address `127.0.0.1:9377`.
  - Client tool calls are persisted as `ToolCall` messages when deferred; a continuation request's `UserInputs[ToolCallResult]` is rendered for the model, echoed back as a typed `ToolCallResult` message, and followed by the grounding cue.
  - Registry gating comes from `Settings.supported_tools` (documents/computer-use/ask-user/run_agents are now gated, previously documents were unconditional), `web_search_enabled`, `mcp_context`, and `InputContext.updated_skills_context`; plan mode is recovered from history on continuations.

## Verification (this session)

- `cargo test -p local_agent_runtime`: 32 unit + 30 integration passed (1 ignored live test).
- `cargo test -p warp_local_agent`: 115 passed (9 router tests with a scripted provider, incl. disconnect → cancel).
- `cargo check -p warp --lib`: ok. `cargo test -p warp local_runtime --lib --features local_ollama_runtime_tool_use`: 61 passed.
- `cargo clippy -p local_agent_runtime -p warp_local_agent --all-targets --tests -- -D warnings`: clean (local clippy is 1.97; no rustup, so the pinned 1.92 is unavailable).
- Formatting: `cargo fmt -p local_agent_runtime` and `-p warp_local_agent` with the project config both report clean; neither changed app file appears in the formatter's diff list. Repo-wide `./script/format --check` fails on 27 files this change never touched (pre-existing drift on `daniel/dev`, local rustfmt 1.97 vs pinned 1.92).
- `cargo metadata --locked` ok (Cargo.lock gained the crate entry).
- Binary smoke: `/health` → 200 `ok`, `/debug/spec` lists 8 built-in tools, garbage body → 400.

## Decisions / Blockers

- Port 9277 collides with Warp's own local HTTP server (`crates/http_server`, Stable=9277 … Oss=9282); the service defaults to 9377. Sections D/E must use the same value.
- sccache's remote cache (WebDAV on a Tailscale address) is unreachable on this machine; all cargo commands were run with `RUSTC_WRAPPER=`.
- `./script/format` on this machine also reorders imports in 28 untouched files on `daniel/dev` (pre-existing branch drift toward 2021-style ordering); those files were left alone.
- Temporary duplication until Section D: transcript envelope in the bridge; `local_*` modules still in `app/src/ai/`.

## Next

Review feat-048/049, then implement Section C (feat-050): `warp_multi_agent_client::generate_local_agent_output`.
