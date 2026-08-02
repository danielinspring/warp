use std::net::{IpAddr, Ipv4Addr};

use futures_util::{SinkExt, StreamExt};
use session_sharing_protocol::common::{
    Role, Scrollback, UserID, WindowSize, WriteToPtyFailureReason, WriteToPtyRequestId,
    WriteToPtySeqNo,
};
use session_sharing_protocol::viewer::{DownstreamMessage, InitPayload, UpstreamMessage};
use tokio_tungstenite::tungstenite::Message;

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

fn guest_ws_url(handle: &ShareHandle) -> String {
    format!(
        "ws://{}/local-session/{}/ws",
        handle.addr,
        handle.secret.as_str()
    )
}

fn initialize_payload() -> UpstreamMessage {
    UpstreamMessage::Initialize(InitPayload {
        viewer_id: None,
        user_id: UserID::default(),
        last_received_event_no: None,
        latest_block_id: None,
        telemetry_context: None,
        feature_support: Default::default(),
    })
}

async fn join_as_viewer(
    ws_url: String,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    DownstreamMessage,
) {
    let (mut socket, response) = tokio_tungstenite::connect_async(ws_url)
        .await
        .expect("guest should connect with the correct secret");
    assert_eq!(response.status().as_u16(), 101);

    let init = initialize_payload().to_json().unwrap();
    socket
        .send(Message::Text(init.into()))
        .await
        .expect("initialize should send");

    let message = socket
        .next()
        .await
        .expect("hub should reply")
        .expect("reply should not error");
    let Message::Text(text) = message else {
        panic!("expected text JoinedSuccessfully, got {message:?}");
    };
    let joined = DownstreamMessage::from_json(text.as_ref()).expect("parse JoinedSuccessfully");
    (socket, joined)
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
fn correct_secret_returns_lite_viewer_html() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    assert!(!handle.has_wasm_viewer);

    let (status, body) = get(&handle.url);
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(body.contains("Local session share"));
    assert!(body.contains("connecting"));
    assert!(!body.contains("WASM viewer coming soon"));

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
fn initialize_receives_joined_successfully_as_reader() {
    let mut hub = LocalSessionShareHub::new();
    hub.set_window_size(WindowSize {
        num_rows: 24,
        num_cols: 80,
    })
    .expect_err("set_window_size requires an active share");

    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    hub.set_window_size(WindowSize {
        num_rows: 24,
        num_cols: 80,
    })
    .expect("set_window_size should work while active");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (_socket, joined) = join_as_viewer(guest_ws_url(&handle)).await;
        let DownstreamMessage::JoinedSuccessfully {
            scrollback,
            window_size,
            participant_list,
            ..
        } = joined
        else {
            panic!("expected JoinedSuccessfully");
        };
        assert!(scrollback.blocks.is_empty());
        assert_eq!(window_size.num_rows, 24);
        assert_eq!(window_size.num_cols, 80);
        assert!(participant_list
            .viewers
            .iter()
            .all(|viewer| viewer.role == Role::Reader));
    });

    hub.stop();
}

#[test]
fn set_scrollback_is_served_on_join() {
    use session_sharing_protocol::common::ScrollbackBlock;

    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    hub.set_scrollback(Scrollback {
        blocks: vec![ScrollbackBlock {
            raw: br#"{"kind":"test-block"}"#.to_vec(),
        }],
        is_alt_screen_active: true,
    })
    .expect("set_scrollback should work while active");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (_socket, joined) = join_as_viewer(guest_ws_url(&handle)).await;
        let DownstreamMessage::JoinedSuccessfully { scrollback, .. } = joined else {
            panic!("expected JoinedSuccessfully");
        };
        assert_eq!(scrollback.blocks.len(), 1);
        assert_eq!(scrollback.blocks[0].raw, br#"{"kind":"test-block"}"#);
        assert!(scrollback.is_alt_screen_active);
    });

    hub.stop();
}

#[test]
fn set_scrollback_caps_oversized_snapshot() {
    use session_sharing_protocol::common::ScrollbackBlock;

    let mut hub = LocalSessionShareHub::new();
    let _handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let block_size = 64 * 1024;
    let block_count = 8;
    let mut scrollback = Scrollback {
        blocks: (0..block_count)
            .map(|i| ScrollbackBlock {
                raw: vec![b'a' + (i as u8 % 26); block_size],
            })
            .collect(),
        is_alt_screen_active: false,
    };
    let original_bytes = scrollback.num_bytes().as_u64();
    assert!(original_bytes > 200 * 1024);

    // Cap well below the original size; oldest blocks should be dropped.
    let max_bytes = 200 * 1024;
    cap_scrollback(&mut scrollback, max_bytes);
    assert!(scrollback.num_bytes().as_u64() <= max_bytes);
    assert!(!scrollback.blocks.is_empty());
    assert!(scrollback.blocks.len() < block_count);

    hub.set_scrollback(scrollback)
        .expect("capped scrollback should be accepted");

    hub.stop();
}

#[test]
fn publish_pty_bytes_reaches_joined_guest() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        hub.publish_pty_bytes(b"hello guest")
            .expect("publish should succeed");

        let message = socket
            .next()
            .await
            .expect("hub should broadcast")
            .expect("broadcast should not error");
        let Message::Text(text) = message else {
            panic!("expected text OrderedTerminalEvent, got {message:?}");
        };
        let DownstreamMessage::OrderedTerminalEvent(event) =
            DownstreamMessage::from_json(text.as_ref()).expect("parse event")
        else {
            panic!("expected OrderedTerminalEvent");
        };
        let session_sharing_protocol::common::OrderedTerminalEventType::PtyBytesRead { bytes } =
            event.event_type
        else {
            panic!("expected PtyBytesRead");
        };
        let decoded = lz4_flex::block::decompress_size_prepended(&bytes).unwrap();
        assert_eq!(decoded, b"hello guest");
    });

    hub.stop();
}

#[test]
fn publish_command_started_event_reaches_joined_guest() {
    use session_sharing_protocol::common::OrderedTerminalEventType;

    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        hub.publish_event(OrderedTerminalEventType::CommandExecutionStarted {
            participant_id: session_sharing_protocol::common::ParticipantId::new(),
            ai_metadata: None,
        })
        .expect("publish should succeed");

        let message = socket
            .next()
            .await
            .expect("hub should broadcast")
            .expect("broadcast should not error");
        let Message::Text(text) = message else {
            panic!("expected text OrderedTerminalEvent, got {message:?}");
        };
        let DownstreamMessage::OrderedTerminalEvent(event) =
            DownstreamMessage::from_json(text.as_ref()).expect("parse event")
        else {
            panic!("expected OrderedTerminalEvent");
        };
        assert!(matches!(
            event.event_type,
            OrderedTerminalEventType::CommandExecutionStarted { .. }
        ));
    });

    hub.stop();
}

#[test]
fn write_to_pty_from_guest_is_rejected() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        let request = UpstreamMessage::WriteToPty {
            request_id: WriteToPtyRequestId {
                participant_id: session_sharing_protocol::common::ParticipantId::new(),
                op_no: WriteToPtySeqNo::zero(),
            },
            bytes: b"intrusion".to_vec(),
        };
        socket
            .send(Message::Text(request.to_json().unwrap().into()))
            .await
            .expect("write request should send");

        let message = socket
            .next()
            .await
            .expect("hub should reply")
            .expect("reply should not error");
        let Message::Text(text) = message else {
            panic!("expected text failure, got {message:?}");
        };
        assert!(matches!(
            DownstreamMessage::from_json(text.as_ref()).unwrap(),
            DownstreamMessage::WriteToPtyRequestFailed {
                reason: WriteToPtyFailureReason::InsufficientPermissions
            }
        ));
    });

    hub.stop();
}

fn write_fake_wasm_bundle(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("wasm")).unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(
        root.join("index.html"),
        "<!doctype html><html><body>fake-wasm-index-marker</body></html>",
    )
    .unwrap();
    std::fs::write(root.join("wasm/probe.txt"), "wasm-asset-ok").unwrap();
    std::fs::write(root.join("assets/probe.txt"), "static-asset-ok").unwrap();
}

#[test]
fn wasm_bundle_serves_index_and_assets() {
    let bundle = tempfile::tempdir().unwrap();
    write_fake_wasm_bundle(bundle.path());

    let mut hub = LocalSessionShareHub::new();
    let handle = hub
        .start_with_options(loopback_ip(), 0, Some(bundle.path().to_path_buf()))
        .expect("start should succeed");
    assert!(handle.has_wasm_viewer);

    let (status, body) = get(&handle.url);
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(!body.contains("WASM viewer coming soon"));
    assert!(body.contains("fake-wasm-index-marker"));

    let bad_url = format!("http://{}/local-session/not-the-secret", handle.addr);
    let (bad_status, bad_body) = get(&bad_url);
    assert_ne!(bad_status, reqwest::StatusCode::OK);
    assert!(!bad_body.contains("fake-wasm-index-marker"));

    let (wasm_status, wasm_body) = get(&format!(
        "http://{}/assets/client/wasm/probe.txt",
        handle.addr
    ));
    assert_eq!(wasm_status, reqwest::StatusCode::OK);
    assert_eq!(wasm_body, "wasm-asset-ok");

    let (static_status, static_body) = get(&format!(
        "http://{}/assets/client/static/probe.txt",
        handle.addr
    ));
    assert_eq!(static_status, reqwest::StatusCode::OK);
    assert_eq!(static_body, "static-asset-ok");

    hub.stop();
}

#[test]
fn boot_json_returns_ws_url_for_valid_secret() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    let boot_url = format!(
        "http://{}/local-session/{}/boot.json",
        handle.addr,
        handle.secret.as_str()
    );
    let (status, body) = get(&boot_url);
    assert_eq!(status, reqwest::StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).expect("boot.json should be JSON");
    assert_eq!(
        json["ws_url"].as_str().unwrap(),
        format!(
            "ws://{}/local-session/{}/ws",
            handle.addr,
            handle.secret.as_str()
        )
    );
    assert_eq!(json["secret"].as_str().unwrap(), handle.secret.as_str());

    let bad_boot = format!("http://{}/local-session/wrong/boot.json", handle.addr);
    let (bad_status, _) = get(&bad_boot);
    assert_ne!(bad_status, reqwest::StatusCode::OK);

    hub.stop();
}
