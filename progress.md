# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-30  
**Active Feature:** `feat-020` — Durable local plan and todo state  
**Status:** Done  

## What's Done

- Phase A; Phase C early (009–012); Phase B (013–018); feat-019 web tools.
- **feat-020:** `update_todos` / `mark_todos_completed` with in-process state, UpdateTodos task messages, hydrate from history, prompt injection.

## Next

Phase C remaining:
1. OpenAI-compat polish  
2. model-specific prompt packs  
3. multimodal  
4. Git workflow helpers  

## Verification

- local_todos: 2 passed  
- local_runtime: 44 passed  
- format --check: passed  
