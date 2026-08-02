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
- Agent Mode turns are mirrored as plain text (`LocalShareAgentExchange`), published on `UpdatedStreamingExchange` and replayed to late joiners; the viewer shows one agent block per turn, updated as it streams.
- Agent prompt / control upstream messages from guests stay rejected. Link = auth for execute (toasts say so).
- WarpOss.app rebuilt and relaunched with these changes.

## Recommended Next Step

Dogfood both paths in one session: type `echo hello-from-guest` in the guest bar and press Enter, then run `/agent what is this repo about?` on the host and confirm the streamed answer appears in the browser.
