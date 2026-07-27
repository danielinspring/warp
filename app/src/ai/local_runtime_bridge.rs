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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ai::agent::action::FileEdit;
use ai::diff_validation::ParsedDiff;
use ai::skills::SkillReference;
use futures::channel::oneshot;
use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::tools::{PermissionDecision, ToolCall, ToolCallResult, ToolSafetyClass};
use local_agent_runtime::{ToolExecutionError, ToolExecutor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use warp_multi_agent_api as api;
use warp_util::local_or_remote_path::LocalOrRemotePath;

use crate::ai::agent::api::RequestParams;
use crate::ai::agent::task::TaskId;
use crate::ai::agent::{
    AIAgentAction, AIAgentActionId, AIAgentActionResult, AIAgentActionResultType,
    AIAgentActionType, AIAgentInput, AnyFileContent, AskUserQuestionItem, AskUserQuestionOption,
    AskUserQuestionType, FileContext, FileGlobV2Result, GrepResult, ReadFilesResult,
    RequestCommandOutputResult, SearchCodebaseResult,
};

/// Request sent from the runtime task to the app/UI model task for real Warp tool execution.
pub struct ToolExecutionRequest {
    pub call: ToolCall,
    pub registry: Arc<LocalRuntimeToolRegistry>,
    pub task_id: TaskId,
    pub request_id: String,
    pub response_tx: oneshot::Sender<Result<ToolCallResult, ToolExecutionError>>,
}

const LOCAL_RUNTIME_TRANSCRIPT_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct LocalRuntimeTranscriptData {
    version: u8,
    message: LocalRuntimeTranscriptMessage,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalRuntimeTranscriptMessage {
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        call_id: String,
        result: ToolCallResult,
    },
}

pub fn encode_local_runtime_tool_call_data(call: &ToolCall) -> String {
    serde_json::to_string(&LocalRuntimeTranscriptData {
        version: LOCAL_RUNTIME_TRANSCRIPT_VERSION,
        message: LocalRuntimeTranscriptMessage::ToolCall { call: call.clone() },
    })
    .unwrap_or_default()
}

pub fn encode_local_runtime_tool_result_data(
    call_id: impl Into<String>,
    result: &ToolCallResult,
) -> String {
    serde_json::to_string(&LocalRuntimeTranscriptData {
        version: LOCAL_RUNTIME_TRANSCRIPT_VERSION,
        message: LocalRuntimeTranscriptMessage::ToolResult {
            call_id: call_id.into(),
            result: result.clone(),
        },
    })
    .unwrap_or_default()
}

pub fn decode_local_runtime_tool_call_data(data: &str) -> Option<ToolCall> {
    let data: LocalRuntimeTranscriptData = serde_json::from_str(data).ok()?;
    if data.version != LOCAL_RUNTIME_TRANSCRIPT_VERSION {
        return None;
    }
    match data.message {
        LocalRuntimeTranscriptMessage::ToolCall { call } => Some(call),
        LocalRuntimeTranscriptMessage::ToolResult { .. } => None,
    }
}

pub fn decode_local_runtime_tool_result_data(data: &str) -> Option<(String, ToolCallResult)> {
    let data: LocalRuntimeTranscriptData = serde_json::from_str(data).ok()?;
    if data.version != LOCAL_RUNTIME_TRANSCRIPT_VERSION {
        return None;
    }
    match data.message {
        LocalRuntimeTranscriptMessage::ToolResult { call_id, result } => Some((call_id, result)),
        LocalRuntimeTranscriptMessage::ToolCall { .. } => None,
    }
}

#[derive(Debug, Clone)]
pub struct LocalRuntimeToolRegistry {
    schemas: Vec<ToolSchema>,
    routes: HashMap<String, LocalRuntimeToolRoute>,
    permission_mode: LocalRuntimePermissionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRuntimePermissionMode {
    Default,
    AcceptEdits,
    Plan,
}

#[derive(Debug, Clone)]
pub struct LocalRuntimeToolRoute {
    safety_class: ToolSafetyClass,
    kind: LocalRuntimeToolRouteKind,
}

#[derive(Debug, Clone)]
enum LocalRuntimeToolRouteKind {
    BuiltIn,
    McpTool {
        server_id: Option<Uuid>,
        name: String,
    },
    ReadMcpResource,
    ReadSkill {
        skill_lookup: HashMap<String, SkillReference>,
    },
}

impl LocalRuntimeToolRegistry {
    pub fn from_request(params: &RequestParams) -> Self {
        let mut registry = Self::built_ins();
        registry.permission_mode = LocalRuntimePermissionMode::from_request(params);

        if let Some(context) = &params.mcp_context {
            registry.add_mcp_context(context);
        }

        let skills = skill_references_for_request(params);
        if !skills.is_empty() {
            registry.add_read_skill(skills);
        }
        if params.ask_user_question_enabled {
            registry.add_ask_user_question();
        }
        if registry.permission_mode == LocalRuntimePermissionMode::Plan {
            registry.retain_plan_tools();
        }

        registry
    }

    pub fn built_ins() -> Self {
        let mut registry = Self {
            schemas: Vec::new(),
            routes: HashMap::new(),
            permission_mode: LocalRuntimePermissionMode::Default,
        };

        for schema in build_tool_schemas() {
            let safety_class = match schema.name.as_str() {
                "run_shell_command" | "edit_files" => ToolSafetyClass::Interactive,
                "read_files" | "grep" | "file_glob_v2" | "search_codebase" => {
                    ToolSafetyClass::ReadOnly
                }
                _ => ToolSafetyClass::Interactive,
            };
            registry.add_tool(schema, safety_class, LocalRuntimeToolRouteKind::BuiltIn);
        }

        registry
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.schemas.clone()
    }

    pub fn contains_tool(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    pub fn safety_class(&self, name: &str) -> ToolSafetyClass {
        self.routes
            .get(name)
            .map(|route| route.safety_class)
            .unwrap_or(ToolSafetyClass::Interactive)
    }

    pub fn permission_mode(&self) -> LocalRuntimePermissionMode {
        self.permission_mode
    }

    fn route(&self, name: &str) -> Option<&LocalRuntimeToolRoute> {
        self.routes.get(name)
    }

    fn add_tool(
        &mut self,
        schema: ToolSchema,
        safety_class: ToolSafetyClass,
        kind: LocalRuntimeToolRouteKind,
    ) {
        if self.routes.contains_key(&schema.name) {
            return;
        }

        self.routes.insert(
            schema.name.clone(),
            LocalRuntimeToolRoute { safety_class, kind },
        );
        self.schemas.push(schema);
    }

    fn add_mcp_context(&mut self, context: &crate::ai::agent::MCPContext) {
        let mut has_resources = false;

        for server in &context.servers {
            let server_slug = sanitize_function_name(&server.name);
            let server_id = Uuid::parse_str(&server.id).ok();
            has_resources |= !server.resources.is_empty();

            for tool in &server.tools {
                let tool_name = tool.name.to_string();
                let function_name = unique_tool_name(
                    &self.routes,
                    &format!("mcp__{server_slug}__{}", sanitize_function_name(&tool_name)),
                );
                let description = tool
                    .description
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| {
                        format!("Call MCP tool `{tool_name}` on server `{}`", server.name)
                    });
                let parameters = serde_json::to_value(tool.input_schema.as_ref())
                    .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));

                self.add_tool(
                    ToolSchema {
                        name: function_name,
                        description,
                        parameters,
                    },
                    ToolSafetyClass::Interactive,
                    LocalRuntimeToolRouteKind::McpTool {
                        server_id,
                        name: tool_name,
                    },
                );
            }
        }

        #[allow(deprecated)]
        {
            has_resources |= !context.resources.is_empty();
            for tool in &context.tools {
                let tool_name = tool.name.to_string();
                let function_name = unique_tool_name(
                    &self.routes,
                    &format!("mcp__default__{}", sanitize_function_name(&tool_name)),
                );
                let description = tool
                    .description
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("Call MCP tool `{tool_name}`"));
                let parameters = serde_json::to_value(tool.input_schema.as_ref())
                    .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));

                self.add_tool(
                    ToolSchema {
                        name: function_name,
                        description,
                        parameters,
                    },
                    ToolSafetyClass::Interactive,
                    LocalRuntimeToolRouteKind::McpTool {
                        server_id: None,
                        name: tool_name,
                    },
                );
            }
        }

        if has_resources {
            self.add_read_mcp_resource();
        }
    }

    fn add_read_mcp_resource(&mut self) {
        self.add_tool(
            ToolSchemaBuilder::new(
                "read_mcp_resource",
                "Read an active MCP resource by URI or by name.",
            )
            .optional_string("uri", "The MCP resource URI to read")
            .optional_string("name", "The MCP resource name to read")
            .build(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::ReadMcpResource,
        );
    }

    fn add_ask_user_question(&mut self) {
        self.add_tool(
            ask_user_question_schema(),
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
    }

    fn add_read_skill(&mut self, skill_lookup: HashMap<String, SkillReference>) {
        self.add_tool(
            ToolSchemaBuilder::new("read_skill", "Read an available Warp skill by reference.")
                .required_string(
                    "skill",
                    "The skill name or displayed reference to read, such as @warp-skill:name or a SKILL.md path",
                )
                .build(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::ReadSkill { skill_lookup },
        );
    }

    fn retain_plan_tools(&mut self) {
        self.routes.retain(|name, route| {
            route.safety_class == ToolSafetyClass::ReadOnly || name == "ask_user_question"
        });
        self.schemas
            .retain(|schema| self.routes.contains_key(&schema.name));
    }
}

impl LocalRuntimePermissionMode {
    fn from_request(params: &RequestParams) -> Self {
        let plan_mode = params.input.iter().rev().any(|input| {
            matches!(
                input,
                AIAgentInput::UserQuery {
                    user_query_mode: crate::ai::agent::UserQueryMode::Plan,
                    ..
                }
            )
        });
        if plan_mode {
            Self::Plan
        } else if params.autonomy_level == api::AutonomyLevel::Unsupervised {
            Self::AcceptEdits
        } else {
            Self::Default
        }
    }
}

pub struct WarpToolExecutor {
    /// Tools available in the current session.
    registry: Arc<LocalRuntimeToolRegistry>,
    request_tx: async_channel::Sender<ToolExecutionRequest>,
    task_id: TaskId,
    request_id: String,
}

impl WarpToolExecutor {
    pub fn new(
        request_tx: async_channel::Sender<ToolExecutionRequest>,
        registry: Arc<LocalRuntimeToolRegistry>,
        task_id: TaskId,
        request_id: String,
    ) -> Self {
        Self {
            registry,
            request_tx,
            task_id,
            request_id,
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for WarpToolExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        self.registry.schemas()
    }

    fn safety_class(&self, tool_name: &str) -> ToolSafetyClass {
        self.registry.safety_class(tool_name)
    }

    fn safety_class_for_call(&self, call: &ToolCall) -> ToolSafetyClass {
        if call.name == "run_shell_command" && shell_command_is_read_only(&call.arguments) {
            ToolSafetyClass::ReadOnly
        } else {
            self.safety_class(&call.name)
        }
    }

    async fn check_permission(&self, call: &ToolCall) -> PermissionDecision {
        if self.registry.permission_mode() == LocalRuntimePermissionMode::Plan
            && self.registry.safety_class(&call.name) != ToolSafetyClass::ReadOnly
            && call.name != "ask_user_question"
        {
            PermissionDecision::Deny {
                reason: format!("Tool `{}` is unavailable in plan mode", call.name),
            }
        } else if self.registry.contains_tool(&call.name) {
            // Warp's action model owns the real permission/autocomplete decision and may block on
            // user confirmation. Default and AcceptEdits both enter `execute`; AcceptEdits only
            // changes Warp's existing UI behavior and never bypasses protected-path, isolation, or
            // risky-command policy.
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
                registry: Arc::clone(&self.registry),
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
            "Run a shell command in the user's terminal. Use this for any system operation. For read-only lookups (find, ls, pwd, cat, mdfind, grep, python3 -c, etc.) set is_read_only=true so the command can auto-run. On macOS prefer python3 (not python). For quick math/scripts use: python3 -c 'print(sum(range(1, 101)))'. To locate a directory by name outside the project, prefer a FAST scoped search — on macOS: mdfind 'kMDItemFSName == \"folder-name\"c' | head -20; or find ~/codish -maxdepth 5 -type d -name 'folder-name' 2>/dev/null. Avoid unbounded find ~ without -maxdepth.",
        )
        .required_string("command", "The shell command to execute")
        .optional_bool("is_read_only", "Set true for commands with no side effects (find, ls, pwd, cat, mdfind, grep, python3 -c). Defaults to an automatic read-only heuristic when omitted.")
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
            "Find files matching glob patterns under search_dir (defaults to the current project/working directory). This is project-scoped and lists files, not an arbitrary filesystem folder search. To find a directory by name under the home folder, use run_shell_command with is_read_only=true (prefer mdfind or find with -maxdepth under a known path like ~/codish), not file_glob_v2.",
        )
        .required_string_array(
            "patterns",
            "Glob patterns such as '**/*.rs' or 'src/**/*.ts'",
        )
        .optional_string(
            "search_dir",
            "Base directory to search from (defaults to the current working directory)",
        )
        .build(),
        ToolSchemaBuilder::new(
            "search_codebase",
            "Semantic search across the codebase. Use for finding concepts, functions, or implementations.",
        )
        .required_string("query", "Natural language search query")
        .optional_string_array("path_filters", "Optional path prefixes to restrict the search")
        .optional_string("codebase_path", "Optional codebase root path")
        .build(),
        ToolSchema {
            name: "edit_files".to_string(),
            description: "Propose reviewed file edits using Warp's CodeDiff UI (never writes directly to disk). REQUIRED for any file modifications. Call this instead of using shell to write files. Supports 'replace' (with search/replace), 'create' (with content), and 'delete'. Always provide precise 'edits' array.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Optional short title for the edit set"
                    },
                    "edits": {
                        "type": "array",
                        "description": "List of edits to perform. Each item must have 'type' and 'file'. For 'replace' also provide exact 'search' string to find and 'replace' string. For 'create' provide 'content'. Matches must be exact for replace.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["replace", "create", "delete"],
                                    "description": "The kind of edit"
                                },
                                "file": {
                                    "type": "string",
                                    "description": "Absolute or relative path to the target file"
                                },
                                "search": {
                                    "type": "string",
                                    "description": "The exact existing text to find and replace (for type=replace). Must match precisely."
                                },
                                "replace": {
                                    "type": "string",
                                    "description": "The new text to insert in place of search (for type=replace)"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "The full new file content (for type=create)"
                                }
                            },
                            "required": ["type", "file"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["edits"],
                "additionalProperties": false
            }),
        },
    ]
}

fn ask_user_question_schema() -> ToolSchema {
    ToolSchema {
        name: "ask_user_question".to_string(),
        description: "Pause and ask the user one or more multiple-choice clarification questions."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question_id": { "type": "string" },
                            "question": { "type": "string" },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "items": { "type": "string" }
                            },
                            "recommended_option_index": { "type": "integer", "minimum": -1 },
                            "is_multiselect": { "type": "boolean" },
                            "supports_other": { "type": "boolean" }
                        },
                        "required": ["question_id", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
pub fn is_supported_tool(name: &str) -> bool {
    LocalRuntimeToolRegistry::built_ins().contains_tool(name)
}

#[cfg(test)]
pub fn tool_call_to_ai_action(
    call: &ToolCall,
    task_id: &TaskId,
) -> Result<AIAgentAction, ToolExecutionError> {
    tool_call_to_ai_action_with_registry(call, task_id, &LocalRuntimeToolRegistry::built_ins())
}

pub fn tool_call_to_ai_action_with_registry(
    call: &ToolCall,
    task_id: &TaskId,
    registry: &LocalRuntimeToolRegistry,
) -> Result<AIAgentAction, ToolExecutionError> {
    let route = registry
        .route(&call.name)
        .ok_or_else(|| ToolExecutionError::NotFound {
            name: call.name.clone(),
        })?;

    let action = match &route.kind {
        LocalRuntimeToolRouteKind::BuiltIn => built_in_tool_call_to_ai_action(call)?,
        LocalRuntimeToolRouteKind::McpTool { server_id, name } => AIAgentActionType::CallMCPTool {
            server_id: *server_id,
            name: name.clone(),
            input: arguments_object(&call.arguments, &call.name)?
                .clone()
                .into(),
        },
        LocalRuntimeToolRouteKind::ReadMcpResource => {
            validate_allowed_arguments(&call.arguments, &["name", "uri"], &call.name)?;
            let uri = optional_string(&call.arguments, "uri")?;
            let name = optional_string(&call.arguments, "name")?;
            let Some(name_or_uri) = name.clone().or_else(|| uri.clone()) else {
                return Err(ToolExecutionError::InvalidInput {
                    reason: "Tool `read_mcp_resource` requires `uri` or `name`".to_string(),
                });
            };
            AIAgentActionType::ReadMCPResource {
                server_id: None,
                name: name_or_uri,
                uri,
            }
        }
        LocalRuntimeToolRouteKind::ReadSkill { skill_lookup } => {
            validate_allowed_arguments(&call.arguments, &["skill"], &call.name)?;
            let skill = required_string(&call.arguments, "skill", &call.name)?;
            AIAgentActionType::ReadSkill(crate::ai::agent::ReadSkillRequest {
                skill: skill_lookup
                    .get(&skill)
                    .cloned()
                    .unwrap_or_else(|| parse_skill_reference(skill)),
            })
        }
    };

    Ok(AIAgentAction {
        id: AIAgentActionId::from(call.id.clone()),
        task_id: task_id.clone(),
        action,
        requires_result: true,
    })
}

fn built_in_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    if call.name == "edit_files" {
        return edit_files_tool_call_to_ai_action(call);
    }
    if call.name == "ask_user_question" {
        return ask_user_question_tool_call_to_ai_action(call);
    }

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

    Ok(action)
}

fn ask_user_question_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["questions"], &call.name)?;
    let questions = call
        .arguments
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `ask_user_question` requires array argument `questions`".to_string(),
        })?;
    if questions.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `ask_user_question` requires at least one question".to_string(),
        });
    }

    let questions = questions
        .iter()
        .map(parse_ask_user_question)
        .collect::<Result<_, _>>()?;
    Ok(AIAgentActionType::AskUserQuestion { questions })
}

fn parse_ask_user_question(value: &Value) -> Result<AskUserQuestionItem, ToolExecutionError> {
    let arguments = Value::Object(
        value
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "`ask_user_question.questions` entries must be objects".to_string(),
            })?
            .clone(),
    );
    validate_allowed_arguments(
        &arguments,
        &[
            "question_id",
            "question",
            "options",
            "recommended_option_index",
            "is_multiselect",
            "supports_other",
        ],
        "ask_user_question question",
    )?;
    let labels = required_string_array(&arguments, &["options"], "ask_user_question question")?;
    if labels.len() < 2 {
        return Err(ToolExecutionError::InvalidInput {
            reason: "`ask_user_question` requires at least two options per question".to_string(),
        });
    }
    let recommended_index = optional_i64(&arguments, "recommended_option_index")?
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < labels.len());
    let options = labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| AskUserQuestionOption {
            label,
            recommended: recommended_index == Some(index),
        })
        .collect();

    Ok(AskUserQuestionItem {
        question_id: required_string(&arguments, "question_id", "ask_user_question question")?,
        question: required_string(&arguments, "question", "ask_user_question question")?,
        question_type: AskUserQuestionType::MultipleChoice {
            is_multiselect: optional_bool(&arguments, "is_multiselect")?.unwrap_or(false),
            options,
            supports_other: optional_bool(&arguments, "supports_other")?.unwrap_or(true),
        },
    })
}

fn edit_files_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["title", "edits"], &call.name)?;
    let edits_value = call
        .arguments
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` requires array argument `edits`".to_string(),
        })?;

    if edits_value.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` requires at least one edit".to_string(),
        });
    }

    let file_edits = edits_value
        .iter()
        .map(parse_file_edit)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AIAgentActionType::RequestFileEdits {
        file_edits,
        title: optional_string(&call.arguments, "title")?,
    })
}

fn parse_file_edit(value: &Value) -> Result<FileEdit, ToolExecutionError> {
    let object = value
        .as_object()
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "`edit_files.edits` entries must be objects".to_string(),
        })?;
    let arguments = Value::Object(object.clone());
    validate_allowed_arguments(
        &arguments,
        &["type", "file", "search", "replace", "content"],
        "edit_files edit",
    )?;

    let edit_type = required_string(&arguments, "type", "edit_files edit")?;
    let file = Some(required_string(&arguments, "file", "edit_files edit")?);

    match edit_type.as_str() {
        "replace" => Ok(FileEdit::Edit(ParsedDiff::StrReplaceEdit {
            file,
            search: Some(required_string(
                &arguments,
                "search",
                "edit_files replace edit",
            )?),
            replace: Some(required_string(
                &arguments,
                "replace",
                "edit_files replace edit",
            )?),
        })),
        "create" => Ok(FileEdit::Create {
            file,
            content: Some(required_string(
                &arguments,
                "content",
                "edit_files create edit",
            )?),
        }),
        "delete" => Ok(FileEdit::Delete { file }),
        _ => Err(ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` edit type must be `replace`, `create`, or `delete`"
                .to_string(),
        }),
    }
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
    let runtime_result = action_result_to_tool_result(result);
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
        server_message_data: encode_local_runtime_tool_result_data(
            result.id.to_string(),
            &runtime_result,
        ),
        citations: vec![],
        fetched_memories: vec![],
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
        AIAgentActionResultType::RequestFileEdits(result) => result.try_into().ok()?,
        AIAgentActionResultType::ReadMCPResource(result) => result.try_into().ok()?,
        AIAgentActionResultType::CallMCPTool(result) => result.try_into().ok()?,
        AIAgentActionResultType::ReadSkill(result) => result.try_into().ok()?,
        AIAgentActionResultType::AskUserQuestion(result) => result.into(),
        _ => return None,
    };

    match request_result {
        RequestResult::RunShellCommand(result) => Some(MessageResult::RunShellCommand(result)),
        RequestResult::ReadFiles(result) => Some(MessageResult::ReadFiles(result)),
        RequestResult::SearchCodebase(result) => Some(MessageResult::SearchCodebase(result)),
        RequestResult::Grep(result) => Some(MessageResult::Grep(result)),
        RequestResult::FileGlobV2(result) => Some(MessageResult::FileGlobV2(result)),
        RequestResult::ApplyFileDiffs(result) => Some(MessageResult::ApplyFileDiffs(result)),
        RequestResult::ReadMcpResource(result) => Some(MessageResult::ReadMcpResource(result)),
        RequestResult::CallMcpTool(result) => Some(MessageResult::CallMcpTool(result)),
        RequestResult::ReadSkill(result) => Some(MessageResult::ReadSkill(result)),
        RequestResult::AskUserQuestion(result) => Some(MessageResult::AskUserQuestion(result)),
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
        }) => {
            let status = if exit_code.was_successful() {
                "completed"
            } else {
                "failed"
            };
            // Put stdout first so weak models see the answer before metadata.
            serde_json::json!({
                "status": status,
                "exit_code": exit_code.value(),
                "stdout": output,
                "command": command,
                "instruction": if exit_code.was_successful() {
                    "Command finished successfully. Answer the user using stdout. Do not invent timeouts or claim failure."
                } else {
                    "Command failed. Report exit_code and stdout/stderr to the user."
                },
            })
            .to_string()
        }
        AIAgentActionResultType::RequestCommandOutput(
            RequestCommandOutputResult::LongRunningCommandSnapshot {
                command,
                grid_contents,
                block_id,
                ..
            },
        ) => serde_json::json!({
            "status": "long_running",
            "stdout": grid_contents,
            "command": command,
            "block_id": block_id.to_string(),
            "instruction": "Partial output only. If stdout already answers the user, report that answer immediately. Do not invent a timeout failure. Do not start another shell command until this one finishes.",
        })
        .to_string(),
        AIAgentActionResultType::RequestCommandOutput(
            RequestCommandOutputResult::CancelledBeforeExecution,
        ) => serde_json::json!({
            "status": "cancelled",
            "error": "Shell command was cancelled before execution. A previous command may still be running in the terminal, or the user dismissed approval. Wait for the active command to finish, then retry with a single scoped read-only command.",
        })
        .to_string(),
        AIAgentActionResultType::RequestCommandOutput(RequestCommandOutputResult::Denylisted {
            command,
        }) => serde_json::json!({
            "status": "denied",
            "command": command,
            "error": "Command is on the denylist and cannot be executed.",
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
        AIAgentActionResultType::RequestFileEdits(result) => match result {
            crate::ai::agent::RequestFileEditsResult::Success {
                updated_files,
                diff,
                ..
            } => {
                let updated_files = updated_files
                    .iter()
                    .map(|updated_file| {
                        serde_json::json!({
                            "was_edited_by_user": updated_file.was_edited_by_user,
                            "file": {
                                "path": updated_file.file_context.file_name,
                                "line_count": updated_file.file_context.line_count,
                            },
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "status": "accepted",
                    "updated_files": updated_files,
                    "diff": diff,
                })
                .to_string()
            }
            crate::ai::agent::RequestFileEditsResult::DiffApplicationFailed { error } => {
                serde_json::json!({
                    "status": "error",
                    "error": error,
                })
                .to_string()
            }
            crate::ai::agent::RequestFileEditsResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
        },
        AIAgentActionResultType::ReadMCPResource(result) => match result {
            crate::ai::agent::ReadMCPResourceResult::Success { resource_contents } => {
                serde_json::json!({
                    "resource_contents": resource_contents,
                })
                .to_string()
            }
            crate::ai::agent::ReadMCPResourceResult::Error(error) => serde_json::json!({
                "error": error,
            })
            .to_string(),
            crate::ai::agent::ReadMCPResourceResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
        },
        AIAgentActionResultType::CallMCPTool(result) => match result {
            crate::ai::agent::CallMCPToolResult::Success { result } => serde_json::json!({
                "result": result,
            })
            .to_string(),
            crate::ai::agent::CallMCPToolResult::Error(error) => serde_json::json!({
                "error": error,
            })
            .to_string(),
            crate::ai::agent::CallMCPToolResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
        },
        AIAgentActionResultType::ReadSkill(result) => match result {
            crate::ai::agent::ReadSkillResult::Success { content } => {
                file_contexts_to_json(std::slice::from_ref(content))
            }
            crate::ai::agent::ReadSkillResult::Error(error) => serde_json::json!({
                "error": error,
            })
            .to_string(),
            crate::ai::agent::ReadSkillResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
        },
        AIAgentActionResultType::AskUserQuestion(result) => {
            use crate::ai::agent::{AskUserQuestionAnswerItem, AskUserQuestionResult};

            match result {
                AskUserQuestionResult::Success { answers } => {
                    let answers = answers
                        .iter()
                        .map(|answer| match answer {
                            AskUserQuestionAnswerItem::Answered {
                                question_id,
                                selected_options,
                                other_text,
                            } => serde_json::json!({
                                "question_id": question_id,
                                "status": "answered",
                                "selected_options": selected_options,
                                "other_text": other_text,
                            }),
                            AskUserQuestionAnswerItem::Skipped { question_id } => {
                                serde_json::json!({
                                    "question_id": question_id,
                                    "status": "skipped",
                                })
                            }
                        })
                        .collect::<Vec<_>>();
                    serde_json::json!({
                        "status": "completed",
                        "answers": answers,
                    })
                    .to_string()
                }
                AskUserQuestionResult::SkippedByAutoApprove { question_ids } => serde_json::json!({
                    "status": "skipped",
                    "question_ids": question_ids,
                })
                .to_string(),
                AskUserQuestionResult::Error(error) => serde_json::json!({
                    "status": "error",
                    "error": error,
                })
                .to_string(),
                AskUserQuestionResult::Cancelled => serde_json::json!({
                    "status": "cancelled",
                })
                .to_string(),
            }
        }
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
        "run_shell_command" => {
            validate_allowed_arguments(
                &call.arguments,
                &["command", "is_read_only", "is_risky", "uses_pager"],
                &call.name,
            )?;
            let command = required_string(&call.arguments, "command", &call.name)?;
            let explicit_read_only = optional_bool(&call.arguments, "is_read_only")?;
            let is_read_only =
                explicit_read_only.unwrap_or_else(|| infer_shell_command_is_read_only(&command));
            Ok(Tool::RunShellCommand(
                api::message::tool_call::RunShellCommand {
                    command,
                    is_read_only,
                    is_risky: optional_bool(&call.arguments, "is_risky")?.unwrap_or(false),
                    uses_pager: optional_bool(&call.arguments, "uses_pager")?.unwrap_or(false),
                    // Local runtime has no long-running-command poll tools; always wait
                    // for completion (capped by ShellCommandExecutor::MAX_AGENT_DELAY_DURATION).
                    wait_until_complete_value: Some(
                        api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                            true,
                        ),
                    ),
                    ..Default::default()
                },
            ))
        }
        "read_files" => {
            validate_allowed_arguments(&call.arguments, &["paths", "files"], &call.name)?;
            Ok(Tool::ReadFiles(api::message::tool_call::ReadFiles {
                files: required_string_array(&call.arguments, &["paths", "files"], &call.name)?
                    .into_iter()
                    .map(|path| api::message::tool_call::read_files::File {
                        name: path,
                        line_ranges: vec![],
                    })
                    .collect(),
            }))
        }
        "grep" => {
            validate_allowed_arguments(
                &call.arguments,
                &["queries", "patterns", "pattern", "path"],
                &call.name,
            )?;
            Ok(Tool::Grep(api::message::tool_call::Grep {
                queries: if has_any_argument(&call.arguments, &["queries", "patterns"]) {
                    required_string_array(&call.arguments, &["queries", "patterns"], &call.name)?
                } else {
                    vec![required_string(&call.arguments, "pattern", &call.name)?]
                },
                path: optional_string(&call.arguments, "path")?.unwrap_or_else(|| ".".to_string()),
            }))
        }
        "file_glob_v2" => {
            validate_allowed_arguments(
                &call.arguments,
                &["patterns", "pattern", "search_dir", "path"],
                &call.name,
            )?;
            let search_dir = match optional_string(&call.arguments, "search_dir")? {
                Some(search_dir) => search_dir,
                None => optional_string(&call.arguments, "path")?.unwrap_or_default(),
            };
            Ok(Tool::FileGlobV2(api::message::tool_call::FileGlobV2 {
                patterns: if has_any_argument(&call.arguments, &["patterns"]) {
                    required_string_array(&call.arguments, &["patterns"], &call.name)?
                } else {
                    vec![required_string(&call.arguments, "pattern", &call.name)?]
                },
                search_dir,
                min_depth: 0,
                max_depth: 0,
                max_matches: 0,
            }))
        }
        "search_codebase" => {
            validate_allowed_arguments(
                &call.arguments,
                &["query", "path_filters", "codebase_path"],
                &call.name,
            )?;
            Ok(Tool::SearchCodebase(
                api::message::tool_call::SearchCodebase {
                    query: required_string(&call.arguments, "query", &call.name)?,
                    path_filters: optional_string_array(&call.arguments, "path_filters")?
                        .unwrap_or_default(),
                    codebase_path: optional_string(&call.arguments, "codebase_path")?
                        .unwrap_or_default(),
                },
            ))
        }
        "edit_files" => Ok(Tool::ApplyFileDiffs(edit_files_tool_call_to_proto(call)?)),
        "ask_user_question" => Ok(Tool::AskUserQuestion(ask_user_question_tool_call_to_proto(
            call,
        )?)),
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

fn ask_user_question_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::AskUserQuestion, ToolExecutionError> {
    use api::ask_user_question::question::QuestionType;

    let AIAgentActionType::AskUserQuestion { questions } =
        ask_user_question_tool_call_to_ai_action(call)?
    else {
        unreachable!("ask-user conversion always returns AskUserQuestion");
    };
    Ok(api::AskUserQuestion {
        questions: questions
            .into_iter()
            .map(|question| {
                let AskUserQuestionType::MultipleChoice {
                    is_multiselect,
                    options,
                    supports_other,
                } = question.question_type;
                let recommended_option_index = options
                    .iter()
                    .position(|option| option.recommended)
                    .and_then(|index| i32::try_from(index).ok())
                    .unwrap_or(-1);
                api::ask_user_question::Question {
                    question_id: question.question_id,
                    question: question.question,
                    question_type: Some(QuestionType::MultipleChoice(
                        api::ask_user_question::MultipleChoice {
                            options: options
                                .into_iter()
                                .map(|option| api::ask_user_question::Option {
                                    label: option.label,
                                })
                                .collect(),
                            recommended_option_index,
                            is_multiselect,
                            supports_other,
                        },
                    )),
                }
            })
            .collect(),
    })
}

pub fn tool_call_to_proto_tool_with_registry(
    call: &ToolCall,
    registry: &LocalRuntimeToolRegistry,
) -> Result<api::message::tool_call::Tool, ToolExecutionError> {
    use api::message::tool_call::Tool;

    let route = registry
        .route(&call.name)
        .ok_or_else(|| ToolExecutionError::NotFound {
            name: call.name.clone(),
        })?;
    match &route.kind {
        LocalRuntimeToolRouteKind::BuiltIn => tool_call_to_proto_tool(call),
        LocalRuntimeToolRouteKind::McpTool { server_id, name } => {
            let arguments = arguments_object(&call.arguments, &call.name)?;
            Ok(Tool::CallMcpTool(api::message::tool_call::CallMcpTool {
                server_id: server_id.map(|id| id.to_string()).unwrap_or_default(),
                name: name.clone(),
                args: Some(json_object_to_prost_struct(arguments)?),
            }))
        }
        LocalRuntimeToolRouteKind::ReadMcpResource => {
            validate_allowed_arguments(&call.arguments, &["name", "uri"], &call.name)?;
            let uri = optional_string(&call.arguments, "uri")?
                .or(optional_string(&call.arguments, "name")?)
                .ok_or_else(|| ToolExecutionError::InvalidInput {
                    reason: "Tool `read_mcp_resource` requires `uri` or `name`".to_string(),
                })?;
            Ok(Tool::ReadMcpResource(
                api::message::tool_call::ReadMcpResource {
                    server_id: String::new(),
                    uri,
                },
            ))
        }
        LocalRuntimeToolRouteKind::ReadSkill { skill_lookup } => {
            use api::message::tool_call::read_skill::SkillReference as ProtoSkillReference;

            validate_allowed_arguments(&call.arguments, &["skill"], &call.name)?;
            let skill = required_string(&call.arguments, "skill", &call.name)?;
            let reference = skill_lookup
                .get(&skill)
                .cloned()
                .unwrap_or_else(|| parse_skill_reference(skill.clone()));
            let skill_reference = match reference {
                SkillReference::Path(path) => ProtoSkillReference::SkillPath(path.display_path()),
                SkillReference::BundledSkillId(id) => ProtoSkillReference::BundledSkillId(id),
            };
            Ok(Tool::ReadSkill(api::message::tool_call::ReadSkill {
                name: skill,
                skill_reference: Some(skill_reference),
            }))
        }
    }
}

pub fn proto_tool_call_to_runtime_with_registry(
    tool_call: &api::message::ToolCall,
    registry: &LocalRuntimeToolRegistry,
) -> Option<ToolCall> {
    use api::message::tool_call::read_skill::SkillReference as ProtoSkillReference;
    use api::message::tool_call::Tool;

    let tool = tool_call.tool.as_ref()?;
    let (name, arguments) = match tool {
        Tool::RunShellCommand(tool) => (
            "run_shell_command".to_string(),
            serde_json::json!({
                "command": tool.command,
                "is_read_only": tool.is_read_only,
                "is_risky": tool.is_risky,
                "uses_pager": tool.uses_pager,
            }),
        ),
        Tool::ReadFiles(tool) => (
            "read_files".to_string(),
            serde_json::json!({
                "paths": tool.files.iter().map(|file| file.name.clone()).collect::<Vec<_>>(),
            }),
        ),
        Tool::Grep(tool) => (
            "grep".to_string(),
            serde_json::json!({
                "queries": tool.queries,
                "path": tool.path,
            }),
        ),
        Tool::FileGlobV2(tool) => (
            "file_glob_v2".to_string(),
            serde_json::json!({
                "patterns": tool.patterns,
                "search_dir": tool.search_dir,
            }),
        ),
        Tool::SearchCodebase(tool) => (
            "search_codebase".to_string(),
            serde_json::json!({
                "query": tool.query,
                "path_filters": tool.path_filters,
                "codebase_path": tool.codebase_path,
            }),
        ),
        Tool::ApplyFileDiffs(tool) => {
            let mut edits = Vec::new();
            edits.extend(tool.diffs.iter().map(|diff| {
                serde_json::json!({
                    "type": "replace",
                    "file": diff.file_path,
                    "search": diff.search,
                    "replace": diff.replace,
                })
            }));
            edits.extend(tool.new_files.iter().map(|file| {
                serde_json::json!({
                    "type": "create",
                    "file": file.file_path,
                    "content": file.content,
                })
            }));
            edits.extend(tool.deleted_files.iter().map(|file| {
                serde_json::json!({
                    "type": "delete",
                    "file": file.file_path,
                })
            }));
            (
                "edit_files".to_string(),
                serde_json::json!({
                    "title": tool.summary,
                    "edits": edits,
                }),
            )
        }
        Tool::CallMcpTool(tool) => {
            let name = registry
                .routes
                .iter()
                .find_map(|(function_name, route)| match &route.kind {
                    LocalRuntimeToolRouteKind::McpTool { server_id, name }
                        if name == &tool.name
                            && server_id.map(|id| id.to_string()).unwrap_or_default()
                                == tool.server_id =>
                    {
                        Some(function_name.clone())
                    }
                    LocalRuntimeToolRouteKind::BuiltIn
                    | LocalRuntimeToolRouteKind::McpTool { .. }
                    | LocalRuntimeToolRouteKind::ReadMcpResource
                    | LocalRuntimeToolRouteKind::ReadSkill { .. } => None,
                })
                .unwrap_or_else(|| {
                    format!(
                        "mcp__restored__{}",
                        sanitize_function_name(tool.name.as_str())
                    )
                });
            (
                name,
                tool.args
                    .as_ref()
                    .map(prost_struct_to_json)
                    .unwrap_or_else(|| serde_json::json!({})),
            )
        }
        Tool::ReadMcpResource(tool) => (
            "read_mcp_resource".to_string(),
            serde_json::json!({ "uri": tool.uri }),
        ),
        Tool::ReadSkill(tool) => {
            let skill = match tool.skill_reference.as_ref()? {
                ProtoSkillReference::SkillPath(path) => path.clone(),
                ProtoSkillReference::BundledSkillId(id) => format!("@warp-skill:{id}"),
            };
            (
                "read_skill".to_string(),
                serde_json::json!({ "skill": skill }),
            )
        }
        Tool::AskUserQuestion(tool) => {
            use api::ask_user_question::question::QuestionType;

            let questions = tool
                .questions
                .iter()
                .filter_map(|question| {
                    let QuestionType::MultipleChoice(multiple_choice) =
                        question.question_type.as_ref()?;
                    Some(serde_json::json!({
                        "question_id": question.question_id,
                        "question": question.question,
                        "options": multiple_choice
                            .options
                            .iter()
                            .map(|option| option.label.clone())
                            .collect::<Vec<_>>(),
                        "recommended_option_index": multiple_choice.recommended_option_index,
                        "is_multiselect": multiple_choice.is_multiselect,
                        "supports_other": multiple_choice.supports_other,
                    }))
                })
                .collect::<Vec<_>>();
            (
                "ask_user_question".to_string(),
                serde_json::json!({ "questions": questions }),
            )
        }
        _ => return None,
    };

    Some(ToolCall {
        id: tool_call.tool_call_id.clone(),
        name,
        arguments,
    })
}

fn json_object_to_prost_struct(
    object: &serde_json::Map<String, Value>,
) -> Result<prost_types::Struct, ToolExecutionError> {
    object
        .iter()
        .map(|(key, value)| Ok((key.clone(), json_to_prost_value(value)?)))
        .collect::<Result<_, _>>()
        .map(|fields| prost_types::Struct { fields })
}

fn json_to_prost_value(value: &Value) -> Result<prost_types::Value, ToolExecutionError> {
    use prost_types::value::Kind;

    let kind =
        match value {
            Value::Null => Kind::NullValue(0),
            Value::Bool(value) => Kind::BoolValue(*value),
            Value::Number(value) => Kind::NumberValue(value.as_f64().ok_or_else(|| {
                ToolExecutionError::InvalidInput {
                    reason: "MCP numeric argument cannot be represented as f64".to_string(),
                }
            })?),
            Value::String(value) => Kind::StringValue(value.clone()),
            Value::Array(values) => Kind::ListValue(prost_types::ListValue {
                values: values
                    .iter()
                    .map(json_to_prost_value)
                    .collect::<Result<_, _>>()?,
            }),
            Value::Object(object) => Kind::StructValue(json_object_to_prost_struct(object)?),
        };
    Ok(prost_types::Value { kind: Some(kind) })
}

fn prost_struct_to_json(value: &prost_types::Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), prost_value_to_json(value)))
            .collect(),
    )
}

fn prost_value_to_json(value: &prost_types::Value) -> Value {
    use prost_types::value::Kind;

    match value.kind.as_ref() {
        Some(Kind::NullValue(_)) | None => Value::Null,
        Some(Kind::BoolValue(value)) => Value::Bool(*value),
        Some(Kind::NumberValue(value)) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(value)) => Value::String(value.clone()),
        Some(Kind::ListValue(value)) => {
            Value::Array(value.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(value)) => prost_struct_to_json(value),
    }
}

fn edit_files_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::message::tool_call::ApplyFileDiffs, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["title", "edits"], &call.name)?;
    let edits_value = call
        .arguments
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` requires array argument `edits`".to_string(),
        })?;

    if edits_value.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` requires at least one edit".to_string(),
        });
    }

    let mut diffs = Vec::new();
    let mut new_files = Vec::new();
    let mut deleted_files = Vec::new();

    for edit in edits_value {
        let object = edit
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "`edit_files.edits` entries must be objects".to_string(),
            })?;
        let arguments = Value::Object(object.clone());
        validate_allowed_arguments(
            &arguments,
            &["type", "file", "search", "replace", "content"],
            "edit_files edit",
        )?;

        let edit_type = required_string(&arguments, "type", "edit_files edit")?;
        let file_path = required_string(&arguments, "file", "edit_files edit")?;
        match edit_type.as_str() {
            "replace" => {
                diffs.push(api::message::tool_call::apply_file_diffs::FileDiff {
                    file_path,
                    search: required_string(&arguments, "search", "edit_files replace edit")?,
                    replace: required_string(&arguments, "replace", "edit_files replace edit")?,
                });
            }
            "create" => {
                new_files.push(api::message::tool_call::apply_file_diffs::NewFile {
                    file_path,
                    content: required_string(&arguments, "content", "edit_files create edit")?,
                });
            }
            "delete" => {
                deleted_files
                    .push(api::message::tool_call::apply_file_diffs::DeleteFile { file_path });
            }
            _ => {
                return Err(ToolExecutionError::InvalidInput {
                    reason: "Tool `edit_files` edit type must be `replace`, `create`, or `delete`"
                        .to_string(),
                });
            }
        }
    }

    Ok(api::message::tool_call::ApplyFileDiffs {
        summary: optional_string(&call.arguments, "title")?.unwrap_or_default(),
        diffs,
        new_files,
        deleted_files,
        v4a_updates: vec![],
    })
}

fn has_any_argument(arguments: &Value, names: &[&str]) -> bool {
    names.iter().any(|name| arguments.get(name).is_some())
}

/// Whether a `run_shell_command` call should be treated as read-only for scheduling
/// and Warp auto-execute. Prefers an explicit `is_read_only` argument, otherwise
/// applies a conservative command heuristic.
fn shell_command_is_read_only(arguments: &Value) -> bool {
    if let Some(Value::Bool(is_read_only)) = arguments.get("is_read_only") {
        return *is_read_only;
    }
    arguments
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(infer_shell_command_is_read_only)
}

fn infer_shell_command_is_read_only(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lowered = trimmed.to_ascii_lowercase();
    const UNSAFE_MARKERS: &[&str] = &[
        " rm ",
        "rm ",
        "sudo ",
        " chmod ",
        " chown ",
        " mv ",
        " cp ",
        " dd ",
        " mkfs",
        " shutdown",
        " reboot",
        " kill ",
        " pkill ",
        " curl ",
        " wget ",
        "| sh",
        "|bash",
        "tee ",
        "sed -i",
        "truncate ",
        "git commit",
        "git push",
        "git reset",
        "git checkout",
        "git switch",
        "npm install",
        "pip install",
        "cargo install",
    ];
    let padded = format!(" {lowered} ");
    if UNSAFE_MARKERS.iter().any(|marker| padded.contains(marker)) {
        return false;
    }
    // Allow discarding stdout/stderr to /dev/null; treat other redirects as writes.
    let without_null_redirects = lowered
        .replace("2>/dev/null", "")
        .replace("2> /dev/null", "")
        .replace(">/dev/null", "")
        .replace("> /dev/null", "");
    if without_null_redirects.contains('>') {
        return false;
    }

    // Strip common wrappers like `cd ... && find ...` and inspect each segment.
    let segments = lowered
        .split(['&', ';', '|', '\n'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty());
    for segment in segments {
        let first = segment
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches("./");
        const READ_ONLY_COMMANDS: &[&str] = &[
            "ls", "find", "pwd", "cat", "head", "tail", "wc", "which", "type", "echo", "printf",
            "rg", "grep", "egrep", "fgrep", "fd", "tree", "stat", "file", "du", "df", "date",
            "whoami", "id", "env", "printenv", "uname", "hostname", "basename", "dirname",
            "realpath", "readlink", "git", "cd", "true", "false", "test", "[", "mdfind", "locate",
            // Interpreter one-liners are handled below (require -c/-e).
            "python", "python3", "node", "nodejs", "ruby", "perl",
        ];
        if !READ_ONLY_COMMANDS.contains(&first) {
            return false;
        }
        if matches!(
            first,
            "python" | "python3" | "node" | "nodejs" | "ruby" | "perl"
        ) {
            // Only treat pure -c/-e eval one-liners as read-only compute.
            let has_eval_flag = segment.split_whitespace().any(|t| t == "-c" || t == "-e");
            if !has_eval_flag {
                return false;
            }
            continue;
        }
        if first == "git" {
            let sub = segment.split_whitespace().nth(1).unwrap_or_default();
            const READ_ONLY_GIT: &[&str] = &[
                "status",
                "log",
                "diff",
                "show",
                "branch",
                "tag",
                "remote",
                "ls-files",
                "rev-parse",
                "describe",
                "blame",
                "grep",
                "shortlog",
            ];
            if !READ_ONLY_GIT.contains(&sub) {
                return false;
            }
        }
    }
    true
}

fn validate_allowed_arguments(
    arguments: &Value,
    allowed: &[&str],
    tool_name: &str,
) -> Result<(), ToolExecutionError> {
    let object = arguments_object(arguments, tool_name)?;
    let mut unsupported = object
        .keys()
        .filter(|name| !allowed.contains(&name.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if unsupported.is_empty() {
        return Ok(());
    }

    unsupported.sort();
    Err(ToolExecutionError::InvalidInput {
        reason: format!(
            "Tool `{tool_name}` does not support argument(s): `{}`",
            unsupported.join("`, `")
        ),
    })
}

fn arguments_object<'a>(
    arguments: &'a Value,
    tool_name: &str,
) -> Result<&'a serde_json::Map<String, Value>, ToolExecutionError> {
    arguments
        .as_object()
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Tool `{tool_name}` requires object arguments"),
        })
}

fn required_string(
    arguments: &Value,
    name: &str,
    tool_name: &str,
) -> Result<String, ToolExecutionError> {
    let value =
        optional_string(arguments, name)?.ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Tool `{tool_name}` requires non-empty string argument `{name}`"),
        })?;

    if value.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Tool `{tool_name}` requires non-empty string argument `{name}`"),
        });
    }

    Ok(value)
}

fn optional_string(arguments: &Value, name: &str) -> Result<Option<String>, ToolExecutionError> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };

    let Some(value) = value.as_str() else {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a string"),
        });
    };

    if value.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a non-empty string"),
        });
    }

    Ok(Some(value.to_string()))
}

fn optional_bool(arguments: &Value, name: &str) -> Result<Option<bool>, ToolExecutionError> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };

    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a boolean"),
        })
}

fn optional_i64(arguments: &Value, name: &str) -> Result<Option<i64>, ToolExecutionError> {
    match arguments.get(name) {
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: format!("Argument `{name}` must be an integer"),
            }),
        None => Ok(None),
    }
}

fn required_string_array(
    arguments: &Value,
    names: &[&str],
    tool_name: &str,
) -> Result<Vec<String>, ToolExecutionError> {
    for name in names {
        let Some(values) = optional_string_array(arguments, name)? else {
            continue;
        };

        if values.is_empty() {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!(
                    "Tool `{tool_name}` requires non-empty string array argument `{name}`"
                ),
            });
        }

        return Ok(values);
    }

    Err(ToolExecutionError::InvalidInput {
        reason: format!(
            "Tool `{tool_name}` requires non-empty string array argument `{}`",
            names.join("` or `")
        ),
    })
}

fn optional_string_array(
    arguments: &Value,
    name: &str,
) -> Result<Option<Vec<String>>, ToolExecutionError> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };

    if let Some(values) = value.as_array() {
        return validate_string_values(
            name,
            values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        ToolExecutionError::InvalidInput {
                            reason: format!("Argument `{name}` must contain only strings"),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map(Some);
    }

    let Some(value) = value.as_str() else {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a string array"),
        });
    };

    if let Ok(values) = serde_json::from_str::<Vec<Value>>(value) {
        return validate_string_values(
            name,
            values
                .into_iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        ToolExecutionError::InvalidInput {
                            reason: format!("Argument `{name}` must contain only strings"),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map(Some);
    }

    validate_string_values(name, vec![value.to_string()]).map(Some)
}

fn validate_string_values(
    name: &str,
    values: Vec<String>,
) -> Result<Vec<String>, ToolExecutionError> {
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must contain only non-empty strings"),
        });
    }

    Ok(values)
}

fn skill_references_for_request(params: &RequestParams) -> HashMap<String, SkillReference> {
    let mut skills_by_key = HashMap::new();
    for input in &params.input {
        let Some(contexts) = input.context() else {
            continue;
        };
        for context in contexts {
            let crate::ai::agent::AIAgentContext::Skills { skills } = context else {
                continue;
            };
            for skill in skills {
                skills_by_key.insert(skill.name.clone(), skill.reference.clone());
                skills_by_key.insert(skill.reference.to_string(), skill.reference.clone());
            }
        }
    }
    skills_by_key
}

fn parse_skill_reference(value: String) -> SkillReference {
    const BUNDLED_PREFIX: &str = "@warp-skill:";
    if let Some(id) = value.strip_prefix(BUNDLED_PREFIX) {
        SkillReference::BundledSkillId(id.to_string())
    } else {
        SkillReference::Path(LocalOrRemotePath::Local(PathBuf::from(value)))
    }
}

fn sanitize_function_name(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();

    while sanitized.contains("__") {
        sanitized = sanitized.replace("__", "_");
    }

    sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        "tool".to_string()
    } else if sanitized
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
    {
        sanitized
    } else {
        format!("tool_{sanitized}")
    }
}

fn unique_tool_name(
    routes: &HashMap<String, LocalRuntimeToolRoute>,
    preferred_name: &str,
) -> String {
    if !routes.contains_key(preferred_name) {
        return preferred_name.to_string();
    }

    for index in 2.. {
        let candidate = format!("{preferred_name}_{index}");
        if !routes.contains_key(&candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded sequence must find a unique tool name")
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
    fn infer_shell_command_is_read_only_for_find_and_ls() {
        assert!(infer_shell_command_is_read_only(
            r#"find ~ -type d -name "learn-harness-engineering" 2>/dev/null"#
        ));
        assert!(infer_shell_command_is_read_only(
            r#"mdfind 'kMDItemFSName == "learn-harness-engineering"c' | head -20"#
        ));
        assert!(infer_shell_command_is_read_only(
            "cd ~ && ls -la | grep -i learn"
        ));
        assert!(infer_shell_command_is_read_only(
            r#"python3 -c "print(sum(range(1, 101)))""#
        ));
        assert!(infer_shell_command_is_read_only(
            r#"python -c "print(100*101//2)""#
        ));
        assert!(!infer_shell_command_is_read_only("python3 script.py"));
        assert!(!infer_shell_command_is_read_only("rm -rf /tmp/foo"));
        assert!(!infer_shell_command_is_read_only("echo hi > out.txt"));
    }

    #[test]
    fn shell_action_results_serialize_long_running_and_cancelled_for_model() {
        use crate::ai::agent::RequestCommandOutputResult;
        use crate::terminal::model::block::BlockId;

        let long_running = AIAgentActionResultType::RequestCommandOutput(
            RequestCommandOutputResult::LongRunningCommandSnapshot {
                block_id: BlockId::from("blk_1".to_string()),
                command: "find ~ -type d -name foo".to_string(),
                grid_contents: "/Users/me/foo\n".to_string(),
                cursor: String::new(),
                is_alt_screen_active: false,
            },
        );
        let content = action_result_to_content(&long_running);
        assert!(content.contains("\"status\":\"long_running\""));
        assert!(content.contains("/Users/me/foo"));
        assert!(content.contains("Partial output") || content.contains("stdout"));

        let cancelled = AIAgentActionResultType::RequestCommandOutput(
            RequestCommandOutputResult::CancelledBeforeExecution,
        );
        let content = action_result_to_content(&cancelled);
        assert!(content.contains("\"status\":\"cancelled\""));
        assert!(content.contains("previous command"));
    }

    #[test]
    fn completed_shell_result_puts_stdout_and_forbids_invented_timeout() {
        use crate::ai::agent::RequestCommandOutputResult;
        use crate::terminal::model::block::BlockId;
        use warp_core::command::ExitCode;

        let completed = AIAgentActionResultType::RequestCommandOutput(
            RequestCommandOutputResult::Completed {
                block_id: BlockId::from("blk_1".to_string()),
                command: r#"python3 -c "print(sum(range(1, 101)))""#.to_string(),
                output: "5050\n".to_string(),
                exit_code: ExitCode::from(0),
                start_ts: None,
                completed_ts: None,
            },
        );
        let content = action_result_to_content(&completed);
        assert!(content.contains("\"status\":\"completed\""));
        assert!(content.contains("5050"));
        assert!(content.contains("\"stdout\""));
        assert!(content.contains("Do not invent timeouts"));
    }

    #[test]
    fn shell_command_is_read_only_respects_explicit_flag() {
        assert!(!shell_command_is_read_only(&serde_json::json!({
            "command": "find ~ -type d -name foo",
            "is_read_only": false,
        })));
        assert!(shell_command_is_read_only(&serde_json::json!({
            "command": "find ~ -type d -name foo",
        })));
    }

    fn assert_invalid_input(call: ToolCall, expected_reason: &str) {
        let task_id = TaskId::new("task_1".to_string());
        let err = tool_call_to_ai_action(&call, &task_id).unwrap_err();

        let ToolExecutionError::InvalidInput { reason } = err else {
            panic!("expected invalid input");
        };
        assert!(
            reason.contains(expected_reason),
            "expected `{reason}` to contain `{expected_reason}`"
        );
    }

    #[test]
    fn transcript_envelope_round_trips_exact_tool_data() {
        let call = ToolCall {
            id: "mcp_call_1".to_string(),
            name: "mcp__github__search".to_string(),
            arguments: serde_json::json!({
                "query": "local runtime",
                "nested": { "limit": 3 },
            }),
        };
        let encoded_call = encode_local_runtime_tool_call_data(&call);
        let decoded_call = decode_local_runtime_tool_call_data(&encoded_call).unwrap();
        assert_eq!(decoded_call.id, call.id);
        assert_eq!(decoded_call.name, call.name);
        assert_eq!(decoded_call.arguments, call.arguments);

        let result = ToolCallResult::error(r#"{"error":"denied"}"#);
        let encoded_result = encode_local_runtime_tool_result_data(call.id.clone(), &result);
        let (decoded_call_id, decoded_result) =
            decode_local_runtime_tool_result_data(&encoded_result).unwrap();
        assert_eq!(decoded_call_id, call.id);
        assert_eq!(decoded_result.content, result.content);
        assert!(decoded_result.is_error);
    }

    #[test]
    fn malformed_or_wrong_kind_transcript_envelope_is_ignored() {
        assert!(decode_local_runtime_tool_call_data("{not json").is_none());
        let encoded_result =
            encode_local_runtime_tool_result_data("call_1", &ToolCallResult::success("ok"));
        assert!(decode_local_runtime_tool_call_data(&encoded_result).is_none());
    }

    #[test]
    fn registry_proto_round_trip_preserves_generated_mcp_name_and_arguments() {
        let server_id = Uuid::new_v4();
        let function_name = "mcp__github__search_issues";
        let mut registry = LocalRuntimeToolRegistry::built_ins();
        registry.add_tool(
            ToolSchema {
                name: function_name.to_string(),
                description: "Search issues".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            },
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::McpTool {
                server_id: Some(server_id),
                name: "search_issues".to_string(),
            },
        );
        let call = ToolCall {
            id: "call_1".to_string(),
            name: function_name.to_string(),
            arguments: serde_json::json!({
                "query": "is:open",
                "labels": ["bug", "agent"],
            }),
        };

        let proto_tool = tool_call_to_proto_tool_with_registry(&call, &registry).unwrap();
        let proto_call = api::message::ToolCall {
            tool_call_id: call.id.clone(),
            tool: Some(proto_tool),
        };
        let restored = proto_tool_call_to_runtime_with_registry(&proto_call, &registry).unwrap();

        assert_eq!(restored.id, call.id);
        assert_eq!(restored.name, call.name);
        assert_eq!(restored.arguments, call.arguments);
    }

    #[test]
    fn registry_proto_round_trip_preserves_skill_reference() {
        let skill = "@warp-skill:review".to_string();
        let mut registry = LocalRuntimeToolRegistry::built_ins();
        registry.add_read_skill(HashMap::from([(
            skill.clone(),
            SkillReference::BundledSkillId("review".to_string()),
        )]));
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read_skill".to_string(),
            arguments: serde_json::json!({ "skill": skill }),
        };

        let proto_tool = tool_call_to_proto_tool_with_registry(&call, &registry).unwrap();
        let proto_call = api::message::ToolCall {
            tool_call_id: call.id.clone(),
            tool: Some(proto_tool),
        };
        let restored = proto_tool_call_to_runtime_with_registry(&proto_call, &registry).unwrap();

        assert_eq!(restored.name, "read_skill");
        assert_eq!(restored.arguments["skill"], "@warp-skill:review");
    }

    #[test]
    fn ask_user_question_maps_to_existing_warp_action_and_proto() {
        let mut registry = LocalRuntimeToolRegistry::built_ins();
        registry.add_ask_user_question();
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "ask_user_question".to_string(),
            arguments: serde_json::json!({
                "questions": [{
                    "question_id": "scope",
                    "question": "Which scope?",
                    "options": ["Focused", "Broad"],
                    "recommended_option_index": 0,
                    "is_multiselect": false,
                    "supports_other": true,
                }],
            }),
        };

        let action = tool_call_to_ai_action_with_registry(
            &call,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap();
        let AIAgentActionType::AskUserQuestion { questions } = action.action else {
            panic!("expected ask-user action");
        };
        assert_eq!(questions.len(), 1);
        assert!(questions[0].multiple_choice_options().unwrap()[0].recommended);

        let proto = tool_call_to_proto_tool_with_registry(&call, &registry).unwrap();
        let restored = proto_tool_call_to_runtime_with_registry(
            &api::message::ToolCall {
                tool_call_id: call.id.clone(),
                tool: Some(proto),
            },
            &registry,
        )
        .unwrap();
        assert_eq!(restored.name, call.name);
        assert_eq!(restored.arguments, call.arguments);
    }

    #[test]
    fn ask_user_question_result_preserves_answer_skip_and_cancel_states() {
        use crate::ai::agent::{AskUserQuestionAnswerItem, AskUserQuestionResult};

        let answered = AIAgentActionResultType::AskUserQuestion(AskUserQuestionResult::Success {
            answers: vec![
                AskUserQuestionAnswerItem::Answered {
                    question_id: "one".to_string(),
                    selected_options: vec!["A".to_string()],
                    other_text: String::new(),
                },
                AskUserQuestionAnswerItem::Skipped {
                    question_id: "two".to_string(),
                },
            ],
        });
        let content = action_result_to_content(&answered);
        assert!(content.contains("\"status\":\"answered\""));
        assert!(content.contains("\"status\":\"skipped\""));

        let cancelled = AIAgentActionResultType::AskUserQuestion(AskUserQuestionResult::Cancelled);
        let content = action_result_to_content(&cancelled);
        assert_eq!(content, r#"{"status":"cancelled"}"#);
    }

    #[test]
    fn request_permission_modes_filter_plan_tools_without_weakening_accept_edits() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut plan_params = RequestParams::new_for_test();
        plan_params.ask_user_question_enabled = true;
        plan_params.input = vec![AIAgentInput::UserQuery {
            query: "Plan this".to_string(),
            context: Arc::from([]),
            static_query_type: None,
            referenced_attachments: HashMap::new(),
            user_query_mode: crate::ai::agent::UserQueryMode::Plan,
            running_command: None,
            intended_agent: None,
        }];
        let plan_registry = LocalRuntimeToolRegistry::from_request(&plan_params);
        assert_eq!(
            plan_registry.permission_mode(),
            LocalRuntimePermissionMode::Plan
        );
        assert!(plan_registry.contains_tool("read_files"));
        assert!(plan_registry.contains_tool("ask_user_question"));
        assert!(!plan_registry.contains_tool("run_shell_command"));
        assert!(!plan_registry.contains_tool("edit_files"));

        let mut accept_params = RequestParams::new_for_test();
        accept_params.autonomy_level = api::AutonomyLevel::Unsupervised;
        let accept_registry = LocalRuntimeToolRegistry::from_request(&accept_params);
        assert_eq!(
            accept_registry.permission_mode(),
            LocalRuntimePermissionMode::AcceptEdits
        );
        assert!(accept_registry.contains_tool("edit_files"));
        assert!(accept_registry.contains_tool("run_shell_command"));
    }

    #[test]
    fn extended_action_results_have_typed_persisted_results() {
        use api::message::tool_call_result::Result as MessageResult;

        use crate::ai::agent::{
            CallMCPToolResult, ReadMCPResourceResult, ReadSkillResult, RequestFileEditsResult,
        };

        assert!(matches!(
            action_result_to_proto_tool_call_result_type(
                &AIAgentActionResultType::RequestFileEdits(
                    RequestFileEditsResult::DiffApplicationFailed {
                        error: "failed".to_string(),
                    },
                ),
            ),
            Some(MessageResult::ApplyFileDiffs(_))
        ));
        assert!(matches!(
            action_result_to_proto_tool_call_result_type(&AIAgentActionResultType::CallMCPTool(
                CallMCPToolResult::Error("failed".to_string(),)
            ),),
            Some(MessageResult::CallMcpTool(_))
        ));
        assert!(matches!(
            action_result_to_proto_tool_call_result_type(
                &AIAgentActionResultType::ReadMCPResource(ReadMCPResourceResult::Error(
                    "failed".to_string(),
                )),
            ),
            Some(MessageResult::ReadMcpResource(_))
        ));
        assert!(matches!(
            action_result_to_proto_tool_call_result_type(&AIAgentActionResultType::ReadSkill(
                ReadSkillResult::Error("failed".to_string(),)
            ),),
            Some(MessageResult::ReadSkill(_))
        ));
    }

    #[test]
    fn schemas_advertise_only_supported_v1_tools() {
        let schemas = build_tool_schemas();
        let names = schemas
            .iter()
            .map(|schema| schema.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "run_shell_command",
                "read_files",
                "grep",
                "file_glob_v2",
                "search_codebase",
                "edit_files"
            ]
        );
        assert!(!names.contains(&"create_file"));
        assert!(schemas
            .iter()
            .all(|schema| schema.parameters["additionalProperties"] == false));
    }

    #[test]
    fn schemas_keep_arguments_conservative() {
        let schemas = build_tool_schemas()
            .into_iter()
            .map(|schema| (schema.name.clone(), schema))
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            schemas["run_shell_command"].parameters["required"],
            serde_json::json!(["command"])
        );
        assert_eq!(
            schemas["read_files"].parameters["required"],
            serde_json::json!(["paths"])
        );
        assert_eq!(
            schemas["grep"].parameters["required"],
            serde_json::json!(["queries"])
        );
        assert_eq!(
            schemas["file_glob_v2"].parameters["required"],
            serde_json::json!(["patterns"])
        );
        assert_eq!(
            schemas["search_codebase"].parameters["required"],
            serde_json::json!(["query"])
        );
        assert_eq!(
            schemas["edit_files"].parameters["required"],
            serde_json::json!(["edits"])
        );
    }

    #[test]
    fn tool_calls_map_legacy_aliases_to_warp_actions() {
        let task_id = TaskId::new("task_1".to_string());

        let read_action = tool_call_to_ai_action(
            &call("read_files", serde_json::json!({"files": ["src/lib.rs"]})),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            read_action.action,
            AIAgentActionType::ReadFiles(ref request)
                if request.locations.iter().any(|location| location.name == "src/lib.rs")
        ));

        let grep_action = tool_call_to_ai_action(
            &call("grep", serde_json::json!({"pattern": "needle"})),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            grep_action.action,
            AIAgentActionType::Grep { ref queries, ref path }
                if queries == &vec!["needle".to_string()] && path == "."
        ));

        let glob_action = tool_call_to_ai_action(
            &call(
                "file_glob_v2",
                serde_json::json!({"pattern": "**/*.rs", "path": "crates"}),
            ),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            glob_action.action,
            AIAgentActionType::FileGlobV2 { ref patterns, ref search_dir }
                if patterns == &vec!["**/*.rs".to_string()] && search_dir.as_deref() == Some("crates")
        ));
    }

    #[test]
    fn tool_calls_reject_non_object_arguments() {
        assert_invalid_input(
            call("read_files", serde_json::json!("src/lib.rs")),
            "requires object arguments",
        );
    }

    #[test]
    fn tool_calls_reject_unsupported_arguments() {
        assert_invalid_input(
            call(
                "run_shell_command",
                serde_json::json!({"command": "pwd", "cwd": "/tmp"}),
            ),
            "does not support argument",
        );
    }

    #[test]
    fn tool_calls_reject_wrong_argument_types() {
        let cases = [
            (
                call("run_shell_command", serde_json::json!({"command": 1})),
                "command` must be a string",
            ),
            (
                call(
                    "run_shell_command",
                    serde_json::json!({"command": "pwd", "is_read_only": "yes"}),
                ),
                "is_read_only` must be a boolean",
            ),
            (
                call(
                    "read_files",
                    serde_json::json!({"paths": ["src/lib.rs", 1]}),
                ),
                "paths` must contain only strings",
            ),
            (
                call("grep", serde_json::json!({"queries": []})),
                "requires non-empty string array argument `queries`",
            ),
            (
                call("file_glob_v2", serde_json::json!({"pattern": ""})),
                "pattern` must be a non-empty string",
            ),
            (
                call(
                    "search_codebase",
                    serde_json::json!({"query": "runtime loop", "path_filters": ["app", false]}),
                ),
                "path_filters` must contain only strings",
            ),
        ];

        for (call, expected_reason) in cases {
            assert_invalid_input(call, expected_reason);
        }
    }

    #[test]
    fn tool_calls_reject_empty_required_values() {
        let cases = [
            (
                call("run_shell_command", serde_json::json!({"command": "  "})),
                "command` must be a non-empty string",
            ),
            (
                call("read_files", serde_json::json!({"paths": []})),
                "requires non-empty string array argument `paths`",
            ),
            (
                call("search_codebase", serde_json::json!({"query": ""})),
                "query` must be a non-empty string",
            ),
        ];

        for (call, expected_reason) in cases {
            assert_invalid_input(call, expected_reason);
        }
    }

    #[test]
    fn tool_calls_reject_invalid_preferred_alias_instead_of_falling_back() {
        assert_invalid_input(
            call(
                "grep",
                serde_json::json!({"queries": 1, "pattern": "needle"}),
            ),
            "queries` must be a string array",
        );
        assert_invalid_input(
            call(
                "file_glob_v2",
                serde_json::json!({"patterns": 1, "pattern": "**/*.rs"}),
            ),
            "patterns` must be a string array",
        );
    }

    #[test]
    fn tool_calls_accept_stringified_string_arrays() {
        let task_id = TaskId::new("task_1".to_string());
        let read_action = tool_call_to_ai_action(
            &call(
                "read_files",
                serde_json::json!({"paths": "[\"src/lib.rs\"]"}),
            ),
            &task_id,
        )
        .unwrap();

        assert!(matches!(
            read_action.action,
            AIAgentActionType::ReadFiles(ref request)
                if request.locations.iter().any(|location| location.name == "src/lib.rs")
        ));
    }

    #[test]
    fn tool_calls_reject_stringified_arrays_with_non_strings() {
        assert_invalid_input(
            call(
                "read_files",
                serde_json::json!({"paths": "[\"src/lib.rs\", 1]"}),
            ),
            "paths` must contain only strings",
        );
    }

    #[test]
    fn supported_tool_names_match_advertised_schema_names() {
        let advertised_names = build_tool_schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();

        for name in &advertised_names {
            assert!(is_supported_tool(name));
        }

        assert_eq!(
            advertised_names,
            vec![
                "run_shell_command",
                "read_files",
                "grep",
                "file_glob_v2",
                "search_codebase",
                "edit_files"
            ]
        );
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

        let edit_action = tool_call_to_ai_action(
            &call(
                "edit_files",
                serde_json::json!({
                    "title": "Update greeting",
                    "edits": [
                        {
                            "type": "replace",
                            "file": "src/lib.rs",
                            "search": "hello",
                            "replace": "hi"
                        },
                        {
                            "type": "create",
                            "file": "README.md",
                            "content": "# Readme\n"
                        },
                        {
                            "type": "delete",
                            "file": "old.txt"
                        }
                    ]
                }),
            ),
            &task_id,
        )
        .unwrap();
        assert!(matches!(
            edit_action.action,
            AIAgentActionType::RequestFileEdits { ref file_edits, ref title }
                if title.as_deref() == Some("Update greeting")
                    && matches!(
                        &file_edits[0],
                        FileEdit::Edit(ParsedDiff::StrReplaceEdit { file, search, replace })
                            if file.as_deref() == Some("src/lib.rs")
                                && search.as_deref() == Some("hello")
                                && replace.as_deref() == Some("hi")
                    )
                    && matches!(
                        &file_edits[1],
                        FileEdit::Create { file, content }
                            if file.as_deref() == Some("README.md")
                                && content.as_deref() == Some("# Readme\n")
                    )
                    && matches!(
                        &file_edits[2],
                        FileEdit::Delete { file }
                            if file.as_deref() == Some("old.txt")
                    )
        ));
    }

    #[test]
    fn unsupported_tool_is_not_silently_mapped() {
        let task_id = TaskId::new("task_1".to_string());
        let err = tool_call_to_ai_action(
            &call("write_file", serde_json::json!({"path": "src/lib.rs"})),
            &task_id,
        )
        .unwrap_err();

        assert!(matches!(err, ToolExecutionError::NotFound { name } if name == "write_file"));
    }

    #[test]
    fn edit_files_maps_to_apply_file_diffs_proto_for_ui_rendering() {
        let proto = tool_call_to_proto_tool(&call(
            "edit_files",
            serde_json::json!({
                "title": "Update greeting",
                "edits": [
                    {
                        "type": "replace",
                        "file": "/tmp/warp-agent-easy/hello.rs",
                        "search": "println!(\"Hello\");",
                        "replace": "println!(\"Hello from the local agent!\");"
                    }
                ]
            }),
        ))
        .unwrap();

        let api::message::tool_call::Tool::ApplyFileDiffs(diff) = proto else {
            panic!("expected ApplyFileDiffs tool call");
        };

        assert_eq!(diff.summary, "Update greeting");
        assert_eq!(diff.diffs.len(), 1);
        assert_eq!(diff.diffs[0].file_path, "/tmp/warp-agent-easy/hello.rs");
        assert_eq!(diff.diffs[0].search, "println!(\"Hello\");");
        assert_eq!(
            diff.diffs[0].replace,
            "println!(\"Hello from the local agent!\");"
        );
        assert!(diff.new_files.is_empty());
        assert!(diff.deleted_files.is_empty());
        assert!(diff.v4a_updates.is_empty());
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
    use std::sync::Arc;

    use local_agent_runtime::{FinishReason, RuntimeEvent};
    use uuid::Uuid;
    use warp_multi_agent_api as api;

    use super::{
        encode_local_runtime_tool_call_data, tool_call_to_proto_tool_with_registry,
        LocalRuntimeToolRegistry,
    };

    /// State for mapping runtime events to proto ResponseEvents.
    pub struct EventMapper {
        pub conversation_id: String,
        pub request_id: String,
        pub run_id: String,
        pub task_id: String,
        registry: Arc<LocalRuntimeToolRegistry>,
        task_created: bool,
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
                current_text_message_id: None,
            }
        }

        /// Map a single RuntimeEvent to zero or more ResponseEvents.
        pub fn map_event(&mut self, event: &RuntimeEvent) -> Vec<api::ResponseEvent> {
            match event {
                RuntimeEvent::TurnStarted { turn } => {
                    self.current_text_message_id = None;
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
                RuntimeEvent::TextDelta { text } => {
                    if text.is_empty() {
                        return vec![];
                    }

                    let mut actions = Vec::new();
                    actions.push(begin_transaction());

                    if !self.task_created {
                        actions.push(create_task(&self.task_id));
                        self.task_created = true;
                    }

                    match self.current_text_message_id.clone() {
                        Some(message_id) => {
                            actions.push(append_agent_output(
                                &self.task_id,
                                &message_id,
                                &self.request_id,
                                text,
                            ));
                        }
                        None => {
                            let message_id = Uuid::new_v4().to_string();
                            actions.push(add_agent_output(
                                &self.task_id,
                                &message_id,
                                &self.request_id,
                                text,
                            ));
                            self.current_text_message_id = Some(message_id);
                        }
                    }

                    actions.push(commit_transaction());

                    vec![api::ResponseEvent {
                        r#type: Some(api::response_event::Type::ClientActions(
                            api::response_event::ClientActions { actions },
                        )),
                    }]
                }
                RuntimeEvent::TextCompleted { text } => {
                    if self.current_text_message_id.is_some() {
                        return vec![];
                    }

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
                    self.current_text_message_id = Some(message_id);
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
                        if let Some(action) = tool_call_to_proto_action(
                            &self.task_id,
                            &self.request_id,
                            call,
                            &self.registry,
                        ) {
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
        registry: &LocalRuntimeToolRegistry,
    ) -> Option<api::ClientAction> {
        let proto_tool = tool_call_to_proto_tool_with_registry(call, registry).ok()?;

        let message_id = Uuid::new_v4().to_string();
        let message = api::Message {
            id: message_id,
            task_id: task_id.to_string(),
            request_id: request_id.to_string(),
            timestamp: None,
            server_message_data: encode_local_runtime_tool_call_data(call),
            citations: vec![],
            fetched_memories: vec![],
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

    fn append_agent_output(
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
            action: Some(api::client_action::Action::AppendToMessageContent(
                api::client_action::AppendToMessageContent {
                    task_id: task_id.to_string(),
                    message: Some(message),
                    mask: Some(prost_types::FieldMask {
                        paths: vec!["agent_output.text".to_string()],
                    }),
                },
            )),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn client_actions(event: &api::ResponseEvent) -> &[api::ClientAction] {
            let Some(api::response_event::Type::ClientActions(client_actions)) = &event.r#type
            else {
                panic!("expected client actions event");
            };
            &client_actions.actions
        }

        #[test]
        fn text_delta_creates_then_appends_agent_output_message() {
            let mut mapper = EventMapper::new(
                "conversation_1".to_string(),
                "request_1".to_string(),
                "run_1".to_string(),
                "task_1".to_string(),
                true,
                Arc::new(LocalRuntimeToolRegistry::built_ins()),
            );

            let first = mapper.map_event(&RuntimeEvent::TextDelta {
                text: "hel".to_string(),
            });
            let second = mapper.map_event(&RuntimeEvent::TextDelta {
                text: "lo".to_string(),
            });

            let first_actions = client_actions(&first[0]);
            assert_eq!(first_actions.len(), 3);
            let Some(api::client_action::Action::AddMessagesToTask(add)) = &first_actions[1].action
            else {
                panic!("expected AddMessagesToTask action");
            };
            let message_id = add.messages[0].id.clone();
            let Some(api::message::Message::AgentOutput(output)) = &add.messages[0].message else {
                panic!("expected agent output message");
            };
            assert_eq!(output.text, "hel");

            let second_actions = client_actions(&second[0]);
            assert_eq!(second_actions.len(), 3);
            let Some(api::client_action::Action::AppendToMessageContent(append)) =
                &second_actions[1].action
            else {
                panic!("expected AppendToMessageContent action");
            };
            assert_eq!(append.task_id, "task_1");
            assert_eq!(
                append.mask.as_ref().unwrap().paths,
                vec!["agent_output.text"]
            );

            let message = append.message.as_ref().unwrap();
            assert_eq!(message.id, message_id);
            let Some(api::message::Message::AgentOutput(output)) = &message.message else {
                panic!("expected agent output message");
            };
            assert_eq!(output.text, "lo");

            let merged = field_mask::FieldMaskOperation::append(
                &api::MESSAGE_DESCRIPTOR,
                &add.messages[0],
                message,
                append.mask.clone().unwrap(),
            )
            .apply()
            .expect("append field mask should apply");
            let Some(api::message::Message::AgentOutput(output)) = &merged.message else {
                panic!("expected merged agent output message");
            };
            assert_eq!(output.text, "hello");
        }

        #[test]
        fn edit_files_tool_call_maps_to_apply_file_diffs_client_action() {
            let mut mapper = EventMapper::new(
                "conversation_1".to_string(),
                "request_1".to_string(),
                "run_1".to_string(),
                "task_1".to_string(),
                true,
                Arc::new(LocalRuntimeToolRegistry::built_ins()),
            );

            let events = mapper.map_event(&RuntimeEvent::ToolCallsRequested {
                calls: vec![local_agent_runtime::ToolCall {
                    id: "call_1".to_string(),
                    name: "edit_files".to_string(),
                    arguments: serde_json::json!({
                        "title": "Update greeting",
                        "edits": [
                            {
                                "type": "replace",
                                "file": "/tmp/warp-agent-easy/hello.rs",
                                "search": "println!(\"Hello\");",
                                "replace": "println!(\"Hello from the local agent!\");"
                            }
                        ]
                    }),
                }],
            });

            let actions = client_actions(&events[0]);
            let Some(api::client_action::Action::AddMessagesToTask(add)) = &actions[1].action
            else {
                panic!("expected AddMessagesToTask action");
            };
            let Some(api::message::Message::ToolCall(tool_call)) = &add.messages[0].message else {
                panic!("expected tool call message");
            };
            let Some(api::message::tool_call::Tool::ApplyFileDiffs(diff)) = &tool_call.tool else {
                panic!("expected ApplyFileDiffs tool");
            };

            assert_eq!(tool_call.tool_call_id, "call_1");
            assert_eq!(diff.summary, "Update greeting");
            assert_eq!(diff.diffs.len(), 1);
            assert_eq!(diff.diffs[0].file_path, "/tmp/warp-agent-easy/hello.rs");
        }
    }
}
