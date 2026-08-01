//! Host-side helpers for local LAN session share (bind selection, status,
//! user-facing copy). Command Palette / TerminalView wiring lives in the
//! terminal view layer and calls into these helpers.

use std::net::IpAddr;

use super::bind::{non_loopback_candidates, BindCandidate};

pub const COPY_LOCAL_SHARE_LINK_TEXT: &str = "Local share link copied";
pub const LOCAL_SHARE_ACTIVE_TOAST: &str =
    "Local network share active — anyone with the link on this network can view";
pub const LOCAL_SHARE_START_FAILED_TOAST: &str = "Could not start local network share";
pub const LOCAL_SHARE_CLOUD_BLOCK_TOAST: &str =
    "Stop cloud session sharing before starting a local network share";
pub const LOCAL_SHARE_BLOCKS_CLOUD_TOAST: &str =
    "Stop local network share before starting a cloud shared session";
pub const LOCAL_SHARE_ROTATED_TOAST: &str =
    "Local share link rotated — previous guests must use the new link";
pub const LOCAL_SHARE_ALL_INTERFACES_WARNING: &str =
    "Bound on all interfaces — anyone on any reachable network with the link can view this session";

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

/// Label for a bind candidate in menus / toasts.
pub fn bind_candidate_label(candidate: &BindCandidate) -> String {
    format!("{} ({})", candidate.interface_name, candidate.addr)
}

/// Menu / action label for binding on all IPv4 interfaces.
pub fn all_interfaces_label() -> &'static str {
    "All interfaces (0.0.0.0) — not for the public internet"
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

/// Resolves which IP to use for a palette-driven start without a picker UI.
/// Returns the sole candidate when there is exactly one; otherwise the preferred
/// IPv4/first candidate (PRODUCT.md P6–P7).
pub fn resolve_palette_bind_ip() -> Result<(IpAddr, String), HostUxError> {
    let candidates = non_loopback_candidates();
    match candidates.as_slice() {
        [] => Err(HostUxError::NoBindCandidate),
        [only] => Ok((only.addr, bind_candidate_label(only))),
        _ => {
            let preferred = preferred_bind_ip()?;
            let label = candidates
                .iter()
                .find(|candidate| candidate.addr == preferred)
                .map(bind_candidate_label)
                .unwrap_or_else(|| preferred.to_string());
            Ok((preferred, label))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

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

    #[test]
    fn bind_candidate_label_includes_iface_and_addr() {
        let candidate = BindCandidate {
            interface_name: "en0".to_owned(),
            addr: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        };
        assert_eq!(bind_candidate_label(&candidate), "en0 (192.168.1.10)");
    }

    #[test]
    fn all_interfaces_label_mentions_warning() {
        let label = all_interfaces_label();
        assert!(label.contains("0.0.0.0"));
        assert!(label.to_lowercase().contains("internet") || label.contains("public"));
    }
}
