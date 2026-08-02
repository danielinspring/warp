# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-08-03  
**Active Feature:** (none — feat-045 complete)  
**Status:** Idle  

## What's Done

- Through feat-044: Guest command line, Agent Mode plain-text mirroring, and markdown rendering in the lite viewer.
- feat-045: Guests can start Agent Mode from the browser, and mirrored agent turns keep the host's block order.
  - `Input::submit_line_on_behalf_of_shared_session_participant` classifies a guest line with `slash_command_model.detect_command` and dispatches `execute_slash_command` / `execute_skill_command` before falling back to the shell, so `/agent …` no longer reaches the PTY as a literal path.
  - `TerminalView::apply_local_share_guest_request` skips the long-running-command guard for slash lines, since they never touch the PTY.
  - `ReplayLog` stamps PTY frames and agent snapshots with a shared monotonic sequence; `ReplayLog::replay()` merges them so `JoinSnapshot.backlog` is one ordered stream and the server no longer flushes agent turns last.
  - The lite viewer mounts agent blocks through `view.blocks` (`mountBlockEl`) and defers downstream messages while the first scrollback ingest is animation-frame slicing, so a turn cannot land ahead of the history it follows.

## Verification (feat-045)

- `cargo test -p warp --lib local_session_share`: 39 passed, including the new `late_join_replays_agent_exchanges_in_publish_order`
- `node app/src/terminal/local_session_share/lite_viewer_tests.js`: 61 checks passed, including the new agent-block ordering checks
- `./script/format`: clean
- `cargo clippy -p warp --lib --all-features --tests`: no new warnings in the changed files (pre-existing lints remain in `protocol.rs` and older `hub_tests.rs` cases)
- `cargo build --bin warp`: succeeded

## Next

Dogfood: join the share in a browser, submit `/agent what is this repo about?` from the guest bar, then rotate the link and rejoin to confirm the agent block replays in the same position the host shows.
