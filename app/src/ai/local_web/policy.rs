//! SSRF and privacy policy for local web tools.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use thiserror::Error;
use url::{Host, Url};

/// Limits applied to every outbound web request.
#[derive(Debug, Clone)]
pub struct WebPolicy {
    pub max_redirects: usize,
    pub max_response_bytes: usize,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub user_agent: String,
}

impl Default for WebPolicy {
    fn default() -> Self {
        Self {
            max_redirects: 5,
            max_response_bytes: 2_000_000,
            connect_timeout_secs: 10,
            request_timeout_secs: 20,
            user_agent: "Warp-LocalAgent/1.0 (+https://www.warp.dev; local-web-tools)".to_string(),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WebPolicyError {
    #[error("URL is not valid: {0}")]
    InvalidUrl(String),
    #[error("only http and https URLs are allowed")]
    UnsupportedScheme,
    #[error("URL must include a host")]
    MissingHost,
    #[error("access to host `{0}` is blocked by local web policy ({1})")]
    BlockedHost(String, &'static str),
}

/// Parse and validate a URL before any network I/O.
pub fn validate_http_url(raw: &str, _policy: &WebPolicy) -> Result<Url, WebPolicyError> {
    let url = Url::parse(raw).map_err(|e| WebPolicyError::InvalidUrl(e.to_string()))?;
    validate_parsed_url(&url)?;
    Ok(url)
}

pub fn validate_parsed_url(url: &Url) -> Result<(), WebPolicyError> {
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(WebPolicyError::UnsupportedScheme),
    }

    let host = url.host().ok_or(WebPolicyError::MissingHost)?;
    let display = url.host_str().unwrap_or("unknown").to_string();
    if let Some(reason) = blocked_host_reason(&host) {
        return Err(WebPolicyError::BlockedHost(display, reason));
    }
    Ok(())
}

fn blocked_host_reason(host: &Host<&str>) -> Option<&'static str> {
    match host {
        Host::Domain(name) => {
            let lower = name.to_ascii_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
                return Some("loopback/local name");
            }
            if lower == "metadata.google.internal" || lower == "metadata" {
                return Some("cloud metadata host");
            }
            // Rare: domain that is an IP literal string.
            if let Ok(ip) = name.parse::<IpAddr>() {
                return blocked_ip_reason(ip);
            }
            None
        }
        Host::Ipv4(ip) => blocked_ipv4_reason(*ip),
        Host::Ipv6(ip) => blocked_ipv6_reason(*ip),
    }
}

fn blocked_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_ipv4_reason(v4),
        IpAddr::V6(v6) => blocked_ipv6_reason(v6),
    }
}

fn blocked_ipv4_reason(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        return Some("loopback");
    }
    if ip.is_private() {
        return Some("private network");
    }
    if ip.is_link_local() {
        return Some("link-local");
    }
    if ip.is_broadcast() || ip.is_unspecified() || ip.is_multicast() {
        return Some("special-use address");
    }
    // CGNAT 100.64.0.0/10
    let o = ip.octets();
    if o[0] == 100 && (o[1] & 0xc0) == 64 {
        return Some("carrier-grade NAT");
    }
    // 0.0.0.0/8 already covered by unspecified for 0.0.0.0; block rest of 0.*
    if o[0] == 0 {
        return Some("special-use address");
    }
    None
}

fn blocked_ipv6_reason(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        return Some("loopback");
    }
    if ip.is_unspecified() || ip.is_multicast() {
        return Some("special-use address");
    }
    if ip.is_unique_local() {
        return Some("unique local");
    }
    // IPv4-mapped
    if let Some(v4) = ip.to_ipv4_mapped() {
        return blocked_ipv4_reason(v4);
    }
    // link-local fe80::/10
    let segments = ip.segments();
    if segments[0] & 0xffc0 == 0xfe80 {
        return Some("link-local");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_public_https() {
        let policy = WebPolicy::default();
        assert!(validate_http_url("https://example.com/docs", &policy).is_ok());
        assert!(validate_http_url("http://docs.rs/serde", &policy).is_ok());
    }

    #[test]
    fn denies_non_http_schemes() {
        let policy = WebPolicy::default();
        assert_eq!(
            validate_http_url("file:///etc/passwd", &policy).unwrap_err(),
            WebPolicyError::UnsupportedScheme
        );
        assert_eq!(
            validate_http_url("ftp://example.com/", &policy).unwrap_err(),
            WebPolicyError::UnsupportedScheme
        );
    }

    #[test]
    fn denies_loopback_and_private() {
        let policy = WebPolicy::default();
        for url in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://192.168.1.1/",
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.1.2/",
            "http://metadata.google.internal/",
        ] {
            let err = validate_http_url(url, &policy).unwrap_err();
            assert!(
                matches!(err, WebPolicyError::BlockedHost(_, _)),
                "expected block for {url}, got {err:?}"
            );
        }
    }
}
