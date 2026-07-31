# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-07-31  
**Active Feature:** `feat-028` — Local LAN share host UX + TerminalModel fan-in  
**Status:** In progress (just started)  

## What's Done

- Through feat-027 WASM serve + WebIntent boot.

## Verification (feat-027)

- `cargo test -p warp local_session_share --lib`: 24 passed  
- `cargo test -p warp web_intent --lib`: 4 passed  
- `./script/format --check`: passed  

## Next

Host UX: Command Palette start/copy/stop, TerminalModel → `publish_pty_bytes`, mutual exclusion with cloud share.
