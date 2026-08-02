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
- Joining over plain http on the LAN no longer throws: `crypto.randomUUID` (secure-context only) is replaced by a `getRandomValues` UUID fallback, and an inline data-URI favicon removes the `/favicon.ico` 404.
- A join overlay shows staged percent, a bar and elapsed seconds, ending with `done / total blocks` while the scrollback replays in animation-frame slices.
- Guests that open the link after the share started now get the history they missed: the hub keeps an 8 MiB replay log of published frames and sends it after `JoinedSuccessfully`, before the live stream. Pre-share history still comes from the start-time scrollback snapshot, whose size is now logged.
- Host typing is mirrored live as `LocalShareTypedInput` plain-text snapshots into the guest footer (Warp's editor does not echo through PTY), rendered inside the last prompt row so it continues on the prompt's line.
- Full-screen apps (`claude`, `vim`, `top`) are mirrored: the guest switches to a real alternate-screen grid sized to the host's cols x rows, and returns to blocks when the app exits.
- Stuck SGR 4 underlines from Claude Code are not painted as CSS text-decoration (native terminals hide them under glyph ink; CSS was ruling every row).
- Pre-share history is paginated: newest ~3 blocks mount on join; scroll up loads ~5 older blocks until the beginning of shared history. The snapshot now keeps restored blocks (`shared_session::local_share_scrollback`), so a guest sees the history the host has on screen after an app restart.

## Recommended Next Step

In WarpOss: Start **Local Share** on a session with many prior blocks, open the link, confirm only the latest few show, then scroll up (or click the top marker) until the beginning marker.
