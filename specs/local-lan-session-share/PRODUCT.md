# Local LAN / Tailscale live session share (Chrome WASM)

## Summary

A host running the Warp desktop app can share one live terminal pane over the local network (LAN or Tailscale) via an HTTP URL that opens in Chrome. Guests load a Warp WASM viewer served from the host, see that pane update in realtime, and in v1 cannot type into or control the host PTY. Access is gated only by a secret embedded in the URL. The share exists only while the host keeps that session sharing active.

## Problem

Cloud shared sessions already let guests open `https://app.warp.dev/session/{id}` in Chrome, but they require Warp’s cloud relay and identity stack. Users on the same LAN or Tailscale network want a shareable live link that works without Warp cloud in the path, using an IP/hostname they already trust (for example `http://100.x.x.x:…` or MagicDNS), while the host’s Warp process remains the source of truth for the terminal.

## Goals

- Share one live terminal pane from desktop Warp to Chrome over LAN/Tailscale.
- Guest experience uses the Warp WASM client (not a minimal xterm-only page).
- View-only realtime collab for v1 (see live output; no guest input to the PTY).
- Auth for v1 is URL secret only (no Warp login required for the guest).
- Bind on any chosen LAN/Tailscale interface the host selects.
- Share lifetime is tied to the host: when sharing stops or the host session ends, the URL stops working.

## Non-goals

- Guest control / typing into the host PTY (deferred past v1).
- Warp account, Firebase, team ACL, or Drive-style guest management for this local path (deferred; URL secret only for now).
- Sharing an entire workspace, multiple panes, or multiple tabs as one link in v1.
- Replacing or removing cloud shared sessions (`app.warp.dev` + `sessions.app.warp.dev`).
- Exposing the share to the public internet as a first-class mode (users who bind broadly do so at their own risk; product copy must warn).
- Offline / async recording or later replay of a ended share.
- Requiring the guest to install the Warp desktop app.

## Figma

Figma: none provided

## Decisions locked for v1

1. **Fidelity:** Warp WASM viewer in Chrome.
2. **Control:** view-only first.
3. **Auth:** URL secret only.
4. **Bind:** any LAN (or Tailscale) interface the host chooses.
5. **Scope:** one terminal pane per share.

## Behavior

### Starting a share (host)

1. From a single terminal pane that is eligible to share, the host can start **Local network share** (name may be refined in UI copy) via the existing share entry points and/or Command Palette. Cloud “Share session” remains a separate action and is not removed or renamed into this feature.

2. Starting a local share is only available on the **desktop** Warp app. The WASM client cannot create a local LAN share.

3. At most **one active local share** exists per terminal pane. Starting share again on a pane that is already sharing does not create a second concurrent share; it either no-ops with the existing link still valid, or offers to **rotate** the secret (see 12). The product must not leave two valid secrets for the same pane without an explicit rotate.

4. When share starts successfully, the host sees a clear sharing-on state on that pane (badge, banner, or equivalent) and can **Copy link** and **Stop sharing**.

5. The copied link is an `http` URL of the form reachable on the chosen interface, including the secret, for example `http://<host-address>:<port>/…/<secret>` (exact path shape is an implementation detail). The link is sufficient for a guest on that network to open the session in Chrome without Warp login.

6. On start, the host chooses (or confirms) a **bind address** among local non-loopback IPv4/IPv6 interfaces, including LAN and Tailscale addresses when present. Loopback-only binding is not the default for this feature (guests on other machines could not connect). Binding to all interfaces (`0.0.0.0` / `::`) is allowed but must show an explicit warning that anyone who can reach the port and obtain the URL secret can view the live session.

7. If no suitable non-loopback interface exists, share start fails with a clear error and does not pretend to be active.

8. If the chosen port cannot be bound (in use, permission denied), share start fails with a clear error; the pane is not left in a half-shared state.

9. The host can re-copy the current link at any time while sharing is active without rotating the secret.

10. Stopping share immediately invalidates the current secret and closes guest connections. A previously copied URL must not continue to show live output after stop.

11. Closing the shared pane, closing the tab/window that owns it, or quitting Warp ends the local share the same as Stop sharing.

12. The host can **rotate link** (or stop + start) to invalidate the old secret and issue a new URL. Guests on the old URL lose access; they are not silently moved to the new secret.

### Guest join (Chrome)

13. A guest with network reachability to the host address opens the share URL in Chrome (or another modern Chromium-based browser). They do **not** need the Warp desktop app installed.

14. Opening a valid URL loads the **Warp WASM** viewer for that shared pane. The guest sees a Warp-like terminal viewing surface for the shared session (blocks / scrollback / live updates consistent with what the local share path can stream), not a bare third-party terminal widget as the primary UI.

15. v1 guests are **view-only**:
    - They see live terminal output as the host session produces it.
    - Keystrokes, paste, and other input from the guest must not be applied to the host PTY.
    - Guest UI must make view-only status obvious (banner or equivalent). Affordance that would imply control (editable input that appears to send to the host) must either be absent or clearly disabled with explanation.

16. No Warp sign-in is required to view. Knowing the URL secret is the only access check for v1.

17. Opening a URL with a missing, wrong, or revoked secret does not show session content. The guest sees a failure state (not found / unauthorized / share ended) without leaking scrollback.

18. Opening a URL after the host has stopped sharing, or when the host process is gone, shows a terminal failure state (“share ended” / “host offline”), not a hung blank app with no explanation.

19. Multiple guests may open the same valid URL concurrently. Each sees the same live view-only session. v1 does not require guest-visible presence avatars, but if presence is shown it must not imply control rights.

20. Guests can disconnect and reconnect with the same URL while the share remains active and the secret is unchanged. On reconnect they receive current catch-up view of the session (at least enough to continue following live output; exact scrollback depth may be capped but must be documented in TECH).

21. If the host resizes the terminal, guests’ view follows the host size for that shared pane (no independent guest-driven resize of the host PTY in v1).

### Live updates and fidelity

22. While sharing is active, guests receive **realtime** updates from the host pane with low enough latency for collaborative watching (interactive feel on LAN/Tailscale; brief stalls on poor links are allowed but the connection should recover or surface a reconnecting state rather than silently freezing forever).

23. WASM fidelity means the guest experience is the Warp web client configured for **session viewing**, not a redesigned minimal page. Features that exist only for cloud identity, Drive, or creating shares may be hidden or disabled in this local-share viewer mode; they must not be required for viewing to work.

24. Agent Mode / AI conversation chrome on the host pane: guests always see the live terminal content of the shared pane. Richer Agent Mode transcript UI is **best-effort** in v1 when it can ride the existing shared-session viewer event path without requiring guest Warp login or Warp cloud; missing agent chrome is not a v1 blocker if PTY/blocks stream correctly.

25. Local share does not require the guest’s browser to reach `app.warp.dev` or `sessions.app.warp.dev` for the live stream. Assets for the WASM viewer may be served from the host. If any residual CDN/asset fetch is unavoidable in an early dogfood build, TECH must list it; PRODUCT intent is host-local operation on LAN/Tailscale without Warp cloud as the realtime relay.

### Security and privacy (user-visible)

26. The share URL contains a high-entropy secret. Guessing without the link must be impractical. Secrets are never shown in host telemetry payloads in full.

27. Copying the link may show a toast such as “Sharing link copied” and should warn once (or in the share UI) that anyone on the network with the link can **view** the live session.

28. Because bind may be any LAN interface, the share UI must warn that this is intended for trusted networks (home/LAN/Tailscale), not for exposing a terminal to the open internet.

29. View-only v1 still exposes potentially sensitive terminal contents (secrets typed by the host, file contents, etc.). Host UI copy must treat “view” as sensitive, not harmless.

30. When share ends, guests lose access immediately; there is no lingering public listing of past local shares.

### Interaction with cloud shared sessions

31. A pane may use **either** cloud share or local LAN share as the active share mode for v1, not both at once on the same pane. Attempting to start the second mode while the first is active either blocks with an explanation or offers to stop the first—exact preference: **block with explanation** unless UX research prefers auto-stop.

32. Local LAN share does not create an `app.warp.dev/session/…` cloud session and does not appear in cloud session history.

### Accessibility and chrome

33. Host share controls are keyboard-reachable via Command Palette at minimum (start, copy link, stop).

34. Guest failure and view-only banners must be readable with the active theme / WASM accessibility baseline used by other Warp web surfaces.

### Failure and edge cases

35. If the host’s network interface used in the copied URL disappears (Wi-Fi change, Tailscale down) while sharing continues on another bind, Copy link must refresh to a currently reachable address or clearly indicate the old link may be stale. Active guest connections that break show reconnecting/ended states per (18)/(22).

36. Sleep/lid-close of the host machine that drops connections surfaces as host offline / reconnecting to guests; when the host wakes and sharing is still marked active, guests can reconnect with the same URL.

37. Only one pane is in scope per share link. Other panes in the same tab are not visible or controllable through that link in v1.

## Open questions (collected)

1. Exact share UI labels (“Local network share” vs “Share on LAN / Tailscale”).
2. Whether rotate-secret is a first-class button in v1 or only via stop + start (Behavior 3/12).
3. Preferred port selection UX (fixed default port vs ephemeral) — product only cares that the URL the host copies is the one guests use.

## Companion

Technical plan: [TECH.md](./TECH.md).
