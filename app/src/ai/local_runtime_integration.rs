//! Integration point between `local_agent_runtime` and Warp's agent pipeline.
//!
//! This module provides `run_with_local_runtime`, an alternative to the
//! current `ollama::agent_loop::run_request` that uses the new runtime crate.
//!
//! ## Activation
//!
//! `ResponseStream` selects this path for Ollama requests when
//! `FeatureFlag::LocalOllamaRuntimeToolUse` is enabled. The legacy
//! `ollama::agent_loop` remains the fallback while the flag is disabled.
//!
//! ## What this provides over the current `agent_loop.rs`
//!
//! - Full tool schema advertisement to the LLM
//! - Multi-turn tool call loops (not single-shot)
//! - Proper tool_call_id preservation in the round-trip
//! - Clean separation between LLM provider, tool execution, and event emission
//! - Ready for extraction to a separate service

use std::sync::Arc;

use async_channel::Sender;
use futures::channel::oneshot;
use futures::StreamExt;
use local_agent_runtime::messages::{AssistantMessage, ToolResultMessage, UserMessage};
use local_agent_runtime::provider::ollama::{OllamaProvider, OllamaProviderConfig};
use local_agent_runtime::{
    AgentRuntime, ChannelTelemetrySink, CompositeHooks, ContextBudget, LifecycleHooks,
    LoggingHooks, Message, RuntimeConfig, RuntimeTelemetryEvent, RuntimeTelemetrySink,
    TelemetryLifecycleHooks, ToolNameDenyHooks,
};
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::ai::agent::api::{Event, OllamaConfig, RequestParams, ResponseStream};
use crate::ai::agent::AIAgentInput;
use crate::ai::local_runtime_bridge::event_mapper::EventMapper;
use crate::ai::local_runtime_bridge::{
    decode_local_runtime_tool_call_data, decode_local_runtime_tool_result_data,
    proto_tool_call_to_runtime_with_registry, LocalRuntimeToolRegistry, ToolExecutionRequest,
    WarpToolExecutor,
};
use crate::ai::{local_runtime_event_bus, local_runtime_spec};
use crate::server::server_api::AIApiError;

/// Build a `ResponseStream` using the new local agent runtime.
///
/// This is a drop-in replacement for `ollama::agent_loop::run_request`.
/// `available_skills` is the cwd/home/bundled catalog resolved on the UI thread
/// before spawn (see feat-016).
pub fn run_with_local_runtime(
    cfg: OllamaConfig,
    params: RequestParams,
    available_skills: Vec<crate::ai::skills::SkillDescriptor>,
    tool_request_tx: async_channel::Sender<ToolExecutionRequest>,
    cancellation_rx: oneshot::Receiver<()>,
) -> ResponseStream {
    let (tx, rx) = async_channel::unbounded::<Event>();

    tokio::spawn(async move {
        let result = run_runtime(
            cfg,
            params,
            available_skills,
            tool_request_tx,
            cancellation_rx,
            &tx,
        )
        .await;
        if let Err(err) = result {
            let _ = tx.send(Err(Arc::new(err))).await;
        }
    });

    Box::pin(rx)
}

async fn run_runtime(
    cfg: OllamaConfig,
    params: RequestParams,
    available_skills: Vec<crate::ai::skills::SkillDescriptor>,
    tool_request_tx: async_channel::Sender<ToolExecutionRequest>,
    cancellation_rx: oneshot::Receiver<()>,
    tx: &Sender<Event>,
) -> Result<(), AIApiError> {
    // Set up the runtime components
    let provider = OllamaProvider::new(OllamaProviderConfig {
        base_url: cfg.base_url,
        api_key: cfg.api_key,
        timeout_secs: 300,
        ..Default::default()
    });

    // Determine IDs for event mapping and tool execution.
    let conversation_id = params
        .conversation_token
        .as_ref()
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let request_id = Uuid::new_v4().to_string();
    let run_id = params
        .ambient_agent_task_id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let task_id = params
        .tasks
        .last()
        .map(|t| t.id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let task_exists = !params.tasks.is_empty();

    let registry = Arc::new(
        LocalRuntimeToolRegistry::from_request_with_available_skills(&params, &available_skills),
    );
    let executor = WarpToolExecutor::new(
        tool_request_tx,
        Arc::clone(&registry),
        crate::ai::agent::task::TaskId::new(task_id.clone()),
        request_id.clone(),
    );

    let runtime_config = RuntimeConfig {
        system_prompt: Some(local_runtime_spec::system_prompt_for_request(
            &params, &registry,
        )),
        context_budget: ContextBudget {
            max_input_tokens: params.context_window_limit.map(|limit| limit as usize),
            ..Default::default()
        },
        ..Default::default()
    };

    let (telemetry_tx, telemetry_rx) = async_channel::unbounded::<RuntimeTelemetryEvent>();
    let telemetry_sink: Arc<dyn RuntimeTelemetrySink> =
        Arc::new(ChannelTelemetrySink::new(telemetry_tx));
    let runtime = AgentRuntime::new(provider, executor, runtime_config)
        .with_hooks(default_lifecycle_hooks(Arc::clone(&telemetry_sink)))
        .with_telemetry(Arc::clone(&telemetry_sink));

    // Build initial messages from existing conversation history
    let initial_messages = build_initial_messages(&params, &registry);

    // Extract the user's latest input
    let user_input = extract_user_input(&params).unwrap_or_default();

    let mut mapper = EventMapper::new(
        conversation_id,
        request_id,
        run_id.clone(),
        task_id,
        task_exists,
        Arc::clone(&registry),
    );

    let (mut runtime_events, cancel_handle) =
        runtime.run(cfg.model.clone(), initial_messages, user_input);
    let mut cancellation_rx = Box::pin(cancellation_rx);
    let mut cancellation_sent = false;

    loop {
        tokio::select! {
            _ = &mut cancellation_rx, if !cancellation_sent => {
                cancellation_sent = true;
                cancel_handle.cancel();
            }
            telemetry_event = telemetry_rx.recv() => {
                match telemetry_event {
                    Ok(event) => {
                        log_and_record_runtime_telemetry(event);
                    }
                    Err(_) => {
                        // Runtime finished and closed the telemetry channel.
                    }
                }
            }
            event = runtime_events.next() => {
                let Some(event) = event else {
                    // Drain remaining telemetry before exit.
                    while let Ok(event) = telemetry_rx.try_recv() {
                        log_and_record_runtime_telemetry(event);
                    }
                    break;
                };
                local_runtime_event_bus::publish(&run_id, event.clone());
                let proto_events = mapper.map_event(&event);
                for proto_event in proto_events {
                    let _ = tx.send(Ok(proto_event)).await;
                }
                if matches!(event, local_agent_runtime::RuntimeEvent::Finished { .. }) {
                    while let Ok(event) = telemetry_rx.try_recv() {
                        log_and_record_runtime_telemetry(event);
                    }
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Trusted in-process hooks for local Ollama runs (logging + telemetry + optional deny list).
///
/// Set `WARP_LOCAL_AGENT_DENIED_TOOLS=tool_a,tool_b` to block tools by exact name
/// before permission/UI execution.
fn default_lifecycle_hooks(telemetry: Arc<dyn RuntimeTelemetrySink>) -> Arc<dyn LifecycleHooks> {
    let mut hooks: Vec<Arc<dyn LifecycleHooks>> = vec![
        Arc::new(LoggingHooks),
        Arc::new(TelemetryLifecycleHooks::new(telemetry)),
    ];
    if let Ok(raw) = std::env::var("WARP_LOCAL_AGENT_DENIED_TOOLS") {
        let denied_tools = raw
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !denied_tools.is_empty() {
            hooks.push(Arc::new(ToolNameDenyHooks { denied_tools }));
        }
    }
    Arc::new(CompositeHooks::new(hooks))
}

fn log_and_record_runtime_telemetry(event: RuntimeTelemetryEvent) {
    tracing::info!(
        target: "local_runtime_telemetry",
        ?event,
        "local agent runtime telemetry"
    );
    // Product telemetry registration (schema) for analytics pipelines.
    let _product_event =
        crate::ai::local_runtime_telemetry::LocalRuntimeTelemetryEvent::from(event);
    // Full send requires AppContext; structured log + registered schema cover dogfood today.
    // When a ctx-bearing bridge is available, emit via send_telemetry_from_ctx.
}

fn build_initial_messages(
    params: &RequestParams,
    registry: &LocalRuntimeToolRegistry,
) -> Vec<Message> {
    let mut messages = Vec::new();

    for task in &params.tasks {
        for proto_msg in &task.messages {
            if let Some(msg) = translate_proto_to_runtime_message(proto_msg, registry) {
                messages.push(msg);
            }
        }
    }

    retain_paired_tool_messages(messages)
}

fn translate_proto_to_runtime_message(
    msg: &api::Message,
    registry: &LocalRuntimeToolRegistry,
) -> Option<Message> {
    use api::message::Message as M;
    let inner = msg.message.as_ref()?;
    match inner {
        M::UserQuery(q) => Some(Message::User(UserMessage {
            content: q.query.clone(),
        })),
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
                    local_agent_runtime::ToolCallResult {
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

fn retain_paired_tool_messages(messages: Vec<Message>) -> Vec<Message> {
    use std::collections::HashSet;

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

fn extract_user_input(params: &RequestParams) -> Option<String> {
    for input in &params.input {
        match input {
            AIAgentInput::UserQuery { query, .. } => return Some(query.clone()),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
#[path = "local_runtime_integration_tests.rs"]
mod tests;
