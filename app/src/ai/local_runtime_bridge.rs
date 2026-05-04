//! Bridge between `local_agent_runtime` and Warp's existing tool execution pipeline.
//!
//! This module implements the `ToolExecutor` trait from the runtime crate,
//! translating between:
//! - `local_agent_runtime::ToolCall` ↔ `ai::agent::action::AIAgentActionType`
//! - `local_agent_runtime::ToolCallResult` ↔ proto `ToolCallResult`
//! - `local_agent_runtime::ToolSchema` ↔ OpenAI function schemas
//!
//! It also provides the `RuntimeBridge` that maps `RuntimeEvent`s to
//! Warp's `ResponseEvent` stream so the existing controller/transcript
//! pipeline works unchanged.

use futures::channel::oneshot;
use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::tools::{PermissionDecision, ToolCall, ToolCallResult};
use local_agent_runtime::{ToolExecutionError, ToolExecutor};
use serde_json::Value;
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::ai::agent::task::TaskId;
use crate::ai::agent::{
    AIAgentAction, AIAgentActionId, AIAgentActionResult, AIAgentActionResultType,
    AIAgentActionType, AnyFileContent, FileContext, FileGlobV2Result, GrepResult, ReadFilesResult,
    RequestCommandOutputResult, SearchCodebaseResult,
};

/// Request sent from the runtime task to the app/UI model task for real Warp tool execution.
pub struct ToolExecutionRequest {
    pub call: ToolCall,
    pub task_id: TaskId,
    pub request_id: String,
    pub response_tx: oneshot::Sender<Result<ToolCallResult, ToolExecutionError>>,
}

pub struct WarpToolExecutor {
    /// Tools available in the current session.
    tools: Vec<ToolSchema>,
    request_tx: async_channel::Sender<ToolExecutionRequest>,
    task_id: TaskId,
    request_id: String,
}

impl WarpToolExecutor {
    pub fn new(
        request_tx: async_channel::Sender<ToolExecutionRequest>,
        task_id: TaskId,
        request_id: String,
    ) -> Self {
        Self {
            tools: build_tool_schemas(),
            request_tx,
            task_id,
            request_id,
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for WarpToolExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        self.tools.clone()
    }

    async fn check_permission(&self, call: &ToolCall) -> PermissionDecision {
        if is_supported_tool(&call.name) {
            // Warp's action model owns the real permission/autocomplete decision and may block on
            // user confirmation. The runtime should still enter `execute` so that existing UI path
            // can queue and resolve the action.
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                reason: format!("Unsupported local Ollama tool: {}", call.name),
            }
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.request_tx
            .send(ToolExecutionRequest {
                call: call.clone(),
                task_id: self.task_id.clone(),
                request_id: self.request_id.clone(),
                response_tx,
            })
            .await
            .map_err(|_| {
                ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                    "Local runtime tool bridge is closed"
                ))
            })?;

        response_rx.await.map_err(|_| {
            ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                "Local runtime tool result channel closed"
            ))
        })?
    }

    async fn on_permission_response(&self, _call: &ToolCall, granted: bool) -> PermissionDecision {
        if granted {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                reason: "User denied".to_string(),
            }
        }
    }
}

/// Build the standard set of tool schemas that can be advertised to the LLM.
///
/// These correspond to `api::ToolType` values and match what Warp's backend
/// advertises — but expressed as OpenAI function-calling schemas for local use.
pub fn build_tool_schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchemaBuilder::new(
            "run_shell_command",
            "Run a shell command in the user's terminal. Use this for any system operation.",
        )
        .required_string("command", "The shell command to execute")
        .optional_bool("is_read_only", "Whether the command is expected to avoid side effects")
        .optional_bool("is_risky", "Whether the command should require user confirmation")
        .optional_bool("uses_pager", "Whether the command may open a pager")
        .build(),
        ToolSchemaBuilder::new(
            "read_files",
            "Read the contents of one or more files. Returns file content with line numbers.",
        )
        .required_string_array(
            "paths",
            "File paths to read (absolute or relative to the current working directory)",
        )
        .build(),
        ToolSchemaBuilder::new(
            "grep",
            "Search file contents using a regex pattern. Returns matching lines with file paths.",
        )
        .required_string_array("queries", "Regex patterns to search for")
        .optional_string("path", "Directory to search in (defaults to cwd)")
        .build(),
        ToolSchemaBuilder::new(
            "file_glob_v2",
            "Find files matching one or more glob patterns.",
        )
        .required_string_array(
            "patterns",
            "Glob patterns such as '**/*.rs' or 'src/**/*.ts'",
        )
        .optional_string("search_dir", "Base directory to search from")
        .build(),
        ToolSchemaBuilder::new(
            "search_codebase",
            "Semantic search across the codebase. Use for finding concepts, functions, or implementations.",
        )
        .required_string("query", "Natural language search query")
        .optional_string_array("path_filters", "Optional path prefixes to restrict the search")
        .optional_string("codebase_path", "Optional codebase root path")
        .build(),
    ]
}

pub fn is_supported_tool(name: &str) -> bool {
    matches!(
        name,
        "run_shell_command" | "read_files" | "grep" | "file_glob_v2" | "search_codebase"
    )
}

pub fn tool_call_to_ai_action(
    call: &ToolCall,
    task_id: &TaskId,
) -> Result<AIAgentAction, ToolExecutionError> {
    let action: AIAgentActionType = match tool_call_to_proto_tool(call)? {
        api::message::tool_call::Tool::RunShellCommand(tool) => tool.into(),
        api::message::tool_call::Tool::ReadFiles(tool) => tool.into(),
        api::message::tool_call::Tool::Grep(tool) => tool.into(),
        api::message::tool_call::Tool::FileGlobV2(tool) => tool.into(),
        api::message::tool_call::Tool::SearchCodebase(tool) => tool.into(),
        _ => {
            return Err(ToolExecutionError::NotFound {
                name: call.name.clone(),
            });
        }
    };

    Ok(AIAgentAction {
        id: AIAgentActionId::from(call.id.clone()),
        task_id: task_id.clone(),
        action,
        requires_result: true,
    })
}

pub fn action_result_to_tool_result(result: &AIAgentActionResult) -> ToolCallResult {
    ToolCallResult {
        content: action_result_to_content(&result.result),
        is_error: !result.result.is_successful(),
    }
}

pub fn action_result_to_tool_call_result_client_actions(
    result: &AIAgentActionResult,
    request_id: &str,
) -> Option<Vec<api::ClientAction>> {
    let proto_result =
        action_result_to_proto_tool_call_result_type(&result.result).or_else(|| {
            result
                .result
                .is_cancelled()
                .then_some(proto_tool_call_cancel_result())
        })?;

    let message = api::Message {
        id: Uuid::new_v4().to_string(),
        task_id: result.task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: None,
        server_message_data: String::new(),
        citations: vec![],
        message: Some(api::message::Message::ToolCallResult(
            api::message::ToolCallResult {
                tool_call_id: result.id.to_string(),
                context: None,
                result: Some(proto_result),
            },
        )),
    };

    Some(vec![
        begin_transaction(),
        api::ClientAction {
            action: Some(api::client_action::Action::AddMessagesToTask(
                api::client_action::AddMessagesToTask {
                    task_id: result.task_id.to_string(),
                    messages: vec![message],
                },
            )),
        },
        commit_transaction(),
    ])
}

fn action_result_to_proto_tool_call_result_type(
    result: &AIAgentActionResultType,
) -> Option<api::message::tool_call_result::Result> {
    use api::message::tool_call_result::Result as MessageResult;
    use api::request::input::tool_call_result::Result as RequestResult;

    let request_result: RequestResult = match result.clone() {
        AIAgentActionResultType::RequestCommandOutput(result) => result.try_into().ok()?,
        AIAgentActionResultType::ReadFiles(result) => result.try_into().ok()?,
        AIAgentActionResultType::SearchCodebase(result) => result.try_into().ok()?,
        AIAgentActionResultType::Grep(result) => result.try_into().ok()?,
        AIAgentActionResultType::FileGlobV2(result) => result.try_into().ok()?,
        _ => return None,
    };

    match request_result {
        RequestResult::RunShellCommand(result) => Some(MessageResult::RunShellCommand(result)),
        RequestResult::ReadFiles(result) => Some(MessageResult::ReadFiles(result)),
        RequestResult::SearchCodebase(result) => Some(MessageResult::SearchCodebase(result)),
        RequestResult::Grep(result) => Some(MessageResult::Grep(result)),
        RequestResult::FileGlobV2(result) => Some(MessageResult::FileGlobV2(result)),
        _ => None,
    }
}

fn proto_tool_call_cancel_result() -> api::message::tool_call_result::Result {
    api::message::tool_call_result::Result::Cancel(())
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

fn action_result_to_content(result: &AIAgentActionResultType) -> String {
    match result {
        AIAgentActionResultType::RequestCommandOutput(RequestCommandOutputResult::Completed {
            command,
            output,
            exit_code,
            ..
        }) => serde_json::json!({
            "command": command,
            "exit_code": exit_code.value(),
            "output": output,
        })
        .to_string(),
        AIAgentActionResultType::ReadFiles(ReadFilesResult::Success { files }) => {
            file_contexts_to_json(files)
        }
        AIAgentActionResultType::SearchCodebase(SearchCodebaseResult::Success { files }) => {
            file_contexts_to_json(files)
        }
        AIAgentActionResultType::Grep(GrepResult::Success { matched_files }) => {
            serde_json::json!({ "matched_files": matched_files }).to_string()
        }
        AIAgentActionResultType::FileGlobV2(FileGlobV2Result::Success {
            matched_files,
            warnings,
        }) => serde_json::json!({
            "matched_files": matched_files,
            "warnings": warnings,
        })
        .to_string(),
        _ => result.to_string(),
    }
}

fn file_contexts_to_json(files: &[FileContext]) -> String {
    let files = files
        .iter()
        .map(|file| {
            let content = match &file.content {
                AnyFileContent::StringContent(content) => Value::String(content.clone()),
                AnyFileContent::BinaryContent(_) => Value::String("<binary content>".to_string()),
            };
            serde_json::json!({
                "path": file.file_name,
                "content": content,
                "line_range": file.line_range.as_ref().map(|range| {
                    serde_json::json!({
                        "start": range.start,
                        "end": range.end,
                    })
                }),
                "line_count": file.line_count,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "files": files }).to_string()
}

pub fn tool_call_to_proto_tool(
    call: &ToolCall,
) -> Result<api::message::tool_call::Tool, ToolExecutionError> {
    use api::message::tool_call::Tool;

    match call.name.as_str() {
        "run_shell_command" => Ok(Tool::RunShellCommand(
            api::message::tool_call::RunShellCommand {
                command: required_string(&call.arguments, "command", &call.name)?,
                is_read_only: optional_bool(&call.arguments, "is_read_only").unwrap_or(false),
                is_risky: optional_bool(&call.arguments, "is_risky").unwrap_or(false),
                uses_pager: optional_bool(&call.arguments, "uses_pager").unwrap_or(false),
                ..Default::default()
            },
        )),
        "read_files" => Ok(Tool::ReadFiles(api::message::tool_call::ReadFiles {
            files: required_string_array(&call.arguments, &["paths", "files"], &call.name)?
                .into_iter()
                .map(|path| api::message::tool_call::read_files::File {
                    name: path,
                    line_ranges: vec![],
                })
                .collect(),
        })),
        "grep" => Ok(Tool::Grep(api::message::tool_call::Grep {
            queries: required_string_array(&call.arguments, &["queries", "patterns"], &call.name)
                .or_else(|_| {
                required_string(&call.arguments, "pattern", &call.name).map(|p| vec![p])
            })?,
            path: optional_string(&call.arguments, "path").unwrap_or_else(|| ".".to_string()),
        })),
        "file_glob_v2" => Ok(Tool::FileGlobV2(api::message::tool_call::FileGlobV2 {
            patterns: required_string_array(&call.arguments, &["patterns"], &call.name).or_else(
                |_| required_string(&call.arguments, "pattern", &call.name).map(|p| vec![p]),
            )?,
            search_dir: optional_string(&call.arguments, "search_dir")
                .or_else(|| optional_string(&call.arguments, "path"))
                .unwrap_or_default(),
            min_depth: 0,
            max_depth: 0,
            max_matches: 0,
        })),
        "search_codebase" => Ok(Tool::SearchCodebase(
            api::message::tool_call::SearchCodebase {
                query: required_string(&call.arguments, "query", &call.name)?,
                path_filters: optional_string_array(&call.arguments, "path_filters")
                    .unwrap_or_default(),
                codebase_path: optional_string(&call.arguments, "codebase_path")
                    .unwrap_or_default(),
            },
        )),
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

fn required_string(
    arguments: &Value,
    name: &str,
    tool_name: &str,
) -> Result<String, ToolExecutionError> {
    optional_string(arguments, name).ok_or_else(|| ToolExecutionError::InvalidInput {
        reason: format!("Tool `{tool_name}` requires string argument `{name}`"),
    })
}

fn optional_string(arguments: &Value, name: &str) -> Option<String> {
    arguments
        .get(name)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn optional_bool(arguments: &Value, name: &str) -> Option<bool> {
    arguments.get(name).and_then(|value| value.as_bool())
}

fn required_string_array(
    arguments: &Value,
    names: &[&str],
    tool_name: &str,
) -> Result<Vec<String>, ToolExecutionError> {
    names
        .iter()
        .find_map(|name| optional_string_array(arguments, name))
        .filter(|values| !values.is_empty())
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!(
                "Tool `{tool_name}` requires non-empty string array argument `{}`",
                names.join("` or `")
            ),
        })
}

fn optional_string_array(arguments: &Value, name: &str) -> Option<Vec<String>> {
    let value = arguments.get(name)?;
    if let Some(values) = value.as_array() {
        return Some(
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
        );
    }
    let value = value.as_str()?;
    serde_json::from_str::<Vec<String>>(value)
        .ok()
        .or_else(|| Some(vec![value.to_string()]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::agent::{AIAgentActionResultType, FileContext, ReadFilesResult};

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn schemas_advertise_only_supported_v1_tools() {
        let names = build_tool_schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "run_shell_command",
                "read_files",
                "grep",
                "file_glob_v2",
                "search_codebase"
            ]
        );
        assert!(!names.iter().any(|name| name == "edit_files"));
        assert!(!names.iter().any(|name| name == "create_file"));
    }

    #[test]
    fn tool_calls_map_to_warp_actions() {
        let task_id = TaskId::new("task_1".to_string());

        let shell_action = tool_call_to_ai_action(
            &call("run_shell_command", serde_json::json!({"command": "pwd"})),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            shell_action.action,
            AIAgentActionType::RequestCommandOutput { ref command, .. } if command == "pwd"
        ));

        let read_action = tool_call_to_ai_action(
            &call("read_files", serde_json::json!({"paths": ["src/lib.rs"]})),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            read_action.action,
            AIAgentActionType::ReadFiles(ref request)
                if request.locations.iter().any(|location| location.name == "src/lib.rs")
        ));

        let grep_action = tool_call_to_ai_action(
            &call(
                "grep",
                serde_json::json!({"queries": ["needle"], "path": "src"}),
            ),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            grep_action.action,
            AIAgentActionType::Grep { ref queries, ref path }
                if queries == &vec!["needle".to_string()] && path == "src"
        ));

        let glob_action = tool_call_to_ai_action(
            &call(
                "file_glob_v2",
                serde_json::json!({"patterns": ["**/*.rs"], "search_dir": "crates"}),
            ),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            glob_action.action,
            AIAgentActionType::FileGlobV2 { ref patterns, ref search_dir }
                if patterns == &vec!["**/*.rs".to_string()] && search_dir.as_deref() == Some("crates")
        ));

        let search_action = tool_call_to_ai_action(
            &call(
                "search_codebase",
                serde_json::json!({"query": "runtime loop"}),
            ),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            search_action.action,
            AIAgentActionType::SearchCodebase(ref request) if request.query == "runtime loop"
        ));
    }

    #[test]
    fn unsupported_tool_is_not_silently_mapped() {
        let task_id = TaskId::new("task_1".to_string());
        let err = tool_call_to_ai_action(
            &call("edit_files", serde_json::json!({"path": "src/lib.rs"})),
            &task_id,
        )
        .unwrap_err();

        assert!(matches!(err, ToolExecutionError::NotFound { name } if name == "edit_files"));
    }

    #[test]
    fn action_result_content_includes_real_file_content() {
        let result = AIAgentActionResult {
            id: AIAgentActionId::from("call_1".to_string()),
            task_id: TaskId::new("task_1".to_string()),
            result: AIAgentActionResultType::ReadFiles(ReadFilesResult::Success {
                files: vec![FileContext::new(
                    "src/lib.rs".to_string(),
                    AnyFileContent::StringContent("fn main() {}".to_string()),
                    None,
                    None,
                )],
            }),
        };

        let tool_result = action_result_to_tool_result(&result);
        assert!(!tool_result.is_error);
        assert!(tool_result.content.contains("src/lib.rs"));
        assert!(tool_result.content.contains("fn main() {}"));
    }

    #[test]
    fn action_result_client_actions_persist_tool_call_result_message() {
        let result = AIAgentActionResult {
            id: AIAgentActionId::from("call_1".to_string()),
            task_id: TaskId::new("task_1".to_string()),
            result: AIAgentActionResultType::ReadFiles(ReadFilesResult::Success {
                files: vec![FileContext::new(
                    "src/lib.rs".to_string(),
                    AnyFileContent::StringContent("fn main() {}".to_string()),
                    None,
                    None,
                )],
            }),
        };

        let actions =
            action_result_to_tool_call_result_client_actions(&result, "request_1").unwrap();

        assert_eq!(actions.len(), 3);
        let Some(api::client_action::Action::BeginTransaction(_)) = &actions[0].action else {
            panic!("expected begin transaction");
        };
        let Some(api::client_action::Action::AddMessagesToTask(add_messages)) = &actions[1].action
        else {
            panic!("expected AddMessagesToTask action");
        };
        let Some(api::client_action::Action::CommitTransaction(_)) = &actions[2].action else {
            panic!("expected commit transaction");
        };
        assert_eq!(add_messages.task_id, "task_1");
        assert_eq!(add_messages.messages.len(), 1);

        let message = &add_messages.messages[0];
        assert_eq!(message.task_id, "task_1");
        assert_eq!(message.request_id, "request_1");
        let Some(api::message::Message::ToolCallResult(tool_call_result)) = &message.message else {
            panic!("expected persisted ToolCallResult message");
        };
        assert_eq!(tool_call_result.tool_call_id, "call_1");
        assert!(matches!(
            &tool_call_result.result,
            Some(api::message::tool_call_result::Result::ReadFiles(_))
        ));
    }

    #[test]
    fn cancelled_action_result_persists_cancel_tool_call_result() {
        let result = AIAgentActionResult {
            id: AIAgentActionId::from("call_1".to_string()),
            task_id: TaskId::new("task_1".to_string()),
            result: AIAgentActionResultType::ReadFiles(ReadFilesResult::Cancelled),
        };

        let actions =
            action_result_to_tool_call_result_client_actions(&result, "request_1").unwrap();
        let Some(api::client_action::Action::AddMessagesToTask(add_messages)) = &actions[1].action
        else {
            panic!("expected AddMessagesToTask action");
        };
        let Some(api::message::Message::ToolCallResult(tool_call_result)) =
            &add_messages.messages[0].message
        else {
            panic!("expected persisted ToolCallResult message");
        };

        assert_eq!(tool_call_result.tool_call_id, "call_1");
        assert!(matches!(
            &tool_call_result.result,
            Some(api::message::tool_call_result::Result::Cancel(()))
        ));
    }
}

/// Maps a `local_agent_runtime::RuntimeEvent` stream to Warp `ResponseEvent`s.
///
/// This is used by the local runtime integration to make the runtime's output
/// compatible with the existing controller/transcript pipeline.
pub mod event_mapper {
    use local_agent_runtime::{FinishReason, RuntimeEvent};
    use uuid::Uuid;
    use warp_multi_agent_api as api;

    use super::tool_call_to_proto_tool;

    /// State for mapping runtime events to proto ResponseEvents.
    pub struct EventMapper {
        pub conversation_id: String,
        pub request_id: String,
        pub run_id: String,
        pub task_id: String,
        task_created: bool,
    }

    impl EventMapper {
        pub fn new(
            conversation_id: String,
            request_id: String,
            run_id: String,
            task_id: String,
            task_exists: bool,
        ) -> Self {
            Self {
                conversation_id,
                request_id,
                run_id,
                task_id,
                task_created: task_exists,
            }
        }

        /// Map a single RuntimeEvent to zero or more ResponseEvents.
        pub fn map_event(&mut self, event: &RuntimeEvent) -> Vec<api::ResponseEvent> {
            match event {
                RuntimeEvent::TurnStarted { turn } => {
                    if *turn == 1 {
                        // Emit Init on first turn
                        vec![api::ResponseEvent {
                            r#type: Some(api::response_event::Type::Init(
                                api::response_event::StreamInit {
                                    conversation_id: self.conversation_id.clone(),
                                    request_id: self.request_id.clone(),
                                    run_id: self.run_id.clone(),
                                },
                            )),
                        }]
                    } else {
                        vec![]
                    }
                }
                RuntimeEvent::TextCompleted { text } => {
                    let mut actions = Vec::new();
                    actions.push(begin_transaction());

                    if !self.task_created {
                        actions.push(create_task(&self.task_id));
                        self.task_created = true;
                    }

                    let message_id = Uuid::new_v4().to_string();
                    actions.push(add_agent_output(
                        &self.task_id,
                        &message_id,
                        &self.request_id,
                        text,
                    ));
                    actions.push(commit_transaction());

                    vec![api::ResponseEvent {
                        r#type: Some(api::response_event::Type::ClientActions(
                            api::response_event::ClientActions { actions },
                        )),
                    }]
                }
                RuntimeEvent::ToolCallsRequested { calls } => {
                    let mut actions = Vec::new();
                    actions.push(begin_transaction());

                    if !self.task_created {
                        actions.push(create_task(&self.task_id));
                        self.task_created = true;
                    }

                    for call in calls {
                        if let Some(action) =
                            tool_call_to_proto_action(&self.task_id, &self.request_id, call)
                        {
                            actions.push(action);
                        }
                    }

                    actions.push(commit_transaction());

                    vec![api::ResponseEvent {
                        r#type: Some(api::response_event::Type::ClientActions(
                            api::response_event::ClientActions { actions },
                        )),
                    }]
                }
                RuntimeEvent::Finished { reason } => {
                    let proto_reason = match reason {
                        FinishReason::Done | FinishReason::MaxTurns => {
                            api::response_event::stream_finished::Reason::Done(
                                api::response_event::stream_finished::Done {},
                            )
                        }
                        FinishReason::Cancelled => {
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

                    vec![api::ResponseEvent {
                        r#type: Some(api::response_event::Type::Finished(
                            api::response_event::StreamFinished {
                                reason: Some(proto_reason),
                                ..Default::default()
                            },
                        )),
                    }]
                }
                // Other events (ToolResult, PermissionRequired, etc.) are handled
                // by the app's existing action execution pipeline — they don't need
                // to be mapped to ResponseEvents since the controller processes them
                // directly.
                _ => vec![],
            }
        }
    }

    /// Convert a runtime ToolCall to a proto ClientAction (AddMessagesToTask with ToolCall message).
    fn tool_call_to_proto_action(
        task_id: &str,
        request_id: &str,
        call: &local_agent_runtime::ToolCall,
    ) -> Option<api::ClientAction> {
        let proto_tool = tool_call_to_proto_tool(call).ok()?;

        let message_id = Uuid::new_v4().to_string();
        let message = api::Message {
            id: message_id,
            task_id: task_id.to_string(),
            request_id: request_id.to_string(),
            timestamp: None,
            server_message_data: String::new(),
            citations: vec![],
            message: Some(api::message::Message::ToolCall(api::message::ToolCall {
                tool_call_id: call.id.clone(),
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
}
