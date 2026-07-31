# Local Agent Session Handoff

## Current Objective

Begin next Phase C item (multimodal provider support or Git workflow helpers).

## Last Updated

2026-07-31

## Active Feature

`feat-022` — Model-specific prompt and tool-schema packs (`done`)

## Branch

- `cla-dev-2`

## Current State

- **feat-021 done** (OpenAI-compat polish), committed.
- **feat-022 done:** `app/src/ai/local_runtime_model_packs.rs`
  - `detect_model_family` for qwen / deepseek / llama / generic
  - Prompt addenda via `system_prompt_for_request_with_model`
  - Schema description prefixes on `edit_files`, `run_shell_command`, `update_todos`

## Verification

- cargo test -p warp local_runtime: 49 passed  
- format --check: passed  

## Recommended Next Step

Promote Phase C **multimodal provider support** or **Git workflow helpers** to `feat-023`.
