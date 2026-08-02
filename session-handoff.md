# Local Agent Session Handoff

## Current Objective

Local LAN share guest can view the host session and run commands from the browser lite viewer.

## Last Updated

2026-08-02

## Active Feature

(none)

## Branch

- `cla-dev-2`

## Current State

- Share URL serves the built-in guest viewer (WS join + LZ4 PTY) when no WASM bundle is present.
- Guest renders Warp-style blocks, live typed input, alt-screen grid, history pagination, and restored-block scrollback.
- Guests join as **Executor** (not view-only). Footer `#typed` is contenteditable; Enter sends `ExecuteCommand` to the host.
- Hub reverse channel delivers `LocalShareGuestRequest::{ExecuteCommand, WriteToPty}` to the host pane.
- Host applies ExecuteCommand on the UI thread; WriteToPty only when alt-screen or a long-running command is active.
- Agent / control upstream messages stay rejected. Link = auth for execute (toasts say so).
- WarpOss.app relaunched with the guest-execute binary.

## Recommended Next Step

In WarpOss: Start **Local Share**, open the link, type `echo hello-from-guest` in the browser footer, press Enter, and confirm the host runs it and the guest sees the output.
