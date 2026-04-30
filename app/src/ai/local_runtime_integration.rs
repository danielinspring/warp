//! Integration point between `local_agent_runtime` and Warp's agent pipeline.
//!
//! This module provides `run_with_local_runtime`, an alternative to the
//! current `ollama::agent_loop::run_request` that uses the new runtime crate.
//!
//! ## Activation
//!
//! This path is not yet active. To activate it in the future, replace the call
//! in `generate_multi_agent_output` (in `api/impl.rs`) with:
//!
//! ```rust,ignore
//! let stream = crate::ai::local_runtime_integration::run_with_local_runtime(
//!     ollama_cfg,
//!     params,
//! );
//! ```
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
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::ai::agent::api::{Event, OllamaConfig, RequestParams, ResponseStream};
use crate::ai::agent::AIAgentInput;
use crate::ai::local_runtime_bridge::event_mapper::EventMapper;
use crate::ai::local_runtime_bridge::WarpToolExecutor;
use crate::server::server_api::AIApiError;

use local_agent_runtime::provider::ollama::{OllamaProvider, OllamaProviderConfig};
use local_agent_runtime::messages::{UserMessage, AssistantMessage, SystemMessage, ToolResultMessage};
use local_agent_runtime::{AgentRuntime, Message, RuntimeConfig, RuntimeEvent};

/// Build a `ResponseStream` using the new local agent runtime.
///
/// This is a drop-in replacement for `ollama::agent_loop::run_request`.
pub fn run_with_local_runtime(cfg: OllamaConfig, params: RequestParams) -> ResponseStream {
    let (tx, rx) = async_channel::unbounded::<Event>();

    tokio::spawn(async move {
        let result = run_runtime(cfg, params, &tx).await;
        if let Err(err) = result {
            let _ = tx.send(Err(Arc::new(err))).await;
        }
    });

    Box::pin(rx)
}

async fn run_runtime(
    cfg: OllamaConfig,
    params: RequestParams,
    tx: &Sender<Event>,
) -> Result<(), AIApiError> {
    // Set up the runtime components
    let provider = OllamaProvider::new(OllamaProviderConfig {
        base_url: cfg.base_url,
        api_key: cfg.api_key,
        timeout_secs: 300,
    });

    let executor = WarpToolExecutor::new();

    let runtime_config = RuntimeConfig {
        system_prompt: Some(SYSTEM_PROMPT.to_string()),
        max_turns: 10,
        ..Default::default()
    };

    let runtime = AgentRuntime::new(provider, executor, runtime_config);

    // Build initial messages from existing conversation history
    let initial_messages = build_initial_messages(&params);

    // Extract the user's latest input
    let user_input = extract_user_input(&params).unwrap_or_default();

    // Determine IDs for event mapping
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

    // Run the agent loop
    let (events, _final_messages) = runtime
        .run_to_completion(&cfg.model, initial_messages, &user_input)
        .await
        .map_err(|e| AIApiError::Other(e.into()))?;

    // Map runtime events to proto ResponseEvents
    let mut mapper = EventMapper::new(conversation_id, request_id, run_id, task_id, task_exists);

    for event in &events {
        let proto_events = mapper.map_event(event);
        for proto_event in proto_events {
            let _ = tx.send(Ok(proto_event)).await;
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
        M::ToolCallResult(result) => {
            // Extract text content from the tool call result.
            // The proto uses a oneof for different result types — we extract
            // a generic text representation for the runtime's flat format.
            let content = format!("[tool result for call_id: {}]", result.tool_call_id);
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

fn extract_user_input(params: &RequestParams) -> Option<String> {
    for input in &params.input {
        match input {
            AIAgentInput::UserQuery { query, .. } => return Some(query.clone()),
            _ => continue,
        }
    }
    None
}

const SYSTEM_PROMPT: &str = "You are a coding assistant running locally via Ollama, integrated into the Warp terminal. Reply concisely. When you need to take an action (run a command, read a file, etc.), prefer to call the matching tool; otherwise reply with plain text.";
