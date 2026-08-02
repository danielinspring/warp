# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-08-03  
**Active Feature:** (none — feat-043 complete)  
**Status:** Idle  

## What's Done

- Through feat-041: Local share history pagination + restored-block snapshot.
- feat-042: Local-share guests can run commands (not view-only).
  - Guests join as `Role::Executor`; hub reverse channel delivers `LocalShareGuestRequest::{ExecuteCommand, WriteToPty}` to the host pane.
  - Host applies ExecuteCommand via `try_execute_command_on_behalf_of_shared_session_participant`; WriteToPty only while alt-screen or long-running.
  - Agent prompts and control actions remain rejected; toasts disclose that link holders can run commands.

- feat-043: The guest can actually type, and Agent Mode is mirrored.
  - Typing was impossible in practice: `#typed` is re-parented into the prompt's last row on every render (moving a focused contenteditable node blurs it), the host mirror overwrote the draft, and editability was gated on having seen a `Precmd` hook.
  - The guest now types in `#guestbar`/`#ginput`, a sibling of the footer that no render pass touches. `#typed` went back to being a read-only mirror of host typing.
  - Enter runs the line when the host is idle, and writes `line + CR` to the running command's stdin when one is active. Ctrl+<letter> is forwarded as a control byte.
  - `ExecuteCommand` no longer refuses to send when the guest was never told a buffer id — the hub routes on the socket's participant.
  - Agent Mode turns never reach guests over the PTY (they are native Warp UI, not terminal output), so the host publishes `LocalShareAgentExchange` plain text (query + flattened output + running) keyed by exchange id, from `TerminalView::handle_ai_history_model_event` on `BlocklistAIHistoryEvent::UpdatedStreamingExchange`.
  - The hub retains the latest snapshot per turn (capped at 64) and replays them after the PTY backlog, so a late guest sees the conversation. The viewer renders one agent block per exchange, updated in place as it streams.

## Verification (feat-043)

- `cargo test -p warp local_session_share --lib`: 38 passed (new `agent_exchange_is_mirrored_live_and_on_late_join`)
- `node app/src/terminal/local_session_share/lite_viewer_tests.js`: 46 checks passed
- `./script/format --check`: passed
- `cargo build --profile dev --bin warp-oss`: ok; binary copied into `WarpOss.app`, re-signed ad-hoc, relaunched

## Next

Dogfood: open the share link, type a command in the guest bar and press Enter, then run `/agent …` on the host and confirm the answer streams into the guest.
