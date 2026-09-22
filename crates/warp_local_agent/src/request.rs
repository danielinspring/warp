//! Turns a protobuf `Request` into everything one runtime run needs: ids, provider, registry,
//! system prompt, conversation history, this turn's user input and the tool results to echo.

use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use local_agent_runtime::messages::{
    AssistantMessage, ContentPart, ToolResultMessage, UserMessage,
};
use local_agent_runtime::{
    Message, ToolCallResult, decode_local_runtime_tool_call_data,
    decode_local_runtime_tool_result_data,
};
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::images::{MAX_IMAGE_COUNT_FOR_QUERY, ProcessImageResult, process_image_for_agent};
use crate::model_packs::{ModelFamily, detect_model_family};
use crate::prompt;
use crate::provider::ProviderConfig;
use crate::registry::LocalRuntimeToolRegistry;
use crate::tool_proto::proto_tool_call_to_runtime_with_registry;
use crate::tool_results::{render_tool_call_result, tool_call_result_message};

/// Identifiers the client correlates a response stream with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnIds {
    pub conversation_id: String,
    pub request_id: String,
    pub run_id: String,
    pub task_id: String,
    /// Whether `task_id` names a task the client already has (otherwise the stream creates it).
    pub task_exists: bool,
}

impl TurnIds {
    pub fn from_request(request: &api::Request) -> Self {
        let metadata = request.metadata.as_ref();
        let tasks = request
            .task_context
            .as_ref()
            .map(|context| context.tasks.as_slice())
            .unwrap_or_default();
        Self {
            conversation_id: metadata
                .map(|metadata| metadata.conversation_id.clone())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(new_id),
            request_id: new_id(),
            run_id: metadata
                .map(|metadata| metadata.ambient_agent_task_id.clone())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(new_id),
            task_id: tasks
                .last()
                .map(|task| task.id.clone())
                .unwrap_or_else(new_id),
            task_exists: !tasks.is_empty(),
        }
    }
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub struct TurnPlan {
    pub ids: TurnIds,
    pub provider: ProviderConfig,
    pub model_family: ModelFamily,
    pub registry: Arc<LocalRuntimeToolRegistry>,
    pub system_prompt: String,
    pub context_window_limit: Option<usize>,
    pub initial_messages: Vec<Message>,
    pub user_input: UserMessage,
    /// Client-executed tool results, echoed back for persistence before the run starts.
    pub echo_messages: Vec<api::Message>,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnPlanError {
    #[error("no custom model provider matches model_config.base `{0}`")]
    MissingProvider(String),
    #[error(
        "custom model provider for `{0}` uses an unsupported endpoint schema; only OPENAI_CHAT_COMPLETIONS is supported"
    )]
    UnsupportedSchema(String),
    #[error("No user query found in messages.")]
    MissingUserQuery,
}

pub fn plan_turn(request: &api::Request, ids: TurnIds) -> Result<TurnPlan, TurnPlanError> {
    let provider = provider_config(request)?;
    let registry = Arc::new(LocalRuntimeToolRegistry::from_request(request));
    let model_family = detect_model_family(&provider.model);
    let system_prompt =
        prompt::system_prompt_for_request_with_model(request, &registry, &provider.model);
    let context_window_limit = request
        .settings
        .as_ref()
        .and_then(|settings| settings.model_config.as_ref())
        .map(|model_config| model_config.base_model_context_window_limit)
        .filter(|limit| *limit > 0)
        .map(|limit| limit as usize);

    let mut messages = task_messages(request, &registry);
    let mut echo_messages = Vec::new();
    let mut user_query = None;
    let mut tool_results = 0;
    let mut any_tool_error = false;
    for input in user_inputs(request) {
        match &input.input {
            Some(api::request::input::user_inputs::user_input::Input::UserQuery(query)) => {
                if user_query.is_none() {
                    user_query = Some(query);
                }
            }
            Some(api::request::input::user_inputs::user_input::Input::ToolCallResult(result)) => {
                let rendered = render_tool_call_result(result);
                tool_results += 1;
                any_tool_error |= rendered.is_error;
                echo_messages.push(tool_call_result_message(
                    &ids.task_id,
                    &ids.request_id,
                    result,
                    &rendered,
                ));
                messages.push(Message::ToolResult(ToolResultMessage {
                    call_id: result.tool_call_id.clone(),
                    result: rendered,
                }));
            }
            Some(api::request::input::user_inputs::user_input::Input::CliAgentUserQuery(_))
            | Some(
                api::request::input::user_inputs::user_input::Input::MessagesReceivedFromAgents(_),
            )
            | Some(api::request::input::user_inputs::user_input::Input::EventsFromAgents(_))
            | Some(api::request::input::user_inputs::user_input::Input::PassiveSuggestionResult(
                _,
            ))
            | Some(
                api::request::input::user_inputs::user_input::Input::OrchestrationConfigUpdate(_),
            )
            | Some(api::request::input::user_inputs::user_input::Input::ConversationHandoff(_))
            | None => {}
        }
    }
    let initial_messages = retain_paired_tool_messages(messages);

    let images = request
        .input
        .as_ref()
        .and_then(|input| input.context.as_ref())
        .map(|context| context.images.as_slice())
        .unwrap_or_default();
    let user_input = match user_query {
        Some(query) => build_user_message(&query.query, images),
        None if tool_results > 0 => UserMessage::text(grounding_cue(any_tool_error)),
        None => UserMessage::text(String::new()),
    };
    if !user_input.has_query() && !messages_have_user_query(&initial_messages) {
        return Err(TurnPlanError::MissingUserQuery);
    }

    Ok(TurnPlan {
        ids,
        provider,
        model_family,
        registry,
        system_prompt,
        context_window_limit,
        initial_messages,
        user_input,
        echo_messages,
    })
}

/// Weak local models often ignore successful tool results and invent timeouts; this cue is the
/// user turn that follows client-executed tool results.
fn grounding_cue(any_tool_error: bool) -> &'static str {
    if any_tool_error {
        "Tool results are above (some failed). Answer from those results only. Do not invent different errors or timeouts."
    } else {
        "Tool results are above and succeeded. Answer the user now using only those results (e.g. command output). Do not apologize, do not invent timeouts, and do not re-run the same command unless the user asks."
    }
}

fn user_inputs(request: &api::Request) -> &[api::request::input::user_inputs::UserInput] {
    match request
        .input
        .as_ref()
        .and_then(|input| input.r#type.as_ref())
    {
        Some(api::request::input::Type::UserInputs(user_inputs)) => &user_inputs.inputs,
        _ => &[],
    }
}

fn provider_config(request: &api::Request) -> Result<ProviderConfig, TurnPlanError> {
    let settings = request.settings.as_ref();
    let base = settings
        .and_then(|settings| settings.model_config.as_ref())
        .map(|model_config| model_config.base.as_str())
        .unwrap_or_default();
    let providers = settings
        .and_then(|settings| settings.custom_model_providers.as_ref())
        .map(|providers| providers.providers.as_slice())
        .unwrap_or_default();
    let (provider, model) = providers
        .iter()
        .find_map(|provider| {
            provider
                .models
                .iter()
                .find(|model| model.config_key == base)
                .map(|model| (provider, model))
        })
        .ok_or_else(|| TurnPlanError::MissingProvider(base.to_string()))?;
    let chat_completions =
        api::request::settings::custom_model_providers::CustomEndpointSchema::OpenaiChatCompletions;
    if provider.schema != chat_completions as i32 {
        return Err(TurnPlanError::UnsupportedSchema(base.to_string()));
    }
    let api_key = provider.api_key.trim();
    Ok(ProviderConfig {
        base_url: provider.base_url.clone(),
        api_key: (!api_key.is_empty()).then(|| api_key.to_string()),
        model: model.slug.clone(),
    })
}

fn task_messages(request: &api::Request, registry: &LocalRuntimeToolRegistry) -> Vec<Message> {
    request
        .task_context
        .as_ref()
        .map(|context| context.tasks.as_slice())
        .unwrap_or_default()
        .iter()
        .flat_map(|task| task.messages.iter())
        .filter_map(|message| translate_proto_to_runtime_message(message, registry))
        .collect()
}

fn translate_proto_to_runtime_message(
    msg: &api::Message,
    registry: &LocalRuntimeToolRegistry,
) -> Option<Message> {
    use api::message::Message as M;
    let inner = msg.message.as_ref()?;
    match inner {
        M::UserQuery(q) => Some(Message::User(UserMessage::text(q.query.clone()))),
        M::AgentOutput(out) => Some(Message::Assistant(AssistantMessage {
            content: out.text.clone(),
            tool_calls: vec![],
        })),
        M::ToolCall(tool_call) => {
            let call = decode_local_runtime_tool_call_data(&msg.server_message_data)
                .or_else(|| proto_tool_call_to_runtime_with_registry(tool_call, registry))?;
            Some(Message::Assistant(AssistantMessage {
                content: String::new(),
                tool_calls: vec![call],
            }))
        }
        M::ToolCallResult(result) => {
            let (call_id, runtime_result) = decode_local_runtime_tool_result_data(
                &msg.server_message_data,
            )
            .unwrap_or_else(|| {
                (
                    result.tool_call_id.clone(),
                    ToolCallResult {
                        content: format!("{:?}", result.result),
                        is_error: false,
                    },
                )
            });
            Some(Message::ToolResult(ToolResultMessage {
                call_id,
                result: runtime_result,
            }))
        }
        _ => None,
    }
}

/// Drop assistant tool calls without a result and results without a call so the provider never
/// sees a half-recorded tool exchange.
fn retain_paired_tool_messages(messages: Vec<Message>) -> Vec<Message> {
    let call_ids = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(message.tool_calls.iter().map(|call| &call.id)),
            Message::System(_) | Message::User(_) | Message::ToolResult(_) => None,
        })
        .flatten()
        .cloned()
        .collect::<HashSet<_>>();
    let result_ids = messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(message) => Some(message.call_id.clone()),
            Message::System(_) | Message::User(_) | Message::Assistant(_) => None,
        })
        .collect::<HashSet<_>>();

    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::Assistant(mut assistant) => {
                assistant
                    .tool_calls
                    .retain(|call| result_ids.contains(&call.id));
                (!assistant.content.is_empty() || !assistant.tool_calls.is_empty())
                    .then_some(Message::Assistant(assistant))
            }
            Message::ToolResult(result) => call_ids
                .contains(&result.call_id)
                .then_some(Message::ToolResult(result)),
            Message::System(_) | Message::User(_) => Some(message),
        })
        .collect()
}

fn messages_have_user_query(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(user) => user.has_query(),
        Message::System(_) | Message::Assistant(_) | Message::ToolResult(_) => false,
    })
}

fn build_user_message(query: &str, images: &[api::input_context::Image]) -> UserMessage {
    let mut parts = vec![ContentPart::Text(query.to_string())];
    parts.extend(
        images
            .iter()
            .filter_map(image_to_content_part)
            .take(MAX_IMAGE_COUNT_FOR_QUERY),
    );
    UserMessage { parts }
}

/// Resize/validate an attachment and encode it as an image part. A bad attachment is skipped
/// rather than failing the whole turn.
fn image_to_content_part(image: &api::input_context::Image) -> Option<ContentPart> {
    let processed_bytes = match process_image_for_agent(&image.data) {
        ProcessImageResult::Success { data } => data,
        ProcessImageResult::TooLarge => {
            tracing::warn!("skipping image attachment: too large after resizing");
            return None;
        }
        ProcessImageResult::Error(error) => {
            tracing::warn!(%error, "skipping image attachment: failed to process");
            return None;
        }
    };

    Some(ContentPart::Image {
        mime_type: image.mime_type.clone(),
        data_base64: BASE64_STANDARD.encode(&processed_bytes),
        file_name: None,
    })
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
