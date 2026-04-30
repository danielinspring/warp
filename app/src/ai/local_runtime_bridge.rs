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

use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::tools::{PermissionDecision, ToolCall, ToolCallResult};
use local_agent_runtime::{ToolExecutionError, ToolExecutor};

/// Implements `ToolExecutor` for use within the Warp app.
///
/// In a full implementation, this would hold references to:
/// - The active session (for running shell commands)
/// - The blocklist/permissions model
/// - The MCP server manager
/// - File system access
///
/// For now, it provides the tool schema definitions and a placeholder
/// execution path that the `wire-into-warp` phase will complete.
pub struct WarpToolExecutor {
    /// Tools available in the current session.
    tools: Vec<ToolSchema>,
}

impl WarpToolExecutor {
    /// Create a new executor with the standard set of agent tools.
    pub fn new() -> Self {
        Self {
            tools: build_tool_schemas(),
        }
    }

    /// Create with a custom set of tools (for testing or restricted modes).
    pub fn with_tools(tools: Vec<ToolSchema>) -> Self {
        Self { tools }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for WarpToolExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        self.tools.clone()
    }

    async fn check_permission(&self, _call: &ToolCall) -> PermissionDecision {
        // TODO: Wire into BlocklistAIPermissions.
        // For now, all tools require asking (safest default).
        PermissionDecision::Ask
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
        // TODO: Wire into Warp's action_model/execute/ pipeline.
        // This requires an AppContext/ModelContext which is passed at runtime.
        // For now, return a placeholder indicating the tool is not yet wired.
        Err(ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
            "Tool '{}' execution not yet wired to Warp action pipeline",
            call.name
        )))
    }

    async fn on_permission_response(
        &self,
        _call: &ToolCall,
        granted: bool,
    ) -> PermissionDecision {
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
fn build_tool_schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchemaBuilder::new(
            "run_shell_command",
            "Run a shell command in the user's terminal. Use this for any system operation.",
        )
        .required_string("command", "The shell command to execute")
        .build(),
        ToolSchemaBuilder::new(
            "read_files",
            "Read the contents of one or more files. Returns file content with line numbers.",
        )
        .required_string(
            "paths",
            "JSON array of file paths to read (absolute or relative to cwd)",
        )
        .build(),
        ToolSchemaBuilder::new(
            "edit_files",
            "Apply edits to files using search-and-replace blocks.",
        )
        .required_string("file_path", "The path of the file to edit")
        .required_string("old_content", "The exact content to find and replace")
        .required_string("new_content", "The replacement content")
        .build(),
        ToolSchemaBuilder::new(
            "create_file",
            "Create a new file with the given content.",
        )
        .required_string("path", "The path for the new file")
        .required_string("content", "The file content to write")
        .build(),
        ToolSchemaBuilder::new(
            "grep",
            "Search file contents using a regex pattern. Returns matching lines with file paths.",
        )
        .required_string("pattern", "The regex pattern to search for")
        .optional_string("path", "Directory to search in (defaults to cwd)")
        .build(),
        ToolSchemaBuilder::new(
            "file_glob",
            "Find files matching a glob pattern.",
        )
        .required_string("pattern", "The glob pattern (e.g., '**/*.rs', 'src/**/*.ts')")
        .optional_string("path", "Base directory to search from")
        .build(),
        ToolSchemaBuilder::new(
            "search_codebase",
            "Semantic search across the codebase. Use for finding concepts, functions, or implementations.",
        )
        .required_string("query", "Natural language search query")
        .build(),
    ]
}

/// Maps a `local_agent_runtime::RuntimeEvent` stream to Warp `ResponseEvent`s.
///
/// This is used by the integration point in `agent/api/impl.rs` to make the
/// runtime's output compatible with the existing controller/transcript pipeline.
pub mod event_mapper {
    use local_agent_runtime::{FinishReason, RuntimeEvent, StopReason};
    use uuid::Uuid;
    use warp_multi_agent_api as api;

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
        use api::message::tool_call::Tool;

        let proto_tool = match call.name.as_str() {
            "run_shell_command" => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Tool::RunShellCommand(api::message::tool_call::RunShellCommand {
                    command,
                    ..Default::default()
                })
            }
            "read_files" => {
                let paths_str = call
                    .arguments
                    .get("paths")
                    .and_then(|v| v.as_str())
                    .unwrap_or("[]");
                let file_paths: Vec<String> = serde_json::from_str(paths_str).unwrap_or_default();
                Tool::ReadFiles(api::message::tool_call::ReadFiles {
                    files: file_paths
                        .into_iter()
                        .map(|p| api::message::tool_call::read_files::File {
                            name: p,
                            line_ranges: vec![],
                        })
                        .collect(),
                })
            }
            "grep" => {
                let pattern = call
                    .arguments
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string();
                Tool::Grep(api::message::tool_call::Grep {
                    queries: vec![pattern],
                    path,
                })
            }
            "file_glob" => {
                let pattern = call
                    .arguments
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string();
                Tool::FileGlob(api::message::tool_call::FileGlob {
                    patterns: vec![pattern],
                    path,
                })
            }
            // For unhandled tools, return None — they'll be surfaced as text.
            _ => return None,
        };

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
