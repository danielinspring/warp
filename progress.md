# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-31  
**Active Feature:** `feat-021` — OpenAI-compatible provider polish  
**Status:** Done  

## What's Done

- Phase A; Phase C early (009–012); Phase B (013–018); feat-019 web; feat-020 todos.
- **feat-021:** OpenAI-compat polish on shared Ollama client path:
  - `ProviderError::Unauthorized` for 401/403 (non-retryable)
  - Empty API key filtering in `OllamaProvider` + `OllamaClient`
  - Discovery prefers `/v1/models` for API key, `/v1` URL, or non-`:11434` hosts
  - Settings: “Ollama / OpenAI-compatible”, LM Studio/Groq examples, persist editors on edit
  - Model picker label Ollama vs OpenAI-compatible by host
  - Legacy client accepts object-form tool arguments
  - Clippy Instant → `instant::Instant` in runtime/telemetry

## Next

Phase C remaining:
1. Model-specific prompt packs  
2. Multimodal  
3. Git workflow helpers  

## Verification

- local_agent_runtime: 24 unit + 23 integration passed  
- warp ollama: 5 passed  
- clippy -D warnings: passed  
- format --check: passed  
