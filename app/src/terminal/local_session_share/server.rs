use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use session_sharing_protocol::viewer::{DownstreamMessage, UpstreamMessage};
use tower_http::services::ServeDir;

use super::hub::ShareState;
use super::protocol::{joined_successfully, reply_for_upstream};

const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Warp local session share</title>
  </head>
  <body>
    <h1>Local session share (view-only hub active)</h1>
    <p>WASM viewer coming soon.</p>
  </body>
</html>
"#;

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

    Html(PLACEHOLDER_HTML).into_response()
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
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<ShareState>) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.subscribe_events();
    let mut joined = false;

    loop {
        tokio::select! {
            inbound = stream.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(message) = UpstreamMessage::from_json(text.as_ref()) else {
                            continue;
                        };
                        if !joined {
                            if let UpstreamMessage::Initialize(_) = &message {
                                let reply =
                                    joined_successfully(state.window_size(), state.scrollback());
                                if send_downstream(&mut sink, reply).await.is_err() {
                                    return;
                                }
                                joined = true;
                            }
                            continue;
                        }
                        if let Some(reply) = reply_for_upstream(message) {
                            if send_downstream(&mut sink, reply).await.is_err() {
                                return;
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
            event = events.recv(), if joined => {
                match event {
                    Ok(json) => {
                        if sink.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Skip lagged frames; a follow-up may send SessionEnded.
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
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
