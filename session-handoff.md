# Local Agent Session Handoff

## Current Objective

Extract the in-process local Ollama agent into a standalone `warp-local-agent` service that speaks Warp's multi-agent protobuf over HTTP+SSE. Spec: `specs/local-agent-service/TECH.md`.

## Last Updated

2026-09-22

## Active Feature

None. feat-048 through feat-052 are done and committed, so the extraction described in the spec is complete.

## Branch

- `daniel/dev` (the whole extraction committed across 9 commits; nothing pushed)

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
--skip-dmg`), start the service with `make agent`, and run one GUI turn, since the deletions changed
the path a turn takes and only the protocol has been re-verified since. After that the spec's own
follow-ups are what remain: auto-spawning the service and bundling it in `script/macos/bundle`, and
supporting non-Ollama OpenAI-compatible providers through the same `custom_model_providers` field.
