# Local Agent Session Handoff

## Current Objective

Extract the in-process local Ollama agent into a standalone `warp-local-agent` service that speaks Warp's multi-agent protobuf over HTTP+SSE. Spec: `specs/local-agent-service/TECH.md`.

## Last Updated

2026-09-22

## Active Feature

None. feat-048 (§A runtime deferral), feat-049 (§B `crates/warp_local_agent`) and feat-050 (§C local transport) are done and committed; feat-051 (§D) and feat-052 (§E) are not started.

## Branch

- `daniel/dev` (feat-048, feat-049 and feat-050 are committed; nothing pushed)

## Current State

- Runtime: client-deferred tool calls land as `ToolCallsDeferred` + `AwaitingClientToolResults`; envelope lives in `local_agent_runtime::transcript`.
- Service: `cargo run -p warp_local_agent -- --listen 127.0.0.1:9377` serves `/ai/multi-agent`, `/ai/passive-suggestions`, `/health`, `/debug/spec`. 115 tests pass.
- Transport: `warp_multi_agent_client::generate_local_agent_output(client, base_url, request)` is ready and tested; nothing calls it yet.
- App: only two match arms changed (`agent_viz/model.rs`, bridge `event_mapper`); the in-process path still works (61 `local_runtime` tests pass) and nothing routes to the service yet.
- Environment notes: run cargo with `RUSTC_WRAPPER=` (sccache remote cache down); local clippy/rustfmt are 1.97 (no rustup).

## Recommended Next Step

Implement feat-051 (TECH.md §D). Start with `app/src/ai/agent/api/impl.rs`: drop the early return at L26-33, and when `params.ollama_config` is set, point `settings.custom_model_providers` at one `CustomModelProvider` whose `models[].config_key` matches `model_config.base`, clear `api_keys`, then call `generate_local_agent_output`. The deletions in `response_stream.rs` and the `local_*` modules follow. This is the first step that edits and deletes under `app/`.
