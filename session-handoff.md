# Local Agent Session Handoff

## Current Objective

Extract the in-process local Ollama agent into a standalone `warp-local-agent` service that speaks Warp's multi-agent protobuf over HTTP+SSE. Spec: `specs/local-agent-service/TECH.md`.

## Last Updated

2026-09-22

## Active Feature

None. feat-048 through feat-051 are done and committed, so Sections A through D of the spec are complete. feat-052 (§E: the `make agent` targets and docs) is not started.

## Branch

- `daniel/dev` (Sections A through D committed; nothing pushed)

## Current State

- Runtime: client-deferred tool calls land as `ToolCallsDeferred` + `AwaitingClientToolResults`; envelope lives in `local_agent_runtime::transcript`.
- Service: `cargo run -p warp_local_agent -- --listen 127.0.0.1:9377` serves `/ai/multi-agent`, `/ai/passive-suggestions`, `/health`, `/debug/spec`. 115 tests pass.
- The in-process path no longer exists. Every Ollama turn goes to the service, whose URL comes from
  Settings › AI, the `WARP_LOCAL_AGENT_URL` override, or the `127.0.0.1:9377` default.
- The service must be running or agent turns fail with an error naming the URL and how to start it.
- The agent visualization is fed from the response stream and the action model, so it works for
  cloud runs as well as local ones.
- Environment notes: run cargo with `RUSTC_WRAPPER=` (sccache remote cache down); local clippy/rustfmt are 1.97 (no rustup).

## Recommended Next Step

Rebuild the app (`RUSTC_WRAPPER= ./script/bundle --channel oss --debug --nouniversal --selfsign
--skip-dmg`) and run one GUI turn against the service, since the deletions changed the path that
turn takes. Then feat-052 (TECH.md §E): add `make agent` and `make agent-release`, and rewrite
`dan_docs/how/local_ollama_runtime_tools.md`, which still describes the in-process path and lists
five tools where the service advertises twelve.
