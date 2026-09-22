# Local Agent Session Handoff

## Current Objective

Extract the in-process local Ollama agent into a standalone `warp-local-agent` service that speaks Warp's multi-agent protobuf over HTTP+SSE. Spec: `specs/local-agent-service/TECH.md`.

## Last Updated

2026-09-22

## Active Feature

feat-051 (§D). Its additive half is committed: the app can reach the service and the in-process path still works. The removal half is not started. feat-048, feat-049 and feat-050 are done.

## Branch

- `daniel/dev` (feat-048, feat-049, feat-050 and the additive half of feat-051 are committed; nothing pushed)

## Current State

- Runtime: client-deferred tool calls land as `ToolCallsDeferred` + `AwaitingClientToolResults`; envelope lives in `local_agent_runtime::transcript`.
- Service: `cargo run -p warp_local_agent -- --listen 127.0.0.1:9377` serves `/ai/multi-agent`, `/ai/passive-suggestions`, `/health`, `/debug/spec`. 115 tests pass.
- Transport: `warp_multi_agent_client::generate_local_agent_output(client, base_url, request)` is ready and tested; nothing calls it yet.
- App: set `WARP_LOCAL_AGENT_URL=http://127.0.0.1:9377` (or `ApiKeys::local_agent_url`) and an Ollama turn goes to the service; leave it unset and the in-process path runs exactly as before.
- The in-process path is untouched apart from one guard in `response_stream.rs` and two match arms, and its 61 tests still pass.
- Environment notes: run cargo with `RUSTC_WRAPPER=` (sccache remote cache down); local clippy/rustfmt are 1.97 (no rustup).

## Recommended Next Step

Confirm the service works end to end before deleting anything. Start Ollama, run `cargo run -p warp_local_agent -- --listen 127.0.0.1:9377`, launch the app with `WARP_LOCAL_AGENT_URL=http://127.0.0.1:9377`, configure Ollama in Settings › AI, and walk the prompts in `dan_docs/how/local_ollama_manual_parity_prompts.md`. Then do the removal half of feat-051: delete the `local_*` modules and `ollama/agent_loop`, strip the tool loop from `response_stream.rs`, repoint `convert_from.rs` at `local_agent_runtime::transcript`, remove `FeatureFlag::LocalOllamaRuntimeToolUse` and its cargo feature, re-feed `agent_viz` from `ResponseEvent`s plus `GET /debug/spec`, and add the settings widget.
