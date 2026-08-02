# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-08-02  
**Active Feature:** (none — feat-035 complete)  
**Status:** Idle  

## What's Done

- Through feat-033: Local Share footer chip + host UX.
- feat-034: Built-in HTML viewer for local share URLs when no WASM bundle is staged; auto-discover `WARP_LOCAL_SHARE_WASM_DIR` or `Resources/local_share_wasm` for full Warp WASM.
- feat-035: The guest page is now block-based like Warp instead of a raw PTY dump.
  - Scrollback `SerializedBlock`s render with their stylized command/output, pwd, git branch, exit code and timing.
  - Live PTY is cut into blocks client-side from Warp's shell hooks (`Preexec` / `CommandFinished` / `Precmd`, DCS `ESC P $ d <hex> ST` and OSC 9278) that already ride along in the stream.
  - Hook payloads, in-band generator output (OSC 9277) and alt-screen frames no longer leak as hex/garbage text.
  - Full SGR rendering (16/256/truecolor, bold, dim, italic, underline, inverse) plus CR/BS/tab/erase handling, so progress bars and colored `ls` look right.
  - Light/dark palettes taken from `app/src/themes/default_themes.rs`; footer strip mirrors the host prompt and typed input; hover copy-command/copy-output; reconnect with backoff.

## Verification (feat-035)

- `cargo test -p warp local_session_share --lib`: 31 passed  
- `./script/format --check`: passed  
- Replayed a canned Warp PTY fixture (hooks, colored `ls`, failing `grep`, CR progress bar, `vim` alt-screen, in-band generator burst, live prompt) through the real viewer in Node and in Chrome — blocks, exit codes, colors and the prompt strip all render in both light and dark.

## Next

Dogfood on device: Start Local Share → open the copied URL on a phone/second machine → confirm blocks appear live and history is present on join.
