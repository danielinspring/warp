//! Local LAN / Tailscale live session share (hub + secret + LAN bind +
//! session-sharing-protocol WS shim).
//!
//! This module hosts a host-local HTTP+WS server, gated by a URL secret,
//! that guests on the same LAN or Tailscale network can open in Chrome
//! without going through Warp cloud. See `specs/local-lan-session-share/`
//! for the full product and technical specs.
//!
//! Host TerminalModel fan-in and share UX land in follow-up work (TECH.md PR4).

mod bind;
mod hub;
mod protocol;
mod secret;
mod server;

pub use bind::{is_all_interfaces, non_loopback_candidates, BindCandidate};
pub use hub::{HubError, LocalSessionShareHub, ShareHandle, WASM_BUNDLE_DIR_ENV};
pub use secret::ShareSecret;
