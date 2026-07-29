# Local Agent Session Handoff

## Current Objective

Begin next Phase C item after todos (OpenAI-compat polish or model prompt packs).

## Last Updated

2026-07-30

## Active Feature

`feat-020` — Durable local plan and todo state (`done`)

## Branch

- `cla-dev-2`

## Current State

- **feat-019 done:** local web_search / web_fetch.
- **feat-020 done:** `app/src/ai/local_todos.rs` + registry/event mapper wiring.
  - Tools: `update_todos`, `mark_todos_completed` (always on, ReadOnly, kept in plan mode).
  - In-process state + `Message::UpdateTodos` for existing UI/derive.
  - Hydrate from prior task messages; prompt shows current list.

## Verification

- local_todos: 2 passed  
- local_runtime: 44 passed  
- format --check: passed  

## Recommended Next Step

Promote Phase C **OpenAI-compatible provider polish** or **model-specific prompt packs** to `feat-021` (either unblocks weaker local models / multi-provider UX).
