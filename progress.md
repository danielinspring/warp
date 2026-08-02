# Local Agent Harness Progress

## Current State

**Last Updated:** 2026-08-02  
**Active Feature:** (none — feat-040 complete)  
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

- feat-036: The guest page now works on a plain-http LAN origin and shows join progress.
  - `crypto.randomUUID` is unavailable outside a secure context, which threw on join; the `Initialize` anonymous id now comes from a `randomUuid()` fallback built on `getRandomValues` (then `Math.random`).
  - An inline SVG data-URI favicon stops the browser's `/favicon.ico` 404.
  - A join overlay reports staged progress with a percentage, a bar and a 100ms elapsed timer: fetching session info (10%), opening connection (30%), joining (45%), waiting for host (60%), then scrollback replay 70→100% labelled `done / total blocks`.
  - Scrollback replay is chunked into ~12ms animation-frame slices so a large snapshot reports progress instead of freezing the tab.
  - Join failure, host session end and reconnect backoff now update the overlay rather than leaving it pinned.

- feat-037: A guest joining after the share started now receives the history it missed.
  - Root cause: `tokio::sync::broadcast` only delivers to receivers that already exist, and the WS handler subscribed at socket upgrade. Combined with a scrollback snapshot frozen at share start, everything the host ran between "Start local share" and the guest opening the link was dropped on the floor.
  - `ShareState` now keeps a `ReplayLog` of published downstream frames, capped at `LOCAL_SHARE_MAX_REPLAY_BYTES` (8 MiB) by dropping the oldest frames and always keeping the newest.
  - `ShareState::join` snapshots the backlog and subscribes under the same lock `publish_downstream` holds, so the join boundary has no gap and no duplicates.
  - The WS handler is two-phase: wait for `Initialize`, send `JoinedSuccessfully`, replay the backlog, then stream live. Scrollback (pre-share) and replay log (post-share) do not overlap.
  - Share start logs the scrollback snapshot's block count and byte size (never contents), so an empty pre-share snapshot is diagnosable from the log.

- feat-038: Host typing is mirrored live to the lite viewer before Enter.
  - Warp's input editor does not echo through the PTY, so guests only saw executed commands.
  - Host publishes coalesced `LocalShareTypedInput` plain-text snapshots (not stored in the durable PTY replay log); late joiners get the latest value right after `JoinedSuccessfully`.
  - Lite viewer footer shows `#typed` next to the prompt caret; Preexec clears it.
  - The typed text and caret are re-parented into the prompt's last row on every render pass, so input continues on the prompt's line instead of wrapping to its own.

- feat-039: Full-screen host applications are mirrored to the guest.
  - Alt-screen output addresses cells absolutely (CUP, scroll regions, line inserts), which the block model cannot express, so it used to be dumped into a scratch buffer behind a "not mirrored" notice.
  - The viewer now has a `Screen` grid emulator: fixed rows x cols, DECSTBM scroll region, IL/DL/ICH/DCH/ECH, ED/EL that erase with the active background, deferred right-margin wrap, ESC D/E/M/7/8, and DECTCEM cursor visibility rendered as an inverted cell.
  - `?1049h` / `?1047h` / `?47h` swaps the whole view to the grid (blocks, footer and jump button hide) and back out on exit, leaving a note on the owning block that its output was not captured.
  - Grid geometry follows the host's `JoinedSuccessfully` / `Resize` window size, and the font auto-scales from a measured monospace ratio so the host's grid fits the guest viewport.

- feat-040: Stuck SGR underlines from full-screen apps no longer rule every guest row.
  - Claude Code leaves SGR 4 set for the whole UI; Warp hides it inside the cell under glyph ink, but CSS `text-decoration` sat below the baseline.
  - The lite viewer keeps tracking underline for style identity but no longer emits underline CSS (strikethrough unchanged).

## Verification (feat-036–040)

- `node app/src/terminal/local_session_share/lite_viewer_tests.js`: 22 checks passed (alt-screen grid + underline suppression)  
- `cargo test -p warp local_session_share --lib`: 34 passed (typed-input live + late-join)  
- `./script/format --check`: passed  
- `cargo bundle --profile dev --bin warp-oss --target aarch64-apple-darwin` from `app/`: bundled `WarpOss.app`; relaunched.

## Next

Dogfood: Start Local Share → open the link → run `claude` on the host → confirm the guest grid has no hairline under every row.
