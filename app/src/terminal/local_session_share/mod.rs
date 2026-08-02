//! Local LAN / Tailscale live session share (hub + secret + LAN bind +
//! session-sharing-protocol WS shim).
//!
//! This module hosts a host-local HTTP+WS server, gated by a URL secret,
//! that guests on the same LAN or Tailscale network can open in Chrome
//! without going through Warp cloud. See `specs/local-lan-session-share/`
//! for the full product and technical specs.
//!
//! Host TerminalModel fan-in and Command Palette UX live in `host.rs`.

mod bind;
mod host;
mod hub;
mod protocol;
mod secret;
mod server;

pub use bind::{is_all_interfaces, non_loopback_candidates, BindCandidate};
pub use host::{
    all_interfaces_label, bind_candidate_label, preferred_bind_ip, resolve_palette_bind_ip,
    HostUxError, LocalLanShareStatus, COPY_LOCAL_SHARE_LINK_TEXT, LOCAL_SHARE_ACTIVE_TOAST,
    LOCAL_SHARE_ALL_INTERFACES_WARNING, LOCAL_SHARE_BLOCKS_CLOUD_TOAST,
    LOCAL_SHARE_CLOUD_BLOCK_TOAST, LOCAL_SHARE_LITE_VIEWER_TOAST, LOCAL_SHARE_ROTATED_TOAST,
    LOCAL_SHARE_START_FAILED_TOAST,
};
pub use hub::{
    HubError, LocalSessionShareHub, LocalShareEventPublisher, ShareHandle,
    LOCAL_SHARE_MAX_SCROLLBACK_BYTES, WASM_BUNDLE_DIR_ENV,
};
pub use secret::ShareSecret;
