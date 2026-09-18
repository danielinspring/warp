use std::net::IpAddr;

/// A candidate local network address that a host can bind a share to,
/// reachable by other devices on the same LAN or Tailscale network
/// (PRODUCT.md P6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindCandidate {
    /// The OS-reported interface name (e.g. `en0`, `tailscale0`).
    pub interface_name: String,
    pub addr: IpAddr,
}

/// Lists non-loopback IPv4/IPv6 interface addresses suitable for a local
/// session share URL, including LAN and Tailscale addresses when present
/// (PRODUCT.md P6). Loopback-only binding is not offered as a share address
/// because guests on other machines could not reach it.
pub fn non_loopback_candidates() -> Vec<BindCandidate> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(err) => {
            log::warn!("Failed to enumerate network interfaces for local session share: {err}");
            return Vec::new();
        }
    };

    interfaces
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .map(|interface| {
            let addr = interface.ip();
            BindCandidate {
                interface_name: interface.name,
                addr,
            }
        })
        .collect()
}

/// Whether `addr` represents "all interfaces" (`0.0.0.0` / `::`). Binding to
/// all interfaces is allowed, but PRODUCT requires callers to show an
/// explicit warning that anyone who can reach the port can view the session
/// (PRODUCT.md P6, P28).
pub fn is_all_interfaces(addr: IpAddr) -> bool {
    addr.is_unspecified()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn all_interfaces_detection() {
        assert!(is_all_interfaces(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(is_all_interfaces(std::net::Ipv6Addr::UNSPECIFIED.into()));
        assert!(!is_all_interfaces(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn non_loopback_candidates_excludes_loopback() {
        // We can't assert on the exact set of interfaces in CI, but we can
        // assert the loopback address never appears in the result.
        let candidates = non_loopback_candidates();
        assert!(candidates
            .iter()
            .all(|candidate| !candidate.addr.is_loopback()));
    }
}
