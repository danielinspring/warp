# Local Agent Session Handoff

## Current Objective

Local LAN share guests can watch the host session, run commands from the browser, and follow Agent Mode answers.

## Last Updated

2026-08-03

## Active Feature

(none)

## Branch

- `cla-dev-2`

## Current State

- Share URL serves the built-in guest viewer (WS join + LZ4 PTY) when no WASM bundle is staged.
- Guest renders Warp-style blocks, alt-screen grid, history pagination, and restored-block scrollback.
- Guests join as **Executor**. They type in a dedicated `#guestbar` command line that render passes never re-parent; `#typed` is only a read-only mirror of the host's input editor.
- Enter runs the line when the host is idle, or writes it to the running command's stdin when one is active.
- Agent Mode turns are mirrored as plain text (`LocalShareAgentExchange`), published on `UpdatedStreamingExchange` and replayed to late joiners; the viewer renders each turn as markdown (headings, lists, code) and updates it in place as it streams.
- Guests can start Agent Mode themselves: `Input::submit_line_on_behalf_of_shared_session_participant` runs `detect_command` first, so `/agent …` and skill commands go to the AI stack and everything else still runs in the shell.
- Block order survives join and link rotation: `ReplayLog` stamps PTY frames and agent snapshots with one monotonic sequence and replays them merged, and the viewer holds incoming messages while the first scrollback ingest is still mounting history.
- Agent prompt / control upstream messages from guests stay rejected — guests reach Agent Mode through `ExecuteCommand` routing, not `SendAgentPrompt`. Link = auth for execute (toasts say so).
- WarpOss rebuilt with these changes.

## Recommended Next Step

Dogfood the guest-initiated path: join the share in a browser, type `/agent what is this repo about?` in the guest bar, then rotate the link and rejoin to confirm the agent block replays in the same position the host shows.
