# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-27  
**Active Feature:** `feat-014` — Background shell and process lifecycle  
**Status:** Done  

## What's Done

- Harness + Phase A (feat-001–008)
- Phase C early (feat-009–012): LiteLLM, Qwen tools, read-only shell
- **feat-013:** Bounded local `run_agents` (orchestration_enabled, no parent, max 4, Local-only)
- **feat-014:** Background shell lifecycle — optional `wait_until_complete`, LRC read/write tools, result JSON + proto

## What's In Progress

- None

## Next (sequential)

1. model-gated computer and document actions → feat-015  
2. skill discovery and bundled resources  
3. trusted lifecycle hooks  
4. local-runtime telemetry  
5. Remaining Phase C: web search/fetch, plan/todo, provider polish, prompt packs, multimodal, Git helpers  

## Decisions

- Sequential Phase B before remaining Phase C
- Bridge existing Warp actions rather than inventing parallel subsystems
- Local run_agents depth bound via `parent_agent_id.is_none()`
- Shell defaults to wait=true for weak models; LRC tools enable intentional background work

## Verification Evidence

- local_runtime tests: **40 passed**
- format --check: **passed**
