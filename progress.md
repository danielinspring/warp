# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-28  
**Active Feature:** `feat-015` — Model-gated computer and document actions  
**Status:** Done  

## What's Done

- Phase A, early Phase C, feat-013/014 (run_agents + background shell).
- **feat-015:** Local bridge for `read_documents` / `edit_documents` / `create_documents`; `request_computer_use` / `use_computer` when `computer_use_enabled`; plan-mode gating; JSON results with screenshot metadata only.

## What's In Progress

- None

## Next (sequential Phase B → C)

1. skill discovery and bundled resources  
2. trusted lifecycle hooks  
3. local-runtime telemetry  
4. Phase C remainder: web search/fetch, plan/todo, provider polish, prompt packs, multimodal, Git helpers  

## Verification Evidence

- local_runtime tests: **41 passed**
- format --check: **passed**
