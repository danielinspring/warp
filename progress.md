# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-08-05  
**Active Feature:** (none — feat-047 complete)  
**Status:** Idle  

## What's Done

- Through feat-046: Guest `/agent`, Agent Mode mirroring/markdown, ordered join replay, and plain follow-ups while Agent View is open.
- feat-047: Agent tool-approval mirrored to guests.
  - `BlocklistAIActionModel` exposes `action_awaiting_confirmation`, `conversation_awaiting_confirmation` and `resolve_action_awaiting_confirmation`.
  - `LocalShareAgentExchange` carries `pending_action` (action id, kind, title, detail) and now formats output *with* the action model, so tool-call results appear in the mirrored transcript (capped at 32 KiB).
  - `TerminalView` republishes the owning turn on `ActionBlockedOnUserConfirmation` / `ExecutingAction` / `FinishedAction`, since approval state is not part of the exchange transcript.
  - The lite viewer renders a Reject / Run card; a requested command stays editable, an MCP call or edit list does not.
  - Guests answer over a local-share-only `LocalShareAgentDecision` envelope, parsed ahead of `UpstreamMessage`. Stale or duplicate decisions are no-ops.

## Verification (feat-047)

- `cargo nextest run -p warp local_session_share`: 43 passed, 0 failed
- `node app/src/terminal/local_session_share/lite_viewer_tests.js`: all checks passed (12 new approval checks)
- `./script/format`: clean
- `cargo clippy -p warp --lib --all-features`: no new warnings in touched files

## Next

Rebuild WarpOss with `./script/bundle`, then dogfood: open the share, run `/agent find out what's this repo` from the browser, and press **Run** on the approval card that appears — the host should execute the command and the card should disappear on both sides.
