# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-08-04  
**Active Feature:** (none — feat-046 complete)  
**Status:** Idle  

## What's Done

- Through feat-045: Guest `/agent`, Agent Mode mirroring/markdown, and ordered join replay.
- feat-046: Guest follow-ups while Agent View is open.
  - Non-slash guest lines call `submit_user_query_now` when `agent_view_controller.is_active()`.
  - `/agent` still always starts a new conversation (unchanged slash-command semantics).
  - Long-running PTY guard skipped for Agent View follow-ups as well as slash lines.

## Verification (feat-046)

- `cargo check -p warp --lib`: ok
- `node app/src/terminal/local_session_share/lite_viewer_tests.js`: all checks passed
- `./script/format`: clean

## Next

Rebuild WarpOss, dogfood: open share → `/agent what is this?` from the browser → then send `tell me more` without `/agent` and confirm it continues the same conversation.
