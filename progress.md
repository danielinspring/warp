# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-24  
**Active Feature:** `feat-009` — LiteLLM / OpenAI-compatible local provider discovery  
**Status:** Done

## What's Done

- Audited the existing harness against the five subsystems: instructions, state, verification, scope, and lifecycle.
- Confirmed project-specific truth in `AGENTS.md` and the local-agent competitive gap report.
- Defined the Phase A dependency graph and completion evidence in `feature_list.json`.
- Added all required harness artifacts and made `init.sh` executable.
- Passed structural harness validation at 100/100; all five subsystems scored 5/5.
- Implemented lossless transcript restoration for built-in, edit, MCP, resource, and skill tools.
- Implemented whole-request context budgeting, structured pair-preserving compaction, and recent-turn preservation.
- Implemented provider classification/retry, context-overflow recovery, bounded continuation, and stall detection.
- Added resumable ask-user bridging, request permission modes, and registry-derived truthful prompts.
- Passed `./script/format --check`, 29 local runtime tests, 32 focused Warp tests, and local runtime clippy.
- Passed the live 15-tool-call parity run against `bcl-2` with `qwen3-coder:latest` and a 262,144-token context window.
- Extended the Ollama settings/provider path for LiteLLM and other OpenAI-compatible proxies: normalize trailing `/v1`, fall back from `/api/tags` to `/v1/models`, and update Settings copy.

## What's In Progress

- None.

## Next

1. Rebuild/bundle the OSS app and verify Settings → Test against `http://100.95.111.65:4000` or `.../v1`.
2. Decide whether to fix the unrelated repository-wide clippy findings before PR preparation.
3. Select the next Phase B/C feature only after explicit approval.

## Blockers

- Cold macOS builds cannot compile `warpui` because the Xcode Metal Toolchain is missing.
- Warp clippy reaches pre-existing `warpui_core` `unnecessary_unwrap` failures before checking the app crate.

## Decisions

- Establish and verify the harness before implementing local runtime features.
- Keep one feature `in-progress`.
- Treat Phase A correctness and reliability as the current implementation scope.
- Keep Phase B and Phase C as scoped future work rather than expanding the active feature.
- Preserve Warp's existing action UI as the permission confirmation floor.
- Never mutate persisted history for context compaction; compact only provider-facing copies.
- Use a versioned `server_message_data` envelope to preserve exact local tool aliases and results.
- Reuse the existing Ollama local path for LiteLLM instead of adding a separate settings provider, because chat already uses OpenAI-compatible `/v1/chat/completions`.

## Files Changed

- `AGENTS.md`
- `feature_list.json`
- `progress.md`
- `init.sh`
- `session-handoff.md`
- `app/src/ai/agent/api.rs`
- `app/src/ai/local_runtime_bridge.rs`
- `app/src/ai/local_runtime_integration.rs`
- `app/src/ai/local_runtime_integration_tests.rs`
- `app/src/ai/local_runtime_spec.rs`
- `app/src/ai/ollama/mod.rs`
- `app/src/ai/ollama/mod_test.rs`
- `app/src/settings_view/ai_page.rs`
- `crates/local_agent_runtime/Cargo.toml`
- `crates/local_agent_runtime/src/config.rs`
- `crates/local_agent_runtime/src/error.rs`
- `crates/local_agent_runtime/src/lib.rs`
- `crates/local_agent_runtime/src/messages/normalize.rs`
- `crates/local_agent_runtime/src/messages/normalize_tests.rs`
- `crates/local_agent_runtime/src/provider/ollama.rs`
- `crates/local_agent_runtime/src/runtime.rs`
- `crates/local_agent_runtime/tests/runtime_tests.rs`
- `dan_docs/notes.md`

## Verification Evidence

- `validate-harness.mjs --json`: passed, 100/100; instructions/state/verification/scope/lifecycle each 5/5.
- `./script/format --check`: passed.
- `git diff --check`: passed.
- `cargo test -p local_agent_runtime`: passed (9 unit tests, 22 integration tests, 1 ignored live test, 1 ignored doc test).
- `cargo test -p warp local_runtime --lib --features local_ollama_runtime_tool_use`: passed (32 focused tests) — Phase A evidence.
- `cargo clippy -p local_agent_runtime --all-targets --tests -- -D warnings`: passed.
- Live SSH parity: passed against `bcl-2`, Ollama 0.20.2, `qwen3-coder:latest`, context 262,144; 15 sequential tool calls completed in 9.64 seconds.
- Live LiteLLM probe: `http://100.95.111.65:4000/api/tags` → 404; `/v1/models` → 200 with 20 models.
- Warp clippy: blocked by two existing `warpui_core/src/elements/gui/hoverable.rs` `unnecessary_unwrap` failures.

## Recommended Next Step

Rebuild the OSS bundle and confirm Settings Test Connection succeeds against the LiteLLM URL, then pick the next Phase B/C feature.
