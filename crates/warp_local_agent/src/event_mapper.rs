//! Maps runtime events onto the protobuf `ResponseEvent`s the Warp client understands.

use std::sync::Arc;

use local_agent_runtime::{
    FinishReason, RuntimeEvent, ToolCall, encode_local_runtime_tool_call_data,
    encode_local_runtime_tool_result_data,
};
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::registry::LocalRuntimeToolRegistry;
use crate::tool_proto::tool_call_to_proto_tool_with_registry;

/// State for mapping runtime events to proto ResponseEvents.
pub struct EventMapper {
    pub conversation_id: String,
    pub request_id: String,
    pub run_id: String,
    pub task_id: String,
    registry: Arc<LocalRuntimeToolRegistry>,
    task_created: bool,
    init_sent: bool,
    current_text_message_id: Option<String>,
}

impl EventMapper {
    pub fn new(
        conversation_id: String,
        request_id: String,
        run_id: String,
        task_id: String,
        task_exists: bool,
        registry: Arc<LocalRuntimeToolRegistry>,
    ) -> Self {
        Self {
            conversation_id,
            request_id,
            run_id,
            task_id,
            registry,
            task_created: task_exists,
            init_sent: false,
            current_text_message_id: None,
        }
    }

    /// The `Init` event the first turn would emit.
    pub fn init_event(&self) -> api::ResponseEvent {
        stream_init_event(&self.conversation_id, &self.request_id, &self.run_id)
    }

    /// Record that the caller already sent `Init`, so the first turn does not repeat it.
    pub fn mark_init_sent(&mut self) {
        self.init_sent = true;
    }

    /// Wrap client actions in a transaction, creating the task first when it does not exist yet.
    pub fn transaction(&mut self, actions: Vec<api::ClientAction>) -> api::ResponseEvent {
        let mut wrapped = Vec::with_capacity(actions.len() + 3);
        wrapped.push(begin_transaction());
        if !self.task_created {
            wrapped.push(create_task(&self.task_id));
            self.task_created = true;
        }
        wrapped.extend(actions);
        wrapped.push(commit_transaction());
        client_actions_event(wrapped)
    }

    /// Map a single RuntimeEvent to zero or more ResponseEvents.
    pub fn map_event(&mut self, event: &RuntimeEvent) -> Vec<api::ResponseEvent> {
        match event {
            RuntimeEvent::TurnStarted { turn } => {
                self.current_text_message_id = None;
                if *turn == 1 && !self.init_sent {
                    self.init_sent = true;
                    vec![self.init_event()]
                } else {
                    vec![]
                }
            }
            RuntimeEvent::TextDelta { text } => {
                if text.is_empty() {
                    return vec![];
                }

                let action = match self.current_text_message_id.clone() {
                    Some(message_id) => {
                        append_agent_output(&self.task_id, &message_id, &self.request_id, text)
                    }
                    None => {
                        let message_id = Uuid::new_v4().to_string();
                        let action =
                            add_agent_output(&self.task_id, &message_id, &self.request_id, text);
                        self.current_text_message_id = Some(message_id);
                        action
                    }
                };
                vec![self.transaction(vec![action])]
            }
            RuntimeEvent::TextCompleted { text } => {
                if self.current_text_message_id.is_some() {
                    return vec![];
                }

                let message_id = Uuid::new_v4().to_string();
                let action = add_agent_output(&self.task_id, &message_id, &self.request_id, text);
                self.current_text_message_id = Some(message_id);
                vec![self.transaction(vec![action])]
            }
            // Client tools are persisted when they are deferred (below); persisting them here
            // too would make the client execute calls that a trusted hook may still deny.
            RuntimeEvent::ToolCallsRequested { calls } => {
                let in_process = calls
                    .iter()
                    .filter(|call| self.registry.is_in_process(&call.name))
                    .cloned()
                    .collect::<Vec<_>>();
                self.tool_call_messages(&in_process)
            }
            RuntimeEvent::ToolCallsDeferred { calls } => self.tool_call_messages(calls),
            RuntimeEvent::ToolResult { call_id, result } => {
                // In-process tools register persistence when they execute; client-executed
                // tools are persisted by the request that carries their results.
                let Some(persistence) = self.registry.take_local_tool_persistence(call_id) else {
                    return vec![];
                };

                let mut actions = Vec::new();
                if let Some(update) = persistence.todo_update {
                    actions.push(add_messages(
                        &self.task_id,
                        vec![api::Message {
                            id: Uuid::new_v4().to_string(),
                            task_id: self.task_id.clone(),
                            request_id: self.request_id.clone(),
                            timestamp: None,
                            server_message_data: String::new(),
                            citations: vec![],
                            fetched_memories: vec![],
                            message: Some(api::message::Message::UpdateTodos(update)),
                        }],
                    ));
                }
                actions.push(add_messages(
                    &self.task_id,
                    vec![api::Message {
                        id: Uuid::new_v4().to_string(),
                        task_id: self.task_id.clone(),
                        request_id: self.request_id.clone(),
                        timestamp: None,
                        server_message_data: encode_local_runtime_tool_result_data(
                            call_id.clone(),
                            result,
                        ),
                        citations: vec![],
                        fetched_memories: vec![],
                        message: Some(api::message::Message::ToolCallResult(
                            api::message::ToolCallResult {
                                tool_call_id: call_id.clone(),
                                context: None,
                                result: None,
                            },
                        )),
                    }],
                ));
                vec![self.transaction(actions)]
            }
            RuntimeEvent::Finished { reason } => {
                let proto_reason = match reason {
                    FinishReason::Done
                    | FinishReason::MaxTurns
                    | FinishReason::Cancelled
                    | FinishReason::AwaitingClientToolResults => {
                        api::response_event::stream_finished::Reason::Done(
                            api::response_event::stream_finished::Done {},
                        )
                    }
                    FinishReason::Error(msg) => {
                        api::response_event::stream_finished::Reason::InternalError(
                            api::response_event::stream_finished::InternalError {
                                message: msg.clone(),
                            },
                        )
                    }
                };
                vec![finished_event(proto_reason)]
            }
            // Surface recoverable runtime warnings (model/tool issues) as agent text so the
            // conversation isn't stuck on "Warping..." with no detail.
            RuntimeEvent::Warning { message } => {
                if message.is_empty() {
                    return vec![];
                }
                let message_id = Uuid::new_v4().to_string();
                let action = add_agent_output(
                    &self.task_id,
                    &message_id,
                    &self.request_id,
                    &format!("**Runtime warning:** {message}"),
                );
                vec![self.transaction(vec![action])]
            }
            // Permission and execution progress for client tools is shown by the client's own
            // action cards; in-process tools surface through ToolCall / ToolCallResult messages.
            RuntimeEvent::TurnCompleted { .. }
            | RuntimeEvent::PermissionRequired { .. }
            | RuntimeEvent::ToolExecutionStarted { .. } => vec![],
        }
    }

    fn tool_call_messages(&mut self, calls: &[ToolCall]) -> Vec<api::ResponseEvent> {
        let actions = calls
            .iter()
            .filter_map(|call| {
                tool_call_to_proto_action(&self.task_id, &self.request_id, call, &self.registry)
            })
            .collect::<Vec<_>>();
        if actions.is_empty() {
            return vec![];
        }
        vec![self.transaction(actions)]
    }
}

/// Convert a runtime ToolCall to a proto ClientAction (AddMessagesToTask with ToolCall message).
///
/// Local todo tools have no wire proto tool form; we still persist a ToolCall message with
/// the runtime transcript envelope so pairs restore. Other local-only tools (web, git,
/// list_skills) keep results in the runtime loop only.
fn tool_call_to_proto_action(
    task_id: &str,
    request_id: &str,
    call: &ToolCall,
    registry: &LocalRuntimeToolRegistry,
) -> Option<api::ClientAction> {
    if !registry.contains_tool(&call.name) {
        return None;
    }
    let proto_tool = tool_call_to_proto_tool_with_registry(call, registry).ok();
    if proto_tool.is_none() && call.name != "update_todos" && call.name != "mark_todos_completed" {
        return None;
    }

    Some(add_messages(
        task_id,
        vec![api::Message {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            request_id: request_id.to_string(),
            timestamp: None,
            server_message_data: encode_local_runtime_tool_call_data(call),
            citations: vec![],
            fetched_memories: vec![],
            message: Some(api::message::Message::ToolCall(api::message::ToolCall {
                tool_call_id: call.id.clone(),
                tool: proto_tool,
            })),
        }],
    ))
}

pub fn stream_init_event(
    conversation_id: &str,
    request_id: &str,
    run_id: &str,
) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Init(
            api::response_event::StreamInit {
                conversation_id: conversation_id.to_string(),
                request_id: request_id.to_string(),
                run_id: run_id.to_string(),
            },
        )),
    }
}

pub fn client_actions_event(actions: Vec<api::ClientAction>) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::ClientActions(
            api::response_event::ClientActions { actions },
        )),
    }
}

pub fn finished_event(reason: api::response_event::stream_finished::Reason) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Finished(
            api::response_event::StreamFinished {
                reason: Some(reason),
                ..Default::default()
            },
        )),
    }
}

pub fn add_messages(task_id: &str, messages: Vec<api::Message>) -> api::ClientAction {
    api::ClientAction {
        action: Some(api::client_action::Action::AddMessagesToTask(
            api::client_action::AddMessagesToTask {
                task_id: task_id.to_string(),
                messages,
            },
        )),
    }
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

fn agent_output_message(
    task_id: &str,
    message_id: &str,
    request_id: &str,
    text: &str,
) -> api::Message {
    api::Message {
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
    }
}

fn add_agent_output(
    task_id: &str,
    message_id: &str,
    request_id: &str,
    text: &str,
) -> api::ClientAction {
    add_messages(
        task_id,
        vec![agent_output_message(task_id, message_id, request_id, text)],
    )
}

fn append_agent_output(
    task_id: &str,
    message_id: &str,
    request_id: &str,
    text: &str,
) -> api::ClientAction {
    api::ClientAction {
        action: Some(api::client_action::Action::AppendToMessageContent(
            api::client_action::AppendToMessageContent {
                task_id: task_id.to_string(),
                message: Some(agent_output_message(task_id, message_id, request_id, text)),
                mask: Some(prost_types::FieldMask {
                    paths: vec!["agent_output.text".to_string()],
                }),
            },
        )),
    }
}

#[cfg(test)]
#[path = "event_mapper_tests.rs"]
mod tests;
