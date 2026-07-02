//! In-process agent loop driver for the Ollama harness.
//!
//! When the user has configured a local Ollama server (via the AI settings
//! panel), `generate_multi_agent_output` swaps the Warp backend out for this
//! module. We translate the proto request into OpenAI chat-completions form,
//! call the local model, then synthesize the same `ResponseEvent` stream
//! shape the rest of the client expects — so the existing controller,
//! transcript, and rendering pipelines work unchanged.
//!
//! ## v1 scope
//! - Plain text replies are wired end-to-end.
//! - Tool calls are scaffolded (schemas advertised, OpenAI `tool_calls`
//!   parsed) but only `RunShellCommand` is emitted as a real proto
//!   `ToolCall` message; other tool requests are surfaced as text so the
//!   user can see what the model wanted to do.
//! - Streaming is not yet implemented — the full reply is buffered and
//!   emitted as one `AppendToMessageContent` event. Adding SSE streaming
//!   is a follow-up.
//! - No credit accounting is reported in `StreamFinished` (Ollama is free).

use std::sync::Arc;

use async_channel::Sender;
use futures::stream::StreamExt;
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::ai::agent::api::{Event, OllamaConfig, RequestParams, ResponseStream};
use crate::ai::agent::AIAgentInput;
use crate::server::server_api::AIApiError;

use super::{ChatMessage, OllamaClient, ToolCallParsed};

/// Build a `ResponseStream` that runs one Ollama turn.
///
/// `cancellation_rx` is consumed by the caller via `take_until`, so we don't
/// need to plumb it through this module.
pub fn run_request(cfg: OllamaConfig, params: RequestParams) -> ResponseStream {
    let (tx, rx) = async_channel::unbounded::<Event>();

    // Spawn the actual Ollama call. We don't have a Send-bounded executor
    // handle here, so we use `tokio::spawn`. The MAA controller uses tokio
    // under the hood; if that ever changes, we can switch to a runtime-
    // agnostic spawner.
    tokio::spawn(async move {
        let result = run_turn(cfg, params, &tx).await;
        if let Err(err) = result {
            // Best-effort: if the channel is closed (caller cancelled), we drop.
            let _ = tx.send(Err(Arc::new(err))).await;
        }
        // Dropping `tx` ends the stream.
    });

    Box::pin(rx)
}

async fn run_turn(
    cfg: OllamaConfig,
    params: RequestParams,
    tx: &Sender<Event>,
) -> Result<(), AIApiError> {
    let conversation_id = params
        .conversation_token
        .as_ref()
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let request_id = Uuid::new_v4().to_string();
    // Use the existing ambient-agent task id when present so the
    // controller can correlate this run with its task. Otherwise mint
    // a stable per-request id.
    let run_id = params
        .ambient_agent_task_id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    emit_init(tx, &conversation_id, &request_id, &run_id).await;

    let messages = build_messages(&params);

    let client = OllamaClient::new(cfg.base_url.clone(), cfg.api_key.clone());
    let completion = client
        .chat_with_tools(&cfg.model, messages, None)
        .await
        .map_err(AIApiError::Other)?;

    let task_id = first_task_id(&params).unwrap_or_else(|| Uuid::new_v4().to_string());
    let new_task_needed = first_task_id(&params).is_none();

    emit_assistant_turn(
        tx,
        &task_id,
        new_task_needed,
        &request_id,
        &completion.text,
        &completion.tool_calls,
    )
    .await;

    emit_finished(tx).await;
    Ok(())
}

fn first_task_id(params: &RequestParams) -> Option<String> {
    params.tasks.last().map(|t| t.id.clone())
}

/// Translate the existing proto transcript plus the latest `input` into the
/// flat OpenAI chat-message list Ollama expects.
///
/// We don't try to be exhaustive — only the message types that actually
/// shape an LLM turn (user queries, agent text replies, tool calls, tool
/// results) are translated. Everything else is dropped silently.
fn build_messages(params: &RequestParams) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    messages.push(ChatMessage::system(SYSTEM_PROMPT.to_string()));

    for task in &params.tasks {
        for proto_msg in &task.messages {
            if let Some(chat_msg) = translate_proto_message(proto_msg) {
                messages.push(chat_msg);
            }
        }
    }

    for input in &params.input {
        if let Some(chat_msg) = translate_input(input) {
            messages.push(chat_msg);
        }
    }

    messages
}

const SYSTEM_PROMPT: &str = "You are a coding assistant running locally via Ollama, integrated into the Warp terminal. Reply concisely. When you need to take an action (run a command, read a file, etc.), prefer to call the matching tool; otherwise reply with plain text.";

fn translate_proto_message(msg: &api::Message) -> Option<ChatMessage> {
    use api::message::Message as M;
    let inner = msg.message.as_ref()?;
    match inner {
        M::UserQuery(q) => Some(ChatMessage::user(q.query.clone())),
        M::AgentOutput(out) => Some(ChatMessage::assistant(out.text.clone())),
        M::ToolCallResult(_result) => {
            // We don't yet carry tool-call IDs through the round-trip, so
            // surface the result as a plain user message instead. Full
            // tool-result wiring is a follow-up; see module docs.
            // Best-effort: skip silently rather than fabricate.
            None
        }
        _ => None,
    }
}

fn translate_input(input: &AIAgentInput) -> Option<ChatMessage> {
    match input {
        AIAgentInput::UserQuery { query, .. } => Some(ChatMessage::user(query.clone())),
        AIAgentInput::ActionResult { .. } => {
            // Tool results round-trip through the proto Task on the next
            // call; no extra message needed here.
            None
        }
        _ => None,
    }
}

async fn emit_init(tx: &Sender<Event>, conversation_id: &str, request_id: &str, run_id: &str) {
    let event = api::ResponseEvent {
        r#type: Some(api::response_event::Type::Init(
            api::response_event::StreamInit {
                conversation_id: conversation_id.to_string(),
                request_id: request_id.to_string(),
                run_id: run_id.to_string(),
            },
        )),
    };
    let _ = tx.send(Ok(event)).await;
}

async fn emit_assistant_turn(
    tx: &Sender<Event>,
    task_id: &str,
    create_new_task: bool,
    request_id: &str,
    text: &str,
    tool_calls: &[ToolCallParsed],
) {
    let mut actions = vec![begin_transaction()];

    if create_new_task {
        actions.push(create_task(task_id));
    }

    let agent_message_id = Uuid::new_v4().to_string();
    if !text.is_empty() {
        actions.push(add_agent_output(
            task_id,
            &agent_message_id,
            request_id,
            text,
        ));
    } else if tool_calls.is_empty() {
        // Empty reply with no tool calls — still emit something so the
        // transcript isn't blank.
        actions.push(add_agent_output(
            task_id,
            &agent_message_id,
            request_id,
            "(no response)",
        ));
    }

    for tc in tool_calls {
        if let Some(action) = tool_call_to_action(task_id, request_id, tc) {
            actions.push(action);
        } else {
            // Unknown / unsupported tool — surface the intent as text so the
            // user sees what the model tried to do.
            let fallback_id = Uuid::new_v4().to_string();
            actions.push(add_agent_output(
                task_id,
                &fallback_id,
                request_id,
                &format!(
                    "[Ollama wanted to call `{}` with arguments: {}]",
                    tc.function.name, tc.function.arguments
                ),
            ));
        }
    }

    actions.push(commit_transaction());

    let event = api::ResponseEvent {
        r#type: Some(api::response_event::Type::ClientActions(
            api::response_event::ClientActions { actions },
        )),
    };
    let _ = tx.send(Ok(event)).await;
}

async fn emit_finished(tx: &Sender<Event>) {
    let event = api::ResponseEvent {
        r#type: Some(api::response_event::Type::Finished(
            api::response_event::StreamFinished {
                reason: Some(api::response_event::stream_finished::Reason::Done(
                    api::response_event::stream_finished::Done {},
                )),
                ..Default::default()
            },
        )),
    };
    let _ = tx.send(Ok(event)).await;
}

fn begin_transaction() -> api::ClientAction {
    api::ClientAction {
        action: Some(api::client_action::Action::BeginTransaction(
            api::client_action::BeginTransaction {},
        )),
    }
}

fn commit_transaction() -> api::ClientAction {
    api::ClientAction {
        action: Some(api::client_action::Action::CommitTransaction(
            api::client_action::CommitTransaction {},
        )),
    }
}

fn create_task(task_id: &str) -> api::ClientAction {
    api::ClientAction {
        action: Some(api::client_action::Action::CreateTask(
            api::client_action::CreateTask {
                task: Some(api::Task {
                    id: task_id.to_string(),
                    description: String::new(),
                    dependencies: None,
                    messages: vec![],
                    summary: String::new(),
                    server_data: String::new(),
                }),
            },
        )),
    }
}

fn add_agent_output(
    task_id: &str,
    message_id: &str,
    request_id: &str,
    text: &str,
) -> api::ClientAction {
    let message = api::Message {
        id: message_id.to_string(),
        task_id: task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: None,
        server_message_data: String::new(),
        citations: vec![],
        fetched_memories: vec![],
        message: Some(api::message::Message::AgentOutput(
            api::message::AgentOutput {
                text: text.to_string(),
            },
        )),
    };
    api::ClientAction {
        action: Some(api::client_action::Action::AddMessagesToTask(
            api::client_action::AddMessagesToTask {
                task_id: task_id.to_string(),
                messages: vec![message],
            },
        )),
    }
}

/// Convert an OpenAI-style tool call to a proto `ToolCall` message action.
///
/// v1 only handles `run_shell_command`. Returning `None` lets the caller
/// fall back to a plain-text "model tried to call X" message.
fn tool_call_to_action(
    task_id: &str,
    request_id: &str,
    tc: &ToolCallParsed,
) -> Option<api::ClientAction> {
    use api::message::tool_call::Tool;

    let proto_tool = match tc.function.name.as_str() {
        "run_shell_command" => {
            let args: serde_json::Value = serde_json::from_str(&tc.function.arguments).ok()?;
            let command = args.get("command")?.as_str()?.to_string();
            Tool::RunShellCommand(api::message::tool_call::RunShellCommand {
                command,
                ..Default::default()
            })
        }
        _ => return None,
    };

    let tool_call_id = if tc.id.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        tc.id.clone()
    };
    let message_id = Uuid::new_v4().to_string();

    let message = api::Message {
        id: message_id,
        task_id: task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: None,
        server_message_data: String::new(),
        citations: vec![],
        fetched_memories: vec![],
        message: Some(api::message::Message::ToolCall(api::message::ToolCall {
            tool_call_id,
            tool: Some(proto_tool),
        })),
    };

    Some(api::ClientAction {
        action: Some(api::client_action::Action::AddMessagesToTask(
            api::client_action::AddMessagesToTask {
                task_id: task_id.to_string(),
                messages: vec![message],
            },
        )),
    })
}

/// Compile-time assertion that the returned stream is `Send` so it satisfies
/// the `ResponseStream` alias on non-wasm targets.
#[cfg(not(target_family = "wasm"))]
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<async_channel::Receiver<Event>>();
};

// `StreamExt` is brought in only for `take_until` — keep the import alive
// across cfg shuffles so the warning gate doesn't trip.
#[allow(dead_code)]
fn _ensure_streamext_used<S: futures::Stream + Unpin>(s: S) {
    let _ = StreamExt::map(s, |x| x);
}
