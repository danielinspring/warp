//! Local LAN / Tailscale live session share (PR1: hub + secret + LAN bind +
//! axum HTTP(+WS stub)).
//!
//! This module hosts a host-local HTTP+WS server, gated by a URL secret,
//! that guests on the same LAN or Tailscale network can open in Chrome
//! without going through Warp cloud. See `specs/local-lan-session-share/`
//! for the full product and technical specs.
//!
//! This first PR lands [`LocalSessionShareHub`] lifecycle (start/stop/rotate),
//! the secret and bind-address plumbing, and a placeholder HTML page plus a
//! WebSocket auth stub. The Warp WASM viewer and the real event-stream
//! protocol land in follow-up PRs (see TECH.md's PR2/PR3).

mod bind;
mod hub;
mod secret;
mod server;

pub use bind::{is_all_interfaces, non_loopback_candidates, BindCandidate};
pub use hub::{HubError, LocalSessionShareHub, ShareHandle};
pub use secret::ShareSecret;
