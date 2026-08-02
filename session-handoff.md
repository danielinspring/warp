# Local Agent Session Handoff

## Current Objective

Local LAN share guest opens a real viewer — Warp-style blocks by default (built-in HTML), full WASM when staged.

## Last Updated

2026-08-02

## Active Feature

(none)

## Branch

- `cla-dev-2`

## Current State

- Share URL serves the built-in guest viewer (WS join + LZ4 PTY) when no WASM bundle is present.
- Guest renders **blocks, not a PTY dump**: scrollback `SerializedBlock`s plus live output cut into command blocks via Warp's shell hooks (`Preexec` / `CommandFinished` / `Precmd`); ANSI colors, exit codes, cwd/git header, duration, live prompt strip.
- Hook payloads, in-band generator output (OSC 9277) and alt-screen frames no longer leak as hex garbage.
- Full Warp WASM still served from `WARP_LOCAL_SHARE_WASM_DIR` or app `Resources/local_share_wasm`.

## Recommended Next Step

In WarpOss: Start **Local Share**, open the copied link on a second device, run a few commands (including a failing one and a TUI) and confirm blocks, colors and history look right.
