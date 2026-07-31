use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::hub::ShareState;

/// Sent as the first message on a freshly-upgraded WebSocket connection so
/// guests (or tests) can confirm the hub is alive. The real event stream
/// protocol is added in a follow-up PR.
pub(crate) const WS_HELLO_MESSAGE: &str = "warp-local-session-share:hello";

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

/// Builds the axum router for a local session share hub. Every route other
/// than `/health` is gated by the URL secret embedded in `state`
/// (PRODUCT.md P17, P26).
pub(crate) fn build_router(state: Arc<ShareState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/local-session/{secret}", get(serve_placeholder))
        .route("/local-session/{secret}/ws", get(ws_upgrade))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn serve_placeholder(
    State(state): State<Arc<ShareState>>,
    Path(secret): Path<String>,
) -> Response {
    if state.check_secret(&secret) {
        Html(PLACEHOLDER_HTML).into_response()
    } else {
        // Do not leak whether a share ever existed; a stopped or unknown
        // secret both look like "not found" (PRODUCT.md P17, P18).
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn ws_upgrade(
    State(state): State<Arc<ShareState>>,
    Path(secret): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.check_secret(&secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    if socket
        .send(Message::Text(WS_HELLO_MESSAGE.into()))
        .await
        .is_err()
    {
        return;
    }

    // Protocol shim (scrollback + ordered terminal events) lands in a
    // follow-up PR. For now we just keep the connection open and drain
    // inbound messages, ignoring any content sent by the guest since v1
    // guests are view-only (PRODUCT.md P15).
    while let Some(Ok(_message)) = socket.recv().await {}
}
