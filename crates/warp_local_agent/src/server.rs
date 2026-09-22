//! HTTP surface: the multi-agent endpoint the Warp client already speaks, plus health and debug.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::prelude::BASE64_URL_SAFE;
use futures::{Stream, StreamExt as _};
use local_agent_runtime::{CancelHandle, LifecycleHooks};
use prost::Message as _;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use warp_multi_agent_api as api;

use crate::event_mapper::{finished_event, stream_init_event};
use crate::prompt;
use crate::provider::ProviderFactory;
use crate::registry::LocalRuntimeToolRegistry;
use crate::request::{TurnIds, plan_turn};
use crate::turn::run_turn;

/// Default SSE keep-alive period; also bounds how long a client disconnect goes unnoticed.
const DEFAULT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct ServerState {
    provider_factory: Arc<dyn ProviderFactory>,
    extra_hooks: Option<Arc<dyn LifecycleHooks>>,
    keep_alive_interval: Duration,
}

impl ServerState {
    pub fn new(provider_factory: Arc<dyn ProviderFactory>) -> Self {
        Self {
            provider_factory,
            extra_hooks: None,
            keep_alive_interval: DEFAULT_KEEP_ALIVE_INTERVAL,
        }
    }

    pub fn with_keep_alive_interval(mut self, interval: Duration) -> Self {
        self.keep_alive_interval = interval;
        self
    }

    /// Attach lifecycle hooks to every run, after the built-in logging and deny-list hooks.
    pub fn with_lifecycle_hooks(mut self, hooks: Arc<dyn LifecycleHooks>) -> Self {
        self.extra_hooks = Some(hooks);
        self
    }
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/ai/multi-agent", post(multi_agent))
        .route("/ai/passive-suggestions", post(passive_suggestions))
        .route("/health", get(health))
        .route("/debug/spec", get(debug_spec))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(listener: TcpListener, state: ServerState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

async fn multi_agent(State(state): State<ServerState>, body: Bytes) -> Response {
    let request = match api::Request::decode(body) {
        Ok(request) => request,
        Err(error) => return bad_request(&error),
    };
    let ids = TurnIds::from_request(&request);
    tracing::info!(
        conversation_id = %ids.conversation_id,
        request_id = %ids.request_id,
        run_id = %ids.run_id,
        "multi-agent request"
    );
    match plan_turn(&request, ids.clone()) {
        Ok(plan) => {
            let (events, cancel) = run_turn(
                plan,
                state.provider_factory.as_ref(),
                state.extra_hooks.clone(),
            );
            sse_response(events, Some(cancel), state.keep_alive_interval)
        }
        Err(error) => {
            tracing::warn!(%error, request_id = %ids.request_id, "request rejected");
            let reason = api::response_event::stream_finished::Reason::InternalError(
                api::response_event::stream_finished::InternalError {
                    message: error.to_string(),
                },
            );
            sse_response(
                futures::stream::iter([init_event(&ids), finished_event(reason)]),
                None,
                state.keep_alive_interval,
            )
        }
    }
}

/// Passive suggestions have no local implementation; answer with an empty, successful stream.
async fn passive_suggestions(State(state): State<ServerState>, body: Bytes) -> Response {
    let request = match api::Request::decode(body) {
        Ok(request) => request,
        Err(error) => return bad_request(&error),
    };
    let ids = TurnIds::from_request(&request);
    let done = api::response_event::stream_finished::Reason::Done(
        api::response_event::stream_finished::Done {},
    );
    sse_response(
        futures::stream::iter([init_event(&ids), finished_event(done)]),
        None,
        state.keep_alive_interval,
    )
}

async fn health() -> &'static str {
    "ok"
}

/// System prompt and built-in tool schemas, for the agent visualization pane.
async fn debug_spec() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "system_prompt": prompt::system_prompt(),
        "tools": LocalRuntimeToolRegistry::built_ins().schemas(),
    }))
}

fn init_event(ids: &TurnIds) -> api::ResponseEvent {
    stream_init_event(&ids.conversation_id, &ids.request_id, &ids.run_id)
}

fn bad_request(error: &prost::DecodeError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        format!("invalid protobuf request: {error}"),
    )
        .into_response()
}

/// Cancels the run when the response body is dropped, which is how a client disconnect surfaces.
struct CancelOnDrop(CancelHandle);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn sse_response(
    events: impl Stream<Item = api::ResponseEvent> + Send + 'static,
    cancel: Option<CancelHandle>,
    keep_alive_interval: Duration,
) -> Response {
    let data = stream! {
        let _guard = cancel.map(CancelOnDrop);
        let mut events = std::pin::pin!(events);
        while let Some(event) = events.next().await {
            yield Ok::<Event, Infallible>(sse_event(&event));
        }
    };
    // Keep-alives also make hyper notice a closed socket while a slow model is still loading.
    Sse::new(data)
        .keep_alive(KeepAlive::new().interval(keep_alive_interval))
        .into_response()
}

/// The client decodes `data:` with padded URL-safe base64 (see `warp_multi_agent_client`).
pub fn sse_event(event: &api::ResponseEvent) -> Event {
    Event::default().data(BASE64_URL_SAFE.encode(event.encode_to_vec()))
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
