use std::net::{IpAddr, Ipv4Addr};

use futures_util::{SinkExt, StreamExt};
use session_sharing_protocol::common::{
    Role, Scrollback, UserID, WindowSize, WriteToPtyRequestId, WriteToPtySeqNo,
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
    let (socket, joined, _typed_input) = join_as_viewer_with_typed_input(ws_url).await;
    (socket, joined)
}

async fn join_as_viewer_with_typed_input(
    ws_url: String,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    DownstreamMessage,
    String,
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

    // The hub always follows JoinedSuccessfully (+ durable backlog) with the
    // latest typed-input snapshot so late guests mirror in-progress typing.
    let typed_message = socket
        .next()
        .await
        .expect("hub should send typed-input snapshot")
        .expect("typed-input snapshot should not error");
    let Message::Text(typed_text) = typed_message else {
        panic!("expected text LocalShareTypedInput, got {typed_message:?}");
    };
    let typed_json: serde_json::Value =
        serde_json::from_str(typed_text.as_ref()).expect("parse LocalShareTypedInput");
    let typed_input = typed_json["LocalShareTypedInput"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    (socket, joined, typed_input)
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
fn initialize_receives_joined_successfully_as_executor() {
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
            .all(|viewer| viewer.role == Role::Executor));
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

/// A guest that opens the link some time after the share started must still
/// receive what it missed: the scrollback snapshot only covers the session up
/// to share start, so everything published since then is replayed on join.
#[test]
fn events_published_before_join_are_replayed_to_a_late_guest() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");

    hub.publish_pty_bytes(b"ran before the guest joined")
        .expect("publish should succeed");
    hub.publish_pty_bytes(b" and more")
        .expect("publish should succeed");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        let mut replayed = Vec::new();
        for _ in 0..2 {
            let message = socket
                .next()
                .await
                .expect("hub should replay the backlog")
                .expect("replay should not error");
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
            replayed.extend(lz4_flex::block::decompress_size_prepended(&bytes).unwrap());
        }
        assert_eq!(replayed, b"ran before the guest joined and more");

        // Live events still arrive exactly once after the backlog.
        hub.publish_pty_bytes(b" live")
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
        assert_eq!(
            lz4_flex::block::decompress_size_prepended(&bytes).unwrap(),
            b" live"
        );
    });

    hub.stop();
}

#[test]
fn typed_input_is_mirrored_live_and_on_late_join() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let publisher = hub.event_publisher().expect("publisher while active");

    publisher
        .publish_typed_input("pwd".to_owned())
        .expect("publish typed input");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _, typed) = join_as_viewer_with_typed_input(guest_ws_url(&handle)).await;
        assert_eq!(typed, "pwd");

        publisher
            .publish_typed_input("pwd -P".to_owned())
            .expect("publish typed input update");

        let message = socket
            .next()
            .await
            .expect("hub should broadcast typed input")
            .expect("broadcast should not error");
        let Message::Text(text) = message else {
            panic!("expected text LocalShareTypedInput, got {message:?}");
        };
        let json: serde_json::Value =
            serde_json::from_str(text.as_ref()).expect("parse LocalShareTypedInput");
        assert_eq!(json["LocalShareTypedInput"]["text"], "pwd -P");

        // Identical text is coalesced — no extra frame.
        publisher
            .publish_typed_input("pwd -P".to_owned())
            .expect("publish identical typed input");
        publisher
            .publish_typed_input("".to_owned())
            .expect("clear typed input");
        let message = socket
            .next()
            .await
            .expect("hub should broadcast cleared typed input")
            .expect("broadcast should not error");
        let Message::Text(text) = message else {
            panic!("expected text LocalShareTypedInput, got {message:?}");
        };
        let json: serde_json::Value =
            serde_json::from_str(text.as_ref()).expect("parse LocalShareTypedInput");
        assert_eq!(json["LocalShareTypedInput"]["text"], "");
    });

    hub.stop();
}

#[test]
fn replay_log_drops_oldest_frames_past_the_cap() {
    let mut log = ReplayLog::default();
    let frame = "x".repeat(LOCAL_SHARE_MAX_REPLAY_BYTES / 4);
    for _ in 0..6 {
        log.push(frame.clone());
    }

    assert!(log.bytes <= LOCAL_SHARE_MAX_REPLAY_BYTES);
    assert_eq!(log.frames.len(), 4);

    // A single oversized frame is still kept, so the newest output is never lost.
    let mut log = ReplayLog::default();
    log.push("y".repeat(LOCAL_SHARE_MAX_REPLAY_BYTES * 2));
    assert_eq!(log.frames.len(), 1);
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
fn execute_command_from_guest_is_enqueued_for_host() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let guest_rx = hub
        .take_guest_request_receiver()
        .expect("guest request receiver");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        let request = UpstreamMessage::ExecuteCommand {
            buffer_id: session_sharing_protocol::common::BufferId::from("buf".to_string()),
            command: "echo from-guest".to_string(),
        };
        socket
            .send(Message::Text(request.to_json().unwrap().into()))
            .await
            .expect("execute request should send");

        let message = socket
            .next()
            .await
            .expect("hub should ack")
            .expect("ack should not error");
        let Message::Text(text) = message else {
            panic!("expected text InFlight ack, got {message:?}");
        };
        assert!(matches!(
            DownstreamMessage::from_json(text.as_ref()).unwrap(),
            DownstreamMessage::CommandExecutionRequestInFlight(_)
        ));
    });

    let request = guest_rx
        .try_recv()
        .expect("host should receive the guest ExecuteCommand");
    match request {
        LocalShareGuestRequest::ExecuteCommand { command, .. } => {
            assert_eq!(command, "echo from-guest");
        }
        other => panic!("unexpected guest request: {other:?}"),
    }

    hub.stop();
}

#[test]
fn write_to_pty_from_guest_is_enqueued_for_host() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let guest_rx = hub
        .take_guest_request_receiver()
        .expect("guest request receiver");

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
            bytes: b"guest-bytes".to_vec(),
        };
        socket
            .send(Message::Text(request.to_json().unwrap().into()))
            .await
            .expect("write request should send");

        // WriteToPty has no ack; give the WS handler a moment to enqueue.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    let request = guest_rx
        .try_recv()
        .expect("host should receive the guest WriteToPty");
    match request {
        LocalShareGuestRequest::WriteToPty { bytes, .. } => {
            assert_eq!(bytes, b"guest-bytes");
        }
        other => panic!("unexpected guest request: {other:?}"),
    }

    hub.stop();
}

#[test]
fn agent_exchange_is_mirrored_live_and_on_late_join() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let publisher = hub.event_publisher().expect("publisher");

    // Published before anyone joins: an Agent Mode answer must still reach a
    // guest that opens the link afterwards.
    publisher
        .publish_agent_exchange(LocalShareAgentExchange {
            id: "exchange-1".to_string(),
            query: "/agent what is this repo about?".to_string(),
            output: "It is a terminal.".to_string(),
            running: true,
        })
        .expect("publish should succeed");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        let replayed = next_agent_exchange(&mut socket).await;
        assert_eq!(replayed["query"], "/agent what is this repo about?");
        assert_eq!(replayed["output"], "It is a terminal.");
        assert_eq!(replayed["running"], true);

        // A streamed update replaces the same turn rather than adding another.
        publisher
            .publish_agent_exchange(LocalShareAgentExchange {
                id: "exchange-1".to_string(),
                query: "/agent what is this repo about?".to_string(),
                output: "It is a terminal. Written in Rust.".to_string(),
                running: false,
            })
            .expect("publish should succeed");

        let updated = next_agent_exchange(&mut socket).await;
        assert_eq!(updated["id"], "exchange-1");
        assert_eq!(updated["output"], "It is a terminal. Written in Rust.");
        assert_eq!(updated["running"], false);
    });

    hub.stop();
}

#[test]
fn late_join_replays_agent_exchanges_in_publish_order() {
    let mut hub = LocalSessionShareHub::new();
    let handle = hub.start(loopback_ip(), 0).expect("start should succeed");
    let publisher = hub.event_publisher().expect("publisher");

    // An agent turn sandwiched between two PTY frames has to replay in the
    // middle, not at the end, or a guest that opens the link (or a rotated
    // link) later sees a transcript in a different order than the host's.
    publisher
        .publish_pty_bytes(b"before-agent\r\n")
        .expect("publish should succeed");
    publisher
        .publish_agent_exchange(LocalShareAgentExchange {
            id: "exchange-1".to_string(),
            query: "/agent what is this repo about?".to_string(),
            output: "It is a terminal.".to_string(),
            running: false,
        })
        .expect("publish should succeed");
    publisher
        .publish_pty_bytes(b"after-agent\r\n")
        .expect("publish should succeed");
    // A streamed update must not move the turn to the end of the replay.
    publisher
        .publish_agent_exchange(LocalShareAgentExchange {
            id: "exchange-1".to_string(),
            query: "/agent what is this repo about?".to_string(),
            output: "It is a terminal. Written in Rust.".to_string(),
            running: false,
        })
        .expect("publish should succeed");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut socket, _) = join_as_viewer(guest_ws_url(&handle)).await;

        let mut kinds = Vec::new();
        for _ in 0..8 {
            let message = socket
                .next()
                .await
                .expect("hub should send a frame")
                .expect("frame should not error");
            let Message::Text(text) = message else {
                continue;
            };
            let json: serde_json::Value =
                serde_json::from_str(text.as_ref()).expect("frame should be json");
            if json.get("LocalShareAgentExchange").is_some() {
                kinds.push("agent".to_string());
            } else if json.get("OrderedTerminalEvent").is_some() {
                kinds.push("pty".to_string());
            }
            if kinds.len() == 3 {
                break;
            }
        }

        assert_eq!(kinds, vec!["pty", "agent", "pty"]);
    });

    hub.stop();
}

/// Reads frames until the next `LocalShareAgentExchange` payload, skipping the
/// typed-input snapshot and any ordered events in between.
async fn next_agent_exchange(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    for _ in 0..16 {
        let message = socket
            .next()
            .await
            .expect("hub should send a frame")
            .expect("frame should not error");
        let Message::Text(text) = message else {
            continue;
        };
        let json: serde_json::Value =
            serde_json::from_str(text.as_ref()).expect("frame should be json");
        if let Some(payload) = json.get("LocalShareAgentExchange") {
            return payload.clone();
        }
    }
    panic!("no LocalShareAgentExchange frame arrived");
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
