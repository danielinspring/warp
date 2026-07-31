use std::net::{IpAddr, Ipv4Addr};

use super::*;

fn loopback_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

/// Sends a GET request against `url` using a fresh single-thread tokio
/// runtime, returning the response status and body. The hub's own runtime
/// runs independently, so tests exercise it exactly like an external guest.
fn get(url: &str) -> (reqwest::StatusCode, String) {
    try_get(url).expect("request should complete")
}

/// Like [`get`], but returns `None` instead of panicking when the
/// connection itself fails (e.g. because the listener has been torn down).
/// Both an error response and a refused connection are valid ways for a
/// stopped/rotated share to reject a stale URL (PRODUCT.md P10, P18).
fn try_get(url: &str) -> Option<(reqwest::StatusCode, String)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let response = reqwest::get(url).await.ok()?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Some((status, body))
    })
}

#[test]
fn start_binds_ephemeral_loopback_port() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    assert!(hub.is_active());
    assert_ne!(handle.addr.port(), 0);
    assert!(handle.url.starts_with("http://127.0.0.1:"));
    assert!(handle.url.contains(handle.secret.as_str()));

    hub.stop();
}

#[test]
fn second_start_on_same_hub_fails_while_active() {
    let mut hub = LocalSessionShareHub::new();
    hub.start(loopback_ip(), 0).expect("start should succeed");

    let err = hub
        .start(loopback_ip(), 0)
        .expect_err("starting twice on the same hub should fail");
    assert!(matches!(err, HubError::AlreadyActive));

    hub.stop();
}

#[test]
fn health_endpoint_does_not_require_a_secret() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let (status, body) = get(&format!("http://{}/health", handle.addr));
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body, "ok");

    hub.stop();
}

#[test]
fn correct_secret_returns_placeholder_html() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let (status, body) = get(&handle.url);
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(body.contains("Local session share"));
    assert!(body.contains("WASM viewer coming soon"));

    hub.stop();
}

#[test]
fn wrong_secret_does_not_leak_session_content() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let bad_url = format!("http://{}/local-session/not-the-secret", handle.addr);
    let (status, body) = get(&bad_url);
    assert_ne!(status, reqwest::StatusCode::OK);
    assert!(!body.contains("Local session share"));

    hub.stop();
}

#[test]
fn stop_invalidates_the_url() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let url = handle.url.clone();

    hub.stop();
    assert!(!hub.is_active());

    match try_get(&url) {
        Some((status, _body)) => assert_ne!(status, reqwest::StatusCode::OK),
        None => {} // Connection refused is also an acceptable "stale link" outcome.
    }
}

#[test]
fn rotate_secret_invalidates_old_url_and_serves_new_one() {
    let mut hub = LocalSessionShareHub::new();
    let first = hub.start(loopback_ip(), 0).expect("start should succeed");
    let old_url = first.url.clone();

    let second = hub.rotate_secret().expect("rotate should succeed");
    assert_ne!(first.secret.as_str(), second.secret.as_str());
    assert_eq!(first.addr, second.addr);

    let (old_status, _) = get(&old_url);
    assert_ne!(old_status, reqwest::StatusCode::OK);

    let (new_status, new_body) = get(&second.url);
    assert_eq!(new_status, reqwest::StatusCode::OK);
    assert!(new_body.contains("Local session share"));

    hub.stop();
}

#[test]
fn rotate_secret_fails_when_not_active() {
    let mut hub = LocalSessionShareHub::new();
    let err = hub
        .rotate_secret()
        .expect_err("rotate on an inactive hub should fail");
    assert!(matches!(err, HubError::NotActive));
}

#[test]
fn current_handle_reflects_active_share_without_rotating() {
    let mut hub = LocalSessionShareHub::new();
    assert!(hub.current_handle().is_none());

    let started = hub.start(loopback_ip(), 0).expect("start should succeed");
    let current = hub.current_handle().expect("share should be active");
    assert_eq!(started.secret.as_str(), current.secret.as_str());
    assert_eq!(started.url, current.url);

    hub.stop();
}

#[test]
fn dropping_the_hub_stops_the_share() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let url = handle.url.clone();

    drop(hub);

    match try_get(&url) {
        Some((status, _body)) => assert_ne!(status, reqwest::StatusCode::OK),
        None => {} // Connection refused is also an acceptable "stale link" outcome.
    }
}

#[test]
fn websocket_upgrade_rejects_wrong_secret() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let ws_url = format!("ws://{}/local-session/wrong-secret/ws", handle.addr);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(tokio_tungstenite::connect_async(ws_url));

    match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 401);
        }
        other => panic!("expected an HTTP 401 handshake rejection, got {other:?}"),
    }

    hub.stop();
}

#[test]
fn websocket_upgrade_accepts_correct_secret_and_sends_hello() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let ws_url = format!(
        "ws://{}/local-session/{}/ws",
        handle.addr,
        handle.secret.as_str()
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (mut socket, response) = tokio_tungstenite::connect_async(ws_url)
            .await
            .expect("guest should be able to connect with the correct secret");
        assert_eq!(response.status().as_u16(), 101);

        let message = socket
            .next()
            .await
            .expect("hub should send a hello message")
            .expect("hello message should not be an error");
        assert_eq!(message, Message::Text(server::WS_HELLO_MESSAGE.into()));

        let _ = socket.close(None).await;
    });

    hub.stop();
}
