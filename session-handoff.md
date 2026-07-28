# Local Agent Session Handoff

## Current Objective

Continue Phase B sequentially: next is **model-gated computer and document actions**.

## Last Updated

2026-07-27

## Active Feature

`feat-014` — Background shell and process lifecycle (`done`)

## Branch

- `cla-dev-2`
- Uncommitted: feat-013 + feat-014 local runtime bridge work (and format fixes)

## Current State

- Phase A: done (feat-001–008)
- Phase C early polish: done (feat-009–012)
- **feat-013 done:** bounded `run_agents` (root-only, max 4, Local mode)
- **feat-014 done:** `wait_until_complete` on shell + `read_shell_command_output` / `write_to_long_running_shell_command` LRC tools

## Verification

- `cargo test -p warp local_runtime --lib --features local_ollama_runtime_tool_use`: **40 passed**
- `./script/format --check`: **passed**

## Recommended Next Step

Implement **feat-015: model-gated computer and document actions** (bridge only when `computer_use_enabled` / document tools are safe for the local model).
