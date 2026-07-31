//! Host-side helpers for local LAN session share (bind selection, status,
//! user-facing copy). Command Palette / TerminalView wiring lives in the
//! terminal view layer and calls into these helpers.

use std::net::IpAddr;

use super::bind::non_loopback_candidates;

pub const COPY_LOCAL_SHARE_LINK_TEXT: &str = "Local share link copied";
pub const LOCAL_SHARE_ACTIVE_TOAST: &str =
    "Local network share active — anyone with the link on this network can view";
pub const LOCAL_SHARE_START_FAILED_TOAST: &str = "Could not start local network share";
pub const LOCAL_SHARE_CLOUD_BLOCK_TOAST: &str =
    "Stop cloud session sharing before starting a local network share";
pub const LOCAL_SHARE_BLOCKS_CLOUD_TOAST: &str =
    "Stop local network share before starting a cloud shared session";

/// Keymap / palette gating for whether this pane has an active local share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalLanShareStatus {
    #[default]
    Inactive,
    Active,
}

impl LocalLanShareStatus {
    pub fn as_keymap_context(&self) -> &'static str {
        match self {
            Self::Inactive => "LocalLanShareStatus_Inactive",
            Self::Active => "LocalLanShareStatus_Active",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostUxError {
    #[error("no suitable non-loopback network interface for local session share")]
    NoBindCandidate,
}

/// Picks a default bind IP for a new local share: first non-loopback IPv4 if
/// present, otherwise the first non-loopback candidate (PRODUCT.md P6–P7).
pub fn preferred_bind_ip() -> Result<IpAddr, HostUxError> {
    let candidates = non_loopback_candidates();
    candidates
        .iter()
        .find(|candidate| candidate.addr.is_ipv4())
        .or_else(|| candidates.first())
        .map(|candidate| candidate.addr)
        .ok_or(HostUxError::NoBindCandidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_context_strings_are_stable() {
        assert_eq!(
            LocalLanShareStatus::Inactive.as_keymap_context(),
            "LocalLanShareStatus_Inactive"
        );
        assert_eq!(
            LocalLanShareStatus::Active.as_keymap_context(),
            "LocalLanShareStatus_Active"
        );
    }
}
