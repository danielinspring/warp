use base64::Engine as _;
use base64::prelude::BASE64_URL_SAFE;
use futures::StreamExt as _;
use prost::Message as _;
use tracing_futures::Instrument as _;
use warp_core::channel::ChannelState;
use warp_server_client::base_client::{AmbientHeaderPolicy, BaseClient, TEAM_UID_HEADER};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to authenticate multi-agent request")]
    Authentication(#[source] anyhow::Error),

    #[error("Failed to resolve ambient headers for multi-agent request")]
    AmbientHeaders(#[source] anyhow::Error),

    #[error("Failed to decode base64 multi-agent response event")]
    Base64Decode(#[source] base64::DecodeError),

    #[error("Failed to decode protobuf multi-agent response event")]
    ProtobufDecode(#[source] prost::DecodeError),

    #[error("Multi-agent eventsource stream failed: {0:?}")]
    EventSource(Box<reqwest_eventsource::Error>),
}

cfg_if::cfg_if! {
    if #[cfg(target_family = "wasm")] {
        /// A multi-agent response event stream without an unnecessary `Send` bound on WASM.
        pub type OutputStream = futures::stream::LocalBoxStream<
            'static,
            Result<warp_multi_agent_api::ResponseEvent, Error>,
        >;
    } else {
        /// A multi-agent response event stream that can be sent between native threads.
        pub type OutputStream = futures::stream::BoxStream<
            'static,
            Result<warp_multi_agent_api::ResponseEvent, Error>,
        >;
    }
}

/// Opens a decoded multi-agent response event stream.
///
/// `team_uid` is the raw team UID already extracted from the caller's `TeamContext`; this
/// crate has no visibility into that type, only the wire value. See
/// `specs/multi-team-api-context/TECH.md`.
pub async fn generate_multi_agent_output(
    client: &BaseClient,
    request: &warp_multi_agent_api::Request,
    team_uid: Option<String>,
) -> Result<OutputStream, Error> {
    let auth_token = client
        .get_or_refresh_access_token()
        .await
        .map_err(Error::Authentication)?;
    let is_passive = is_passive_suggestion_request(request);
    let url = endpoint_url(is_passive);

    let mut request_builder = client
        .http_client()
        .post(url)
        .proto(request)
        .prevent_sleep("Agent Mode request in-progress");
    if let Some(token) = auth_token.as_bearer_token() {
        request_builder = request_builder.bearer_auth(token);
    }

    for (name, value) in client
        .ambient_headers(ambient_policy(is_passive))
        .await
        .map_err(Error::AmbientHeaders)?
    {
        request_builder = request_builder.header(name, value);
    }
    if let Some(team_uid) = team_uid {
        request_builder = request_builder.header(TEAM_UID_HEADER, team_uid);
    }

    let raw_stream = client.wrap_eventsource_with_iap_detection(request_builder.eventsource());
    Ok(decode_event_stream(
        raw_stream,
        tracing::info_span!(
            "generate_multi_agent_output",
            tags.cloud_agent = true,
            conversation_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            run_id = tracing::field::Empty,
        ),
    ))
}

/// Opens a decoded response event stream against a local agent service.
///
/// The service speaks the same protocol as the cloud endpoint but needs none of its plumbing:
/// there is no access token to attach, no ambient headers to resolve and no IAP redirect to
/// detect. `base_url` is the service root, such as `http://127.0.0.1:9377`.
pub async fn generate_local_agent_output(
    client: &http_client::Client,
    base_url: &str,
    request: &warp_multi_agent_api::Request,
) -> Result<OutputStream, Error> {
    let url = local_endpoint_url(base_url, is_passive_suggestion_request(request));

    let raw_stream = client
        .post(url)
        .proto(request)
        .prevent_sleep("Local agent request in-progress")
        .eventsource();

    Ok(decode_event_stream(
        raw_stream,
        tracing::info_span!(
            "generate_local_agent_output",
            tags.cloud_agent = false,
            conversation_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            run_id = tracing::field::Empty,
        ),
    ))
}

/// Decodes an SSE stream of base64 protobuf frames, recording stream identifiers on `span`.
fn decode_event_stream(
    raw_stream: http_client::EventSourceStream,
    span: tracing::Span,
) -> OutputStream {
    let output_stream = raw_stream.filter_map(|event| async {
        match event {
            Ok(reqwest_eventsource::Event::Message(message_event)) => {
                Some(decode_response_event(&message_event.data))
            }
            Ok(reqwest_eventsource::Event::Open) => None,
            Err(error) => Some(Err(Error::EventSource(Box::new(error)))),
        }
    });

    // Once we get the init event, add some identifiers to the trace span.
    let output_stream = output_stream.inspect(|event| {
        if let Ok(event) = &event {
            match &event.r#type {
                Some(warp_multi_agent_api::response_event::Type::Init(init)) => {
                    tracing::info!("StreamInit");
                    tracing::Span::current().record("conversation_id", &init.conversation_id);
                    tracing::Span::current().record("request_id", &init.request_id);
                    tracing::Span::current().record("run_id", &init.run_id);
                }
                Some(warp_multi_agent_api::response_event::Type::Finished(_finished)) => {
                    tracing::info!("StreamFinished");
                }
                _ => {}
            }
        }
    });
    let output_stream = output_stream.instrument(span);

    cfg_if::cfg_if! {
        if #[cfg(target_family = "wasm")] {
            output_stream.boxed_local()
        } else {
            output_stream.boxed()
        }
    }
}

fn is_passive_suggestion_request(request: &warp_multi_agent_api::Request) -> bool {
    request.input.as_ref().is_some_and(|input| {
        matches!(
            input.r#type,
            Some(warp_multi_agent_api::request::input::Type::GeneratePassiveSuggestions(_))
        )
    })
}

fn endpoint_url(is_passive: bool) -> String {
    format!(
        "{}/{}/{}",
        ChannelState::server_root_url(),
        if cfg!(feature = "agent_mode_evals") {
            "agent-mode-evals"
        } else {
            "ai"
        },
        if is_passive {
            "passive-suggestions"
        } else {
            "multi-agent"
        }
    )
}

/// The local service serves fixed `/ai/...` routes, so the `agent_mode_evals` prefix used for the
/// cloud endpoint does not apply here.
fn local_endpoint_url(base_url: &str, is_passive: bool) -> String {
    format!(
        "{}/ai/{}",
        base_url.trim_end_matches('/'),
        if is_passive {
            "passive-suggestions"
        } else {
            "multi-agent"
        }
    )
}

fn ambient_policy(is_passive: bool) -> AmbientHeaderPolicy {
    if is_passive {
        // Passive suggestions read from the main conversation, but cannot modify it.
        AmbientHeaderPolicy::omit_all()
    } else {
        AmbientHeaderPolicy::workload_only()
    }
}

fn decode_response_event(data: &str) -> Result<warp_multi_agent_api::ResponseEvent, Error> {
    let decoded_data = BASE64_URL_SAFE
        .decode(data.trim_matches('"'))
        .map_err(Error::Base64Decode)?;
    warp_multi_agent_api::ResponseEvent::decode(decoded_data.as_slice())
        .map_err(Error::ProtobufDecode)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
