# Local Agent Session Handoff

## Current Objective

Extract the in-process local Ollama agent into a standalone `warp-local-agent` service that speaks Warp's multi-agent protobuf over HTTP+SSE. Spec: `specs/local-agent-service/TECH.md`.

## Last Updated

2026-09-22

## Active Feature

None. feat-048 (§A runtime deferral) and feat-049 (§B `crates/warp_local_agent`) are done and awaiting review; feat-050..052 (§C, §D, §E) are recorded as not-started.

## Branch

- `daniel/dev` (uncommitted working tree; no commits were made)

## Current State

- Runtime: client-deferred tool calls land as `ToolCallsDeferred` + `AwaitingClientToolResults`; envelope lives in `local_agent_runtime::transcript`.
- Service: `cargo run -p warp_local_agent -- --listen 127.0.0.1:9377` serves `/ai/multi-agent`, `/ai/passive-suggestions`, `/health`, `/debug/spec`. 115 tests pass.
- App: only two match arms changed (`agent_viz/model.rs`, bridge `event_mapper`); the in-process path still works (61 `local_runtime` tests pass) and nothing routes to the service yet.
- Environment notes: run cargo with `RUSTC_WRAPPER=` (sccache remote cache down); local clippy/rustfmt are 1.97 (no rustup).

## Recommended Next Step

After review: implement feat-050 (TECH.md §C) — add `generate_local_agent_output` to `crates/warp_multi_agent_client`, reusing the `decode_response_event` tail, with a URL test in `lib_tests.rs`. Use base URL default `http://127.0.0.1:9377`.
