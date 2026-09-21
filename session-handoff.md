# Local Agent Session Handoff

## Current Objective

Extract the in-process local Ollama agent into a standalone `warp-local-agent` service that speaks Warp's multi-agent protobuf over HTTP+SSE. Spec: `specs/local-agent-service/TECH.md`.

## Last Updated

2026-09-21

## Active Feature

local-agent-service extraction

## Branch

- `daniel/dev`

## Current State

- Local Ollama agent still runs inside the Warp app (`local_runtime_integration` + `local_agent_runtime`).
- Recent fix on this branch: never POST `/v1/chat/completions` without a non-empty user turn (`23403d7e`).
- Extraction plan is in `specs/local-agent-service/TECH.md` (sections A–E). In-process path is to be removed after the service is wired.

## Recommended Next Step

Next: implement specs/local-agent-service/TECH.md, sections A→E in order.
A and B can be built and tested without touching app/; do those first.
