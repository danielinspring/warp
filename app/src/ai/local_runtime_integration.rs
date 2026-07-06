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
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::ai::agent::api::{Event, OllamaConfig, RequestParams, ResponseStream};
use crate::ai::agent::AIAgentInput;
use crate::ai::local_runtime_bridge::event_mapper::EventMapper;
use crate::ai::local_runtime_bridge::{
    LocalRuntimeToolRegistry, ToolExecutionRequest, WarpToolExecutor,
};
use crate::ai::local_runtime_event_bus;
use crate::ai::local_runtime_spec;
use crate::server::server_api::AIApiError;

use local_agent_runtime::messages::{AssistantMessage, ToolResultMessage, UserMessage};
use local_agent_runtime::provider::ollama::{OllamaProvider, OllamaProviderConfig};
use local_agent_runtime::{AgentRuntime, Message, RuntimeConfig, ToolCall};

/// Build a `ResponseStream` using the new local agent runtime.
///
/// This is a drop-in replacement for `ollama::agent_loop::run_request`.
pub fn run_with_local_runtime(
    cfg: OllamaConfig,
    params: RequestParams,
    tool_request_tx: async_channel::Sender<ToolExecutionRequest>,
    cancellation_rx: oneshot::Receiver<()>,
) -> ResponseStream {
    let (tx, rx) = async_channel::unbounded::<Event>();

    tokio::spawn(async move {
        let result = run_runtime(cfg, params, tool_request_tx, cancellation_rx, &tx).await;
        if let Err(err) = result {
            let _ = tx.send(Err(Arc::new(err))).await;
        }
    });

    Box::pin(rx)
}

async fn run_runtime(
    cfg: OllamaConfig,
    params: RequestParams,
    tool_request_tx: async_channel::Sender<ToolExecutionRequest>,
    cancellation_rx: oneshot::Receiver<()>,
    tx: &Sender<Event>,
) -> Result<(), AIApiError> {
    // Set up the runtime components
    let provider = OllamaProvider::new(OllamaProviderConfig {
        base_url: cfg.base_url,
        api_key: cfg.api_key,
        timeout_secs: 300,
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

    let registry = Arc::new(LocalRuntimeToolRegistry::from_request(&params));
    let executor = WarpToolExecutor::new(
        tool_request_tx,
        Arc::clone(&registry),
        crate::ai::agent::task::TaskId::new(task_id.clone()),
        request_id.clone(),
    );

    let runtime_config = RuntimeConfig {
        system_prompt: Some(local_runtime_spec::system_prompt_for_request(&params)),
        max_turns: 10,
        ..Default::default()
    };

    let runtime = AgentRuntime::new(provider, executor, runtime_config);

    // Build initial messages from existing conversation history
    let initial_messages = build_initial_messages(&params);

    // Extract the user's latest input
    let user_input = extract_user_input(&params).unwrap_or_default();

    let mut mapper = EventMapper::new(
        conversation_id,
        request_id,
        run_id.clone(),
        task_id,
        task_exists,
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
            event = runtime_events.next() => {
                let Some(event) = event else {
                    break;
                };
                local_runtime_event_bus::publish(&run_id, event.clone());
                let proto_events = mapper.map_event(&event);
                for proto_event in proto_events {
                    let _ = tx.send(Ok(proto_event)).await;
                }
                if matches!(event, local_agent_runtime::RuntimeEvent::Finished { .. }) {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn build_initial_messages(params: &RequestParams) -> Vec<Message> {
    let mut messages = Vec::new();

    for task in &params.tasks {
        for proto_msg in &task.messages {
            if let Some(msg) = translate_proto_to_runtime_message(proto_msg) {
                messages.push(msg);
            }
        }
    }

    messages
}

fn translate_proto_to_runtime_message(msg: &api::Message) -> Option<Message> {
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
        M::ToolCall(tool_call) => Some(Message::Assistant(AssistantMessage {
            content: String::new(),
            tool_calls: vec![translate_proto_tool_call(tool_call)?],
        })),
        M::ToolCallResult(result) => {
            let content = format!("{:?}", result.result);
            Some(Message::ToolResult(ToolResultMessage {
                call_id: result.tool_call_id.clone(),
                result: local_agent_runtime::ToolCallResult {
                    content,
                    is_error: false,
                },
            }))
        }
        _ => None,
    }
}

fn translate_proto_tool_call(tool_call: &api::message::ToolCall) -> Option<ToolCall> {
    let tool = tool_call.tool.as_ref()?;
    let (name, arguments) = match tool {
        api::message::tool_call::Tool::RunShellCommand(tool) => (
            "run_shell_command",
            serde_json::json!({
                "command": tool.command.clone(),
                "is_read_only": tool.is_read_only,
                "is_risky": tool.is_risky,
                "uses_pager": tool.uses_pager,
            }),
        ),
        api::message::tool_call::Tool::ReadFiles(tool) => (
            "read_files",
            serde_json::json!({
                "paths": tool.files.iter().map(|file| file.name.clone()).collect::<Vec<_>>(),
            }),
        ),
        api::message::tool_call::Tool::Grep(tool) => (
            "grep",
            serde_json::json!({
                "queries": tool.queries.clone(),
                "path": tool.path.clone(),
            }),
        ),
        api::message::tool_call::Tool::FileGlobV2(tool) => (
            "file_glob_v2",
            serde_json::json!({
                "patterns": tool.patterns.clone(),
                "search_dir": tool.search_dir.clone(),
            }),
        ),
        api::message::tool_call::Tool::SearchCodebase(tool) => (
            "search_codebase",
            serde_json::json!({
                "query": tool.query.clone(),
                "path_filters": tool.path_filters.clone(),
                "codebase_path": tool.codebase_path.clone(),
            }),
        ),
        _ => return None,
    };

    Some(ToolCall {
        id: tool_call.tool_call_id.clone(),
        name: name.to_string(),
        arguments,
    })
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
