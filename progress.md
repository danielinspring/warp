# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-31  
**Active Feature:** `feat-022` — Model-specific prompt and tool-schema packs  
**Status:** Done  

## What's Done

- Through feat-021 OpenAI-compat polish.
- **feat-022:** `local_runtime_model_packs` detects Qwen / DeepSeek / Llama from model id; appends prompt addenda; prefixes allowlisted tool descriptions. Generic = prior behavior.

## Next

Phase C remaining:
1. Multimodal provider support  
2. Git workflow helpers  

## Verification

- warp local_runtime: 49 passed  
- format --check: passed  
