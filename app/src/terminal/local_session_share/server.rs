use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use session_sharing_protocol::viewer::{DownstreamMessage, SessionEndedReason, UpstreamMessage};
use tower_http::services::ServeDir;

use super::hub::{ShareState, typed_input_message_json};
use super::protocol::{UpstreamDisposition, handle_upstream, joined_successfully};

/// Guest page served when no Warp WASM bundle is staged. Speaks the real
/// session-sharing-protocol WS dialect and rebuilds Warp's block UI in the
/// browser: scrollback `SerializedBlock`s are rendered directly, and live PTY
/// output is cut into blocks using the shell hooks (Preexec / CommandFinished /
/// Precmd) that ride along in the stream.
const LITE_VIEWER_HTML: &str = include_str!("lite_viewer.html");

#[derive(Serialize)]
struct BootConfig {
    ws_url: String,
    secret: String,
}

/// Builds the axum router for a local session share hub. Every route other
/// than `/health` (and static WASM assets when configured) is gated by the
/// URL secret embedded in `state` (PRODUCT.md P17, P26).
pub(crate) fn build_router(state: Arc<ShareState>) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/local-session/{secret}", get(serve_session_page))
        .route("/local-session/{secret}/boot.json", get(serve_boot_json))
        .route("/local-session/{secret}/ws", get(ws_upgrade));

    if let Some(dir) = state.wasm_bundle_dir() {
        if dir.join("index.html").is_file() {
            router = router
                .nest_service("/assets/client/wasm", ServeDir::new(dir.join("wasm")))
                .nest_service("/assets/client/static", ServeDir::new(dir.join("assets")));
        }
    }

    router.with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn serve_session_page(
    State(state): State<Arc<ShareState>>,
    Path(secret): Path<String>,
) -> Response {
    if !state.check_secret(&secret) {
        // Do not leak whether a share ever existed; a stopped or unknown
        // secret both look like "not found" (PRODUCT.md P17, P18).
        return StatusCode::NOT_FOUND.into_response();
    }

    if let Some(dir) = state.wasm_bundle_dir() {
        let index_path = dir.join("index.html");
        if index_path.is_file() {
            match tokio::fs::read_to_string(&index_path).await {
                Ok(html) => return Html(html).into_response(),
                Err(err) => {
                    log::error!("Failed to read local share WASM index.html: {err}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
    }

    Html(LITE_VIEWER_HTML).into_response()
}

async fn serve_boot_json(
    State(state): State<Arc<ShareState>>,
    Path(secret): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if !state.check_secret(&secret) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| uri.authority().map(|authority| authority.to_string()))
        .unwrap_or_else(|| "localhost".to_owned());

    let boot = BootConfig {
        ws_url: format!("ws://{host}/local-session/{secret}/ws"),
        secret,
    };
    Json(boot).into_response()
}

async fn ws_upgrade(
    State(state): State<Arc<ShareState>>,
    Path(secret): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.check_secret(&secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, secret))
}

fn session_ended_message() -> DownstreamMessage {
    DownstreamMessage::SessionEnded {
        reason: SessionEndedReason::EndedBySharer,
    }
}

async fn handle_socket(socket: WebSocket, state: Arc<ShareState>, secret: String) {
    let (mut sink, mut stream) = socket.split();
    let mut generation = state.subscribe_generation();
    let joined_generation = *generation.borrow();
    // Re-check after subscribe so a rotate between HTTP upgrade and this task
    // cannot leave an Executor socket authorized under the new generation.
    if !state.guest_still_authorized(&secret, joined_generation) {
        let _ = send_downstream(&mut sink, session_ended_message()).await;
        return;
    }

    // Join phase: nothing is streamed to a guest before it identifies itself,
    // so subscribing only once `Initialize` arrives loses nothing — and lets
    // the backlog snapshot and the subscription be taken atomically.
    let (mut events, viewer_id) = loop {
        tokio::select! {
            inbound = stream.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        if !state.guest_still_authorized(&secret, joined_generation) {
                            let _ = send_downstream(&mut sink, session_ended_message()).await;
                            return;
                        }
                        let Ok(UpstreamMessage::Initialize(_)) =
                            UpstreamMessage::from_json(text.as_ref())
                        else {
                            continue;
                        };
                        let (reply, viewer_id) =
                            joined_successfully(state.window_size(), state.scrollback());
                        if send_downstream(&mut sink, reply).await.is_err() {
                            return;
                        }
                        let snapshot = state.join();
                        if !state.guest_still_authorized(&secret, joined_generation) {
                            let _ = send_downstream(&mut sink, session_ended_message()).await;
                            return;
                        }
                        // Typed-input snapshot first so the guest footer can update
                        // while a large scrollback backlog is still flushing.
                        if let Ok(json) = typed_input_message_json(&snapshot.typed_input) {
                            if sink.send(Message::Text(json.into())).await.is_err() {
                                return;
                            }
                        }
                        // PTY frames and Agent Mode turns are already merged in publish
                        // order, so the guest rebuilds the same block order the host has.
                        for frame in snapshot.backlog {
                            if !state.guest_still_authorized(&secret, joined_generation) {
                                let _ = send_downstream(&mut sink, session_ended_message()).await;
                                return;
                            }
                            if sink.send(Message::Text(frame.into())).await.is_err() {
                                return;
                            }
                        }
                        break (snapshot.events, viewer_id);
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }
            changed = generation.changed() => {
                if changed.is_err()
                    || !state.guest_still_authorized(&secret, joined_generation)
                {
                    let _ = send_downstream(&mut sink, session_ended_message()).await;
                    return;
                }
            }
        }
    };

    loop {
        tokio::select! {
            inbound = stream.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        if !state.guest_still_authorized(&secret, joined_generation) {
                            let _ = send_downstream(&mut sink, session_ended_message()).await;
                            return;
                        }
                        let Ok(message) = UpstreamMessage::from_json(text.as_ref()) else {
                            continue;
                        };
                        match handle_upstream(message, &viewer_id) {
                            UpstreamDisposition::Ignore => {}
                            UpstreamDisposition::Reply(reply) => {
                                if send_downstream(&mut sink, reply).await.is_err() {
                                    return;
                                }
                            }
                            UpstreamDisposition::GuestRequest { request, ack } => {
                                // Recheck immediately before enqueue so a rotate
                                // that raced this inbound frame cannot execute.
                                if !state.guest_still_authorized(&secret, joined_generation) {
                                    let _ = send_downstream(&mut sink, session_ended_message())
                                        .await;
                                    return;
                                }
                                if let Err(err) = state.enqueue_guest_request(request) {
                                    log::warn!("Failed to enqueue local-share guest request: {err}");
                                }
                                if let Some(ack) = ack {
                                    if send_downstream(&mut sink, ack).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }
            event = events.recv() => {
                if !state.guest_still_authorized(&secret, joined_generation) {
                    let _ = send_downstream(&mut sink, session_ended_message()).await;
                    return;
                }
                match event {
                    Ok(json) => {
                        if sink.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Skip lagged frames; rotate/stop send SessionEnded instead.
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
            changed = generation.changed() => {
                if changed.is_err()
                    || !state.guest_still_authorized(&secret, joined_generation)
                {
                    let _ = send_downstream(&mut sink, session_ended_message()).await;
                    return;
                }
            }
        }
    }
}

async fn send_downstream(
    sink: &mut (impl SinkExt<Message> + Unpin),
    message: DownstreamMessage,
) -> Result<(), ()> {
    let Ok(json) = message.to_json() else {
        return Err(());
    };
    sink.send(Message::Text(json.into())).await.map_err(|_| ())
}
