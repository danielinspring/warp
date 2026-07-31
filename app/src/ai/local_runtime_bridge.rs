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
use std::sync::{Arc, Mutex};

use ai::agent::action::FileEdit;
use ai::diff_validation::ParsedDiff;
use ai::document::AIDocumentId;
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
    AIAgentActionType, AIAgentInput, AIAgentPtyWriteMode, AnyFileContent, AskUserQuestionItem,
    AskUserQuestionOption, AskUserQuestionType, CreateDocumentsRequest, CreateDocumentsResult,
    DocumentDiff, DocumentToCreate, EditDocumentsRequest, EditDocumentsResult, FileContext,
    FileGlobV2Result, GrepResult, ReadDocumentsRequest, ReadDocumentsResult, ReadFilesResult,
    ReadShellCommandOutputResult, RequestCommandOutputResult, RequestComputerUseRequest,
    RequestComputerUseResult, RunAgentsAgentOutcomeKind, RunAgentsAgentRunConfig,
    RunAgentsExecutionMode, RunAgentsLaunchedExecutionMode, RunAgentsRequest, RunAgentsResult,
    SearchCodebaseResult, ShellCommandDelay, UseComputerRequest, UseComputerResult,
    WriteToLongRunningShellCommandResult,
};
use crate::terminal::model::block::BlockId;

/// Maximum child agents a local-runtime `run_agents` call may request.
/// Keeps local orchestration bounded relative to cloud-scale fan-out.
pub const LOCAL_RUN_AGENTS_MAX_CHILDREN: usize = 4;

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

/// Compact skill entry for list_skills and system-prompt discovery.
#[derive(Debug, Clone)]
pub struct LocalRuntimeSkillInfo {
    pub name: String,
    pub description: String,
    pub reference: String,
    pub scope: String,
}

/// Persistence payload registered during in-process tool execution and consumed
/// when mapping [`RuntimeEvent::ToolResult`] into Warp task messages.
#[derive(Debug, Clone)]
pub struct LocalToolPersistence {
    pub todo_update: Option<api::message::UpdateTodos>,
}

#[derive(Debug, Clone)]
pub struct LocalRuntimeToolRegistry {
    schemas: Vec<ToolSchema>,
    routes: HashMap<String, LocalRuntimeToolRoute>,
    permission_mode: LocalRuntimePermissionMode,
    skill_catalog: Vec<LocalRuntimeSkillInfo>,
    todo_state: Arc<Mutex<crate::ai::local_todos::LocalTodoState>>,
    /// call_id → side effects to emit when the runtime reports ToolResult.
    pending_local_persistence: Arc<Mutex<HashMap<String, LocalToolPersistence>>>,
    /// Session default working directory, used to resolve local git tool
    /// calls that omit an explicit `repo_path`.
    working_directory: Option<PathBuf>,
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
    /// Catalog listing answered in-process from `skill_catalog` (no UI action).
    ListSkills,
    /// Local HTTP web_search / web_fetch (in-process; no cloud action).
    LocalWeb,
    /// Local durable todos (in-process + UpdateTodos task messages).
    LocalTodo,
    /// Local read-only git workflow tools (in-process; no cloud action).
    LocalGit,
}

impl LocalRuntimeToolRegistry {
    pub fn from_request(params: &RequestParams) -> Self {
        Self::from_request_with_available_skills(params, &[])
    }

    /// Build a registry, merging request-context skills with an optional cwd/bundled catalog.
    pub fn from_request_with_available_skills(
        params: &RequestParams,
        available_skills: &[crate::ai::skills::SkillDescriptor],
    ) -> Self {
        let mut registry = Self::built_ins();
        registry.permission_mode = LocalRuntimePermissionMode::from_request(params);
        registry.working_directory = params
            .session_context
            .current_working_directory()
            .clone()
            .map(PathBuf::from);

        if let Some(context) = &params.mcp_context {
            registry.add_mcp_context(context);
        }

        let mut skills = HashMap::new();
        let mut catalog = Vec::new();
        for skill in available_skills {
            let reference = skill.reference.to_string();
            skills.insert(skill.name.clone(), skill.reference.clone());
            skills.insert(reference.clone(), skill.reference.clone());
            catalog.push(LocalRuntimeSkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
                reference,
                scope: format!("{:?}", skill.scope),
            });
        }
        // Request-context skills (user-attached) merge into lookup + catalog.
        for input in &params.input {
            let Some(contexts) = input.context() else {
                continue;
            };
            for context in contexts {
                let crate::ai::agent::AIAgentContext::Skills {
                    skills: context_skills,
                } = context
                else {
                    continue;
                };
                for skill in context_skills {
                    let reference = skill.reference.to_string();
                    skills.insert(skill.name.clone(), skill.reference.clone());
                    skills.insert(reference.clone(), skill.reference.clone());
                    if !catalog
                        .iter()
                        .any(|entry| entry.reference == reference || entry.name == skill.name)
                    {
                        catalog.push(LocalRuntimeSkillInfo {
                            name: skill.name.clone(),
                            description: String::new(),
                            reference,
                            scope: "context".to_string(),
                        });
                    }
                }
            }
        }
        if !skills.is_empty() {
            registry.skill_catalog = catalog;
            registry.add_list_skills();
            registry.add_read_skill(skills);
        }
        if params.ask_user_question_enabled {
            registry.add_ask_user_question();
        }
        // Depth bound: only root agents may advertise run_agents so children
        // cannot recursively fan out further local orchestration.
        if params.orchestration_enabled && params.parent_agent_id.is_none() {
            registry.add_run_agents();
        }
        // AI documents are client-executed; always bridge for local agents.
        registry.add_document_tools();
        // Computer use is gated to the same request flag as cloud/Oz agent mode.
        if params.computer_use_enabled {
            registry.add_computer_use_tools();
        }
        // Local web search/fetch: request flag only (no cloud server tool).
        if params.web_search_enabled {
            registry.add_web_tools();
        }
        // Durable todos: always available locally (UI + transcript replay).
        registry.add_todo_tools();
        // Read-only git workflow tools: always available locally, like todos.
        registry.add_git_tools();
        {
            let mut todos = registry
                .todo_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            todos.hydrate_from_tasks(&params.tasks);
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
            skill_catalog: Vec::new(),
            todo_state: Arc::new(Mutex::new(crate::ai::local_todos::LocalTodoState::default())),
            pending_local_persistence: Arc::new(Mutex::new(HashMap::new())),
            working_directory: None,
        };

        for schema in build_tool_schemas() {
            let safety_class = match schema.name.as_str() {
                "run_shell_command" | "edit_files" | "write_to_long_running_shell_command" => {
                    ToolSafetyClass::Interactive
                }
                "read_files"
                | "grep"
                | "file_glob_v2"
                | "search_codebase"
                | "read_shell_command_output" => ToolSafetyClass::ReadOnly,
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

    pub fn skill_catalog(&self) -> &[LocalRuntimeSkillInfo] {
        &self.skill_catalog
    }

    pub fn list_skills_json(&self) -> String {
        let skills = self
            .skill_catalog
            .iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.name,
                    "description": skill.description,
                    "reference": skill.reference,
                    "scope": skill.scope,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "skills": skills,
            "instruction": "Use read_skill with name or reference to load full skill instructions. For scripts or files listed inside a skill, use read_files with paths relative to the skill directory or absolute paths from the skill body.",
        })
        .to_string()
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

    fn add_run_agents(&mut self) {
        self.add_tool(
            run_agents_schema(),
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
    }

    fn add_document_tools(&mut self) {
        self.add_tool(
            read_documents_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
        self.add_tool(
            edit_documents_schema(),
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
        self.add_tool(
            create_documents_schema(),
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
    }

    fn add_computer_use_tools(&mut self) {
        self.add_tool(
            request_computer_use_schema(),
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
        self.add_tool(
            use_computer_schema(),
            ToolSafetyClass::Interactive,
            LocalRuntimeToolRouteKind::BuiltIn,
        );
    }

    fn add_list_skills(&mut self) {
        self.add_tool(
            ToolSchemaBuilder::new(
                "list_skills",
                "List available Warp skills for this session (project, home, and activated bundled skills). Use read_skill to load full instructions for one skill.",
            )
            .build(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::ListSkills,
        );
    }

    fn add_web_tools(&mut self) {
        self.add_tool(
            crate::ai::local_web::web_search_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalWeb,
        );
        self.add_tool(
            crate::ai::local_web::web_fetch_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalWeb,
        );
    }

    fn add_todo_tools(&mut self) {
        self.add_tool(
            crate::ai::local_todos::update_todos_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalTodo,
        );
        self.add_tool(
            crate::ai::local_todos::mark_todos_completed_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalTodo,
        );
    }

    fn add_git_tools(&mut self) {
        self.add_tool(
            crate::ai::local_git::git_status_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalGit,
        );
        self.add_tool(
            crate::ai::local_git::draft_commit_message_context_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalGit,
        );
        self.add_tool(
            crate::ai::local_git::draft_pr_summary_context_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalGit,
        );
    }

    pub fn todo_prompt_section(&self) -> Option<String> {
        self.todo_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prompt_section()
    }

    pub fn register_local_tool_persistence(
        &self,
        call_id: String,
        persistence: LocalToolPersistence,
    ) {
        self.pending_local_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call_id, persistence);
    }

    pub fn take_local_tool_persistence(&self, call_id: &str) -> Option<LocalToolPersistence> {
        self.pending_local_persistence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(call_id)
    }

    fn add_read_skill(&mut self, skill_lookup: HashMap<String, SkillReference>) {
        self.add_tool(
            ToolSchemaBuilder::new(
                "read_skill",
                "Read full instructions for an available Warp skill by name or reference (from list_skills). After reading, follow the skill. For bundled script/assets mentioned in the skill, use read_files on those paths.",
            )
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
            route.safety_class == ToolSafetyClass::ReadOnly
                || name == "ask_user_question"
                || name == "list_skills"
                || name == "read_skill"
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
    model_family: crate::ai::local_runtime_model_packs::ModelFamily,
}

impl WarpToolExecutor {
    pub fn new_with_model_family(
        request_tx: async_channel::Sender<ToolExecutionRequest>,
        registry: Arc<LocalRuntimeToolRegistry>,
        task_id: TaskId,
        request_id: String,
        model_family: crate::ai::local_runtime_model_packs::ModelFamily,
    ) -> Self {
        Self {
            registry,
            request_tx,
            task_id,
            request_id,
            model_family,
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for WarpToolExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        crate::ai::local_runtime_model_packs::apply_schema_tweaks(
            self.model_family,
            self.registry.schemas(),
        )
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
        // Catalog listing is answered from the request-scoped registry without a UI action.
        if call.name == "list_skills" {
            if !self.registry.contains_tool("list_skills") {
                return Err(ToolExecutionError::NotFound {
                    name: call.name.clone(),
                });
            }
            return Ok(ToolCallResult {
                content: self.registry.list_skills_json(),
                is_error: false,
            });
        }

        // Local web tools execute in-process with SSRF policy (no Warp action card).
        if call.name == "web_search" || call.name == "web_fetch" {
            if !self.registry.contains_tool(&call.name) {
                return Err(ToolExecutionError::NotFound {
                    name: call.name.clone(),
                });
            }
            return crate::ai::local_web::execute_web_tool(call).await;
        }

        // Local git workflow tools execute in-process (no Warp action card, no mutation).
        if call.name == "git_status"
            || call.name == "draft_commit_message_context"
            || call.name == "draft_pr_summary_context"
        {
            if !self.registry.contains_tool(&call.name) {
                return Err(ToolExecutionError::NotFound {
                    name: call.name.clone(),
                });
            }
            return crate::ai::local_git::execute_git_tool(
                call,
                self.registry.working_directory.as_deref(),
            )
            .await;
        }

        // Local durable todos: update session state + queue UpdateTodos for the event mapper.
        if call.name == "update_todos" || call.name == "mark_todos_completed" {
            if !self.registry.contains_tool(&call.name) {
                return Err(ToolExecutionError::NotFound {
                    name: call.name.clone(),
                });
            }
            let mut todos = self
                .registry
                .todo_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (result, side) = crate::ai::local_todos::execute_todo_tool(call, &mut todos)?;
            drop(todos);
            if let Some(crate::ai::local_todos::LocalTodoSideEffect::UpdateTodos(update)) = side {
                self.registry.register_local_tool_persistence(
                    call.id.clone(),
                    LocalToolPersistence {
                        todo_update: Some(update),
                    },
                );
            } else {
                // Still persist tool result even when mark found no ids.
                self.registry.register_local_tool_persistence(
                    call.id.clone(),
                    LocalToolPersistence { todo_update: None },
                );
            }
            return Ok(result);
        }

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
            "Run a shell command in the user's terminal. Use this for any system operation. For read-only lookups (find, ls, pwd, cat, mdfind, grep, python3 -c, etc.) set is_read_only=true so the command can auto-run. On macOS prefer python3 (not python). For quick math/scripts use: python3 -c 'print(sum(range(1, 101)))'. To locate a directory by name outside the project, prefer a FAST scoped search — on macOS: mdfind 'kMDItemFSName == \"folder-name\"c' | head -20; or find ~/codish -maxdepth 5 -type d -name 'folder-name' 2>/dev/null. Avoid unbounded find ~ without -maxdepth. For servers/tests that must keep running, set wait_until_complete=false, then use read_shell_command_output / write_to_long_running_shell_command with the returned block_id.",
        )
        .required_string("command", "The shell command to execute")
        .optional_bool("is_read_only", "Set true for commands with no side effects (find, ls, pwd, cat, mdfind, grep, python3 -c). Defaults to an automatic read-only heuristic when omitted.")
        .optional_bool("is_risky", "Whether the command should require user confirmation")
        .optional_bool("uses_pager", "Whether the command may open a pager")
        .optional_bool(
            "wait_until_complete",
            "When true (default), wait for the command to finish (capped). When false, return a long-running snapshot with block_id so you can poll or write with the LRC tools.",
        )
        .build(),
        ToolSchemaBuilder::new(
            "read_shell_command_output",
            "Read output from a long-running shell command previously started with wait_until_complete=false. Use the block_id from the long-running snapshot.",
        )
        .required_string(
            "block_id",
            "Block id from a long-running shell snapshot (also accepted as command_id)",
        )
        .optional_bool(
            "wait_until_complete",
            "When true, wait until the command finishes before returning. When false/omitted, return the current snapshot after a short delay.",
        )
        .build(),
        ToolSchemaBuilder::new(
            "write_to_long_running_shell_command",
            "Write input to a long-running shell command (REPL/server) identified by block_id. Prefer mode=line for interactive shells.",
        )
        .required_string(
            "block_id",
            "Block id from a long-running shell snapshot (also accepted as command_id)",
        )
        .required_string("input", "Text or bytes to write to the PTY")
        .optional_string(
            "mode",
            "Write mode: raw (default), line (send enter after text), or block (bracketed paste)",
        )
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

fn run_agents_schema() -> ToolSchema {
    ToolSchema {
        name: "run_agents".to_string(),
        description: format!(
            "Delegate independent work to up to {LOCAL_RUN_AGENTS_MAX_CHILDREN} parallel child agents via Warp orchestration. Use only for parallelizable subtasks; prefer direct tools for sequential work. Local execution only. Child agents cannot call run_agents again."
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Short human-readable summary of what the child agents will do"
                },
                "base_prompt": {
                    "type": "string",
                    "description": "Optional shared instructions prepended to every child agent prompt"
                },
                "agents": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": LOCAL_RUN_AGENTS_MAX_CHILDREN,
                    "description": "One entry per child agent to launch",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Stable short name for the child agent"
                            },
                            "prompt": {
                                "type": "string",
                                "description": "Task prompt for this child agent"
                            },
                            "title": {
                                "type": "string",
                                "description": "Optional display title for the child conversation"
                            }
                        },
                        "required": ["name", "prompt"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["summary", "agents"],
            "additionalProperties": false
        }),
    }
}

fn read_documents_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "read_documents",
        "Read Warp AI documents by UUID. Use document IDs from conversation context or prior create/edit results.",
    )
    .required_string_array(
        "document_ids",
        "One or more AI document UUIDs to read",
    )
    .build()
}

fn edit_documents_schema() -> ToolSchema {
    ToolSchema {
        name: "edit_documents".to_string(),
        description: "Apply search/replace edits to existing Warp AI documents by UUID."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "diffs": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "document_id": { "type": "string", "description": "AI document UUID" },
                            "search": { "type": "string" },
                            "replace": { "type": "string" }
                        },
                        "required": ["document_id", "search", "replace"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["diffs"],
            "additionalProperties": false
        }),
    }
}

fn create_documents_schema() -> ToolSchema {
    ToolSchema {
        name: "create_documents".to_string(),
        description: "Create one or more new Warp AI documents with title and content.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "documents": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "content": { "type": "string" }
                        },
                        "required": ["title", "content"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["documents"],
            "additionalProperties": false
        }),
    }
}

fn request_computer_use_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "request_computer_use",
        "Request user approval to control the computer (mouse/keyboard/screenshots). Call before use_computer when permission is required.",
    )
    .required_string(
        "task_summary",
        "Short summary of the computer-use task for the user approval UI",
    )
    .build()
}

fn use_computer_schema() -> ToolSchema {
    ToolSchema {
        name: "use_computer".to_string(),
        description: "Perform local computer-use actions (type text, move/click mouse, wait, keys). Prefer after request_computer_use is approved. Action objects follow Warp computer_use Action JSON (type_text, wait, mouse_move, mouse_down, mouse_up, etc.).".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action_summary": {
                    "type": "string",
                    "description": "Short summary of this action batch"
                },
                "actions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Ordered computer_use::Action values as JSON objects",
                    "items": { "type": "object" }
                },
                "take_screenshot": {
                    "type": "boolean",
                    "description": "When true, capture a screenshot after actions (metadata returned; image not embedded for local models)"
                }
            },
            "required": ["action_summary", "actions"],
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
        LocalRuntimeToolRouteKind::ListSkills => {
            return Err(ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                "list_skills is handled in-process and should not be queued as a Warp action"
            )));
        }
        LocalRuntimeToolRouteKind::LocalWeb => {
            return Err(ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                "{} is handled in-process and should not be queued as a Warp action",
                call.name
            )));
        }
        LocalRuntimeToolRouteKind::LocalTodo => {
            return Err(ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                "{} is handled in-process and should not be queued as a Warp action",
                call.name
            )));
        }
        LocalRuntimeToolRouteKind::LocalGit => {
            return Err(ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                "{} is handled in-process and should not be queued as a Warp action",
                call.name
            )));
        }
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
    if call.name == "run_agents" {
        return run_agents_tool_call_to_ai_action(call);
    }
    if call.name == "read_shell_command_output" {
        return read_shell_command_output_tool_call_to_ai_action(call);
    }
    if call.name == "write_to_long_running_shell_command" {
        return write_to_long_running_shell_command_tool_call_to_ai_action(call);
    }
    if call.name == "read_documents" {
        return read_documents_tool_call_to_ai_action(call);
    }
    if call.name == "edit_documents" {
        return edit_documents_tool_call_to_ai_action(call);
    }
    if call.name == "create_documents" {
        return create_documents_tool_call_to_ai_action(call);
    }
    if call.name == "request_computer_use" {
        return request_computer_use_tool_call_to_ai_action(call);
    }
    if call.name == "use_computer" {
        return use_computer_tool_call_to_ai_action(call);
    }

    let action: AIAgentActionType = match tool_call_to_proto_tool(call)? {
        api::message::tool_call::Tool::RunShellCommand(tool) => tool.into(),
        api::message::tool_call::Tool::ReadFiles(tool) => tool.into(),
        api::message::tool_call::Tool::Grep(tool) => tool.into(),
        api::message::tool_call::Tool::FileGlobV2(tool) => tool.into(),
        api::message::tool_call::Tool::SearchCodebase(tool) => tool.into(),
        api::message::tool_call::Tool::ReadShellCommandOutput(tool) => tool.into(),
        api::message::tool_call::Tool::WriteToLongRunningShellCommand(tool) => tool.into(),
        _ => {
            return Err(ToolExecutionError::NotFound {
                name: call.name.clone(),
            });
        }
    };

    Ok(action)
}

fn read_shell_command_output_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(
        &call.arguments,
        &["block_id", "command_id", "wait_until_complete"],
        &call.name,
    )?;
    let block_id = required_string_any(&call.arguments, &["block_id", "command_id"], &call.name)?;
    let wait_until_complete =
        optional_bool(&call.arguments, "wait_until_complete")?.unwrap_or(false);
    let delay = if wait_until_complete {
        Some(ShellCommandDelay::OnCompletion)
    } else {
        None
    };
    Ok(AIAgentActionType::ReadShellCommandOutput {
        block_id: BlockId::from(block_id),
        delay,
    })
}

fn write_to_long_running_shell_command_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(
        &call.arguments,
        &["block_id", "command_id", "input", "mode"],
        &call.name,
    )?;
    let block_id = required_string_any(&call.arguments, &["block_id", "command_id"], &call.name)?;
    let input = required_string(&call.arguments, "input", &call.name)?;
    let mode = match optional_string(&call.arguments, "mode")?
        .unwrap_or_else(|| "raw".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "raw" => AIAgentPtyWriteMode::Raw,
        "line" => AIAgentPtyWriteMode::Line,
        "block" => AIAgentPtyWriteMode::Block,
        other => {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!(
                    "Tool `write_to_long_running_shell_command` mode must be raw, line, or block (got {other})"
                ),
            });
        }
    };
    Ok(AIAgentActionType::WriteToLongRunningShellCommand {
        block_id: BlockId::from(block_id),
        input: bytes::Bytes::from(input),
        mode,
    })
}

fn required_string_any(
    arguments: &Value,
    keys: &[&str],
    tool_name: &str,
) -> Result<String, ToolExecutionError> {
    for key in keys {
        if let Some(value) = optional_string(arguments, key)? {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    Err(ToolExecutionError::InvalidInput {
        reason: format!("Tool `{tool_name}` requires one of: {}", keys.join(" or ")),
    })
}

fn read_documents_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["document_ids"], &call.name)?;
    let ids = required_string_array(&call.arguments, &["document_ids"], &call.name)?;
    if ids.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `read_documents` requires at least one document_id".to_string(),
        });
    }
    let document_ids = ids
        .into_iter()
        .map(|id| {
            AIDocumentId::try_from(id.as_str()).map_err(|_| ToolExecutionError::InvalidInput {
                reason: format!("Invalid document_id UUID: {id}"),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AIAgentActionType::ReadDocuments(ReadDocumentsRequest {
        document_ids,
    }))
}

fn edit_documents_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["diffs"], &call.name)?;
    let diffs = call
        .arguments
        .get("diffs")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `edit_documents` requires array argument `diffs`".to_string(),
        })?;
    if diffs.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `edit_documents` requires at least one diff".to_string(),
        });
    }
    let diffs = diffs
        .iter()
        .map(|diff| {
            let obj = Value::Object(
                diff.as_object()
                    .ok_or_else(|| ToolExecutionError::InvalidInput {
                        reason: "`edit_documents.diffs` entries must be objects".to_string(),
                    })?
                    .clone(),
            );
            validate_allowed_arguments(
                &obj,
                &["document_id", "search", "replace"],
                "edit_documents diff",
            )?;
            let document_id = required_string(&obj, "document_id", "edit_documents diff")?;
            let document_id = AIDocumentId::try_from(document_id.as_str()).map_err(|_| {
                ToolExecutionError::InvalidInput {
                    reason: format!("Invalid document_id UUID: {document_id}"),
                }
            })?;
            Ok(DocumentDiff {
                document_id,
                search: required_string(&obj, "search", "edit_documents diff")?,
                replace: required_string(&obj, "replace", "edit_documents diff")?,
            })
        })
        .collect::<Result<Vec<_>, ToolExecutionError>>()?;
    Ok(AIAgentActionType::EditDocuments(EditDocumentsRequest {
        diffs,
    }))
}

fn create_documents_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["documents"], &call.name)?;
    let documents = call
        .arguments
        .get("documents")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `create_documents` requires array argument `documents`".to_string(),
        })?;
    if documents.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `create_documents` requires at least one document".to_string(),
        });
    }
    let documents = documents
        .iter()
        .map(|doc| {
            let obj = Value::Object(
                doc.as_object()
                    .ok_or_else(|| ToolExecutionError::InvalidInput {
                        reason: "`create_documents.documents` entries must be objects".to_string(),
                    })?
                    .clone(),
            );
            validate_allowed_arguments(&obj, &["title", "content"], "create_documents document")?;
            Ok(DocumentToCreate {
                title: required_string(&obj, "title", "create_documents document")?,
                content: required_string(&obj, "content", "create_documents document")?,
            })
        })
        .collect::<Result<Vec<_>, ToolExecutionError>>()?;
    Ok(AIAgentActionType::CreateDocuments(CreateDocumentsRequest {
        documents,
    }))
}

fn request_computer_use_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["task_summary"], &call.name)?;
    let task_summary = required_string(&call.arguments, "task_summary", &call.name)?;
    if task_summary.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `request_computer_use` requires a non-empty task_summary".to_string(),
        });
    }
    Ok(AIAgentActionType::RequestComputerUse(
        RequestComputerUseRequest {
            task_summary,
            screenshot_params: Some(computer_use::ScreenshotParams {
                max_long_edge_px: None,
                max_total_px: None,
                region: None,
            }),
        },
    ))
}

fn use_computer_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(
        &call.arguments,
        &["action_summary", "actions", "take_screenshot"],
        &call.name,
    )?;
    let action_summary = required_string(&call.arguments, "action_summary", &call.name)?;
    let actions_value =
        call.arguments
            .get("actions")
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "Tool `use_computer` requires array argument `actions`".to_string(),
            })?;
    let actions: Vec<computer_use::Action> = serde_json::from_value(actions_value.clone())
        .map_err(|err| ToolExecutionError::InvalidInput {
            reason: format!("Tool `use_computer` actions JSON is invalid: {err}"),
        })?;
    if actions.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `use_computer` requires at least one action".to_string(),
        });
    }
    let take_screenshot = optional_bool(&call.arguments, "take_screenshot")?.unwrap_or(false);
    let screenshot_params = take_screenshot.then_some(computer_use::ScreenshotParams {
        max_long_edge_px: None,
        max_total_px: None,
        region: None,
    });
    Ok(AIAgentActionType::UseComputer(UseComputerRequest {
        action_summary,
        actions,
        screenshot_params,
    }))
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

fn run_agents_tool_call_to_ai_action(
    call: &ToolCall,
) -> Result<AIAgentActionType, ToolExecutionError> {
    validate_allowed_arguments(
        &call.arguments,
        &["summary", "base_prompt", "agents"],
        &call.name,
    )?;
    let summary = required_string(&call.arguments, "summary", &call.name)?;
    if summary.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `run_agents` requires a non-empty `summary`".to_string(),
        });
    }
    let base_prompt = optional_string(&call.arguments, "base_prompt")?.unwrap_or_default();
    let agents = call
        .arguments
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `run_agents` requires array argument `agents`".to_string(),
        })?;
    if agents.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `run_agents` requires at least one agent".to_string(),
        });
    }
    if agents.len() > LOCAL_RUN_AGENTS_MAX_CHILDREN {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "Tool `run_agents` allows at most {LOCAL_RUN_AGENTS_MAX_CHILDREN} child agents"
            ),
        });
    }

    let agent_run_configs = agents
        .iter()
        .map(parse_run_agents_agent)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AIAgentActionType::RunAgents(RunAgentsRequest {
        summary,
        base_prompt,
        skills: Vec::new(),
        // Leave model/harness empty so Warp's orchestration UI / profile defaults apply.
        model_id: String::new(),
        harness_type: String::new(),
        // Local-runtime orchestration is forced to local execution (no remote fan-out).
        execution_mode: RunAgentsExecutionMode::Local,
        agent_run_configs,
        plan_id: String::new(),
        harness_auth_secret_name: None,
    }))
}

fn parse_run_agents_agent(value: &Value) -> Result<RunAgentsAgentRunConfig, ToolExecutionError> {
    let arguments = Value::Object(
        value
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "`run_agents.agents` entries must be objects".to_string(),
            })?
            .clone(),
    );
    validate_allowed_arguments(&arguments, &["name", "prompt", "title"], "run_agents agent")?;
    let name = required_string(&arguments, "name", "run_agents agent")?;
    if name.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "`run_agents` agent `name` must be non-empty".to_string(),
        });
    }
    let prompt = required_string(&arguments, "prompt", "run_agents agent")?;
    if prompt.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "`run_agents` agent `prompt` must be non-empty".to_string(),
        });
    }
    let title = optional_string(&arguments, "title")?.unwrap_or_default();
    Ok(RunAgentsAgentRunConfig {
        name,
        prompt,
        title,
    })
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
        AIAgentActionResultType::RunAgents(result) => result.try_into().ok()?,
        AIAgentActionResultType::ReadShellCommandOutput(result) => result.try_into().ok()?,
        AIAgentActionResultType::WriteToLongRunningShellCommand(result) => {
            result.try_into().ok()?
        }
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
        RequestResult::RunAgentsResult(result) => Some(MessageResult::RunAgentsResult(result)),
        RequestResult::ReadShellCommandOutput(result) => {
            Some(MessageResult::ReadShellCommandOutput(result))
        }
        RequestResult::WriteToLongRunningShellCommand(result) => {
            Some(MessageResult::WriteToLongRunningShellCommand(result))
        }
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
            "instruction": "Command is still running. If stdout already answers the user, report that answer. Otherwise poll with read_shell_command_output or send input with write_to_long_running_shell_command using this block_id. Do not invent timeouts.",
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
        AIAgentActionResultType::RunAgents(result) => match result {
            RunAgentsResult::Launched {
                model_id,
                harness_type,
                execution_mode,
                agents,
            } => {
                let agents = agents
                    .iter()
                    .map(|agent| match &agent.kind {
                        RunAgentsAgentOutcomeKind::Launched { agent_id } => serde_json::json!({
                            "name": agent.name,
                            "status": "launched",
                            "agent_id": agent_id,
                        }),
                        RunAgentsAgentOutcomeKind::Failed { error } => serde_json::json!({
                            "name": agent.name,
                            "status": "failed",
                            "error": error,
                        }),
                    })
                    .collect::<Vec<_>>();
                let execution_mode = match execution_mode {
                    RunAgentsLaunchedExecutionMode::Local => serde_json::json!({ "type": "local" }),
                    RunAgentsLaunchedExecutionMode::Remote {
                        environment_id,
                        worker_host,
                        computer_use_enabled,
                    } => serde_json::json!({
                        "type": "remote",
                        "environment_id": environment_id,
                        "worker_host": worker_host,
                        "computer_use_enabled": computer_use_enabled,
                    }),
                };
                serde_json::json!({
                    "status": "launched",
                    "model_id": model_id,
                    "harness_type": harness_type,
                    "execution_mode": execution_mode,
                    "agents": agents,
                    "instruction": "Child agents were launched. Do not re-run the same orchestration. Use launched agent_ids if follow-up is needed; otherwise continue with local tools.",
                })
                .to_string()
            }
            RunAgentsResult::Denied { reason } => serde_json::json!({
                "status": "denied",
                "error": reason,
            })
            .to_string(),
            RunAgentsResult::Failure { error } => serde_json::json!({
                "status": "error",
                "error": error,
            })
            .to_string(),
            RunAgentsResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
        },
        AIAgentActionResultType::ReadShellCommandOutput(result) => match result {
            ReadShellCommandOutputResult::CommandFinished {
                command,
                output,
                exit_code,
                block_id,
                ..
            } => {
                let status = if exit_code.was_successful() {
                    "completed"
                } else {
                    "failed"
                };
                serde_json::json!({
                    "status": status,
                    "block_id": block_id.to_string(),
                    "exit_code": exit_code.value(),
                    "stdout": output,
                    "command": command,
                    "instruction": "Long-running command finished. Answer from stdout; do not invent timeouts.",
                })
                .to_string()
            }
            ReadShellCommandOutputResult::LongRunningCommandSnapshot {
                command,
                grid_contents,
                block_id,
                ..
            } => serde_json::json!({
                "status": "long_running",
                "block_id": block_id.to_string(),
                "stdout": grid_contents,
                "command": command,
                "instruction": "Still running. Poll again with read_shell_command_output or write with write_to_long_running_shell_command using block_id.",
            })
            .to_string(),
            ReadShellCommandOutputResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
            ReadShellCommandOutputResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": format!("{error:?}"),
            })
            .to_string(),
        },
        AIAgentActionResultType::WriteToLongRunningShellCommand(result) => match result {
            WriteToLongRunningShellCommandResult::Snapshot {
                block_id,
                grid_contents,
                ..
            } => serde_json::json!({
                "status": "long_running",
                "block_id": block_id.to_string(),
                "stdout": grid_contents,
                "instruction": "Input was written; command still running. Poll with read_shell_command_output if needed.",
            })
            .to_string(),
            WriteToLongRunningShellCommandResult::CommandFinished {
                block_id,
                output,
                exit_code,
                ..
            } => {
                let status = if exit_code.was_successful() {
                    "completed"
                } else {
                    "failed"
                };
                serde_json::json!({
                    "status": status,
                    "block_id": block_id.to_string(),
                    "exit_code": exit_code.value(),
                    "stdout": output,
                    "instruction": "Command finished after write. Answer from stdout.",
                })
                .to_string()
            }
            WriteToLongRunningShellCommandResult::Cancelled => serde_json::json!({
                "status": "cancelled",
            })
            .to_string(),
            WriteToLongRunningShellCommandResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": format!("{error:?}"),
            })
            .to_string(),
        },
        AIAgentActionResultType::ReadDocuments(result) => match result {
            ReadDocumentsResult::Success { documents } => {
                document_contexts_to_json("completed", documents)
            }
            ReadDocumentsResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": error,
            })
            .to_string(),
            ReadDocumentsResult::Cancelled => serde_json::json!({ "status": "cancelled" }).to_string(),
        },
        AIAgentActionResultType::EditDocuments(result) => match result {
            EditDocumentsResult::Success { updated_documents } => {
                document_contexts_to_json("accepted", updated_documents)
            }
            EditDocumentsResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": error,
            })
            .to_string(),
            EditDocumentsResult::Cancelled => serde_json::json!({ "status": "cancelled" }).to_string(),
        },
        AIAgentActionResultType::CreateDocuments(result) => match result {
            CreateDocumentsResult::Success { created_documents } => {
                document_contexts_to_json("created", created_documents)
            }
            CreateDocumentsResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": error,
            })
            .to_string(),
            CreateDocumentsResult::Cancelled => {
                serde_json::json!({ "status": "cancelled" }).to_string()
            }
        },
        AIAgentActionResultType::RequestComputerUse(result) => match result {
            RequestComputerUseResult::Approved {
                screenshot,
                platform,
            } => serde_json::json!({
                "status": "approved",
                "platform": format!("{platform:?}"),
                "screenshot": {
                    "width": screenshot.original_width,
                    "height": screenshot.original_height,
                },
                "instruction": "Computer use approved. Proceed with use_computer actions. Screenshot image bytes are not embedded; use dimensions for coordinate planning.",
            })
            .to_string(),
            RequestComputerUseResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": error,
            })
            .to_string(),
            RequestComputerUseResult::Cancelled => {
                serde_json::json!({ "status": "cancelled" }).to_string()
            }
        },
        AIAgentActionResultType::UseComputer(result) => match result {
            UseComputerResult::Success(action_result) => {
                let screenshot = action_result.screenshot.as_ref().map(|shot| {
                    serde_json::json!({
                        "width": shot.original_width,
                        "height": shot.original_height,
                    })
                });
                let cursor = action_result.cursor_position.map(|pos| {
                    serde_json::json!({ "x": pos.x(), "y": pos.y() })
                });
                serde_json::json!({
                    "status": "completed",
                    "screenshot": screenshot,
                    "cursor_position": cursor,
                    "instruction": "Computer actions finished. Screenshot pixels are not embedded for local models.",
                })
                .to_string()
            }
            UseComputerResult::Error(error) => serde_json::json!({
                "status": "error",
                "error": error,
            })
            .to_string(),
            UseComputerResult::Cancelled => serde_json::json!({ "status": "cancelled" }).to_string(),
        },
        _ => result.to_string(),
    }
}

fn document_contexts_to_json(
    status: &str,
    documents: &[crate::ai::agent::DocumentContext],
) -> String {
    let documents = documents
        .iter()
        .map(|doc| {
            serde_json::json!({
                "document_id": doc.document_id.to_string(),
                "document_version": doc.document_version.to_string(),
                "content": doc.content,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "status": status,
        "documents": documents,
    })
    .to_string()
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
                &[
                    "command",
                    "is_read_only",
                    "is_risky",
                    "uses_pager",
                    "wait_until_complete",
                ],
                &call.name,
            )?;
            let command = required_string(&call.arguments, "command", &call.name)?;
            let explicit_read_only = optional_bool(&call.arguments, "is_read_only")?;
            let is_read_only =
                explicit_read_only.unwrap_or_else(|| infer_shell_command_is_read_only(&command));
            // Default true for weak local models; false enables LRC poll/write tools.
            let wait_until_complete =
                optional_bool(&call.arguments, "wait_until_complete")?.unwrap_or(true);
            Ok(Tool::RunShellCommand(
                api::message::tool_call::RunShellCommand {
                    command,
                    is_read_only,
                    is_risky: optional_bool(&call.arguments, "is_risky")?.unwrap_or(false),
                    uses_pager: optional_bool(&call.arguments, "uses_pager")?.unwrap_or(false),
                    wait_until_complete_value: Some(
                        api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                            wait_until_complete,
                        ),
                    ),
                    ..Default::default()
                },
            ))
        }
        "read_shell_command_output" => Ok(Tool::ReadShellCommandOutput(
            read_shell_command_output_tool_call_to_proto(call)?,
        )),
        "write_to_long_running_shell_command" => Ok(Tool::WriteToLongRunningShellCommand(
            write_to_long_running_shell_command_tool_call_to_proto(call)?,
        )),
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
        "run_agents" => Ok(Tool::RunAgents(run_agents_tool_call_to_proto(call)?)),
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

fn read_shell_command_output_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::message::tool_call::ReadShellCommandOutput, ToolExecutionError> {
    let AIAgentActionType::ReadShellCommandOutput { block_id, delay } =
        read_shell_command_output_tool_call_to_ai_action(call)?
    else {
        unreachable!("read_shell_command_output conversion always returns that action");
    };
    let delay = match delay {
        Some(ShellCommandDelay::OnCompletion) => {
            Some(api::message::tool_call::read_shell_command_output::Delay::OnCompletion(()))
        }
        Some(ShellCommandDelay::Duration(duration)) => Some(
            api::message::tool_call::read_shell_command_output::Delay::Duration(
                prost_types::Duration {
                    seconds: duration.as_secs() as i64,
                    nanos: 0,
                },
            ),
        ),
        None => None,
    };
    Ok(api::message::tool_call::ReadShellCommandOutput {
        command_id: block_id.to_string(),
        delay,
    })
}

fn write_to_long_running_shell_command_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::message::tool_call::WriteToLongRunningShellCommand, ToolExecutionError> {
    use api::message::tool_call::write_to_long_running_shell_command::mode::Mode as ModeVariant;
    use api::message::tool_call::write_to_long_running_shell_command::Mode;

    let AIAgentActionType::WriteToLongRunningShellCommand {
        block_id,
        input,
        mode,
    } = write_to_long_running_shell_command_tool_call_to_ai_action(call)?
    else {
        unreachable!("write_to_long_running_shell_command conversion always returns that action");
    };
    let mode = match mode {
        AIAgentPtyWriteMode::Raw => ModeVariant::Raw(()),
        AIAgentPtyWriteMode::Line => ModeVariant::Line(()),
        AIAgentPtyWriteMode::Block => ModeVariant::Block(()),
    };
    Ok(api::message::tool_call::WriteToLongRunningShellCommand {
        input: input.to_vec(),
        mode: Some(Mode { mode: Some(mode) }),
        command_id: block_id.to_string(),
    })
}

fn run_agents_tool_call_to_proto(call: &ToolCall) -> Result<api::RunAgents, ToolExecutionError> {
    let AIAgentActionType::RunAgents(request) = run_agents_tool_call_to_ai_action(call)? else {
        unreachable!("run_agents conversion always returns RunAgents");
    };
    Ok(api::RunAgents {
        summary: request.summary,
        base_prompt: request.base_prompt,
        skills: Vec::new(),
        model_id: request.model_id,
        harness: None,
        agent_run_configs: request
            .agent_run_configs
            .into_iter()
            .map(|config| api::run_agents::AgentRunConfig {
                name: config.name,
                prompt: config.prompt,
                title: config.title,
            })
            .collect(),
        plan_id: request.plan_id,
        execution_mode: Some(api::run_agents::ExecutionMode::Local(
            api::run_agents::Local {},
        )),
    })
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
        LocalRuntimeToolRouteKind::ListSkills => Err(ToolExecutionError::InvalidInput {
            reason: "list_skills has no wire proto tool form; results stay in the runtime transcript envelope"
                .to_string(),
        }),
        LocalRuntimeToolRouteKind::LocalWeb => Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "{} has no wire proto tool form; results stay in the runtime transcript envelope",
                call.name
            ),
        }),
        LocalRuntimeToolRouteKind::LocalTodo => Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "{} has no wire proto tool form; results stay in the runtime transcript envelope",
                call.name
            ),
        }),
        LocalRuntimeToolRouteKind::LocalGit => Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "{} has no wire proto tool form; results stay in the runtime transcript envelope",
                call.name
            ),
        }),
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
        Tool::RunShellCommand(tool) => {
            let wait_until_complete = tool.wait_until_complete_value.is_none_or(
                |api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                    should_wait,
                )| should_wait,
            );
            (
                "run_shell_command".to_string(),
                serde_json::json!({
                    "command": tool.command,
                    "is_read_only": tool.is_read_only,
                    "is_risky": tool.is_risky,
                    "uses_pager": tool.uses_pager,
                    "wait_until_complete": wait_until_complete,
                }),
            )
        }
        Tool::ReadShellCommandOutput(tool) => {
            let wait_until_complete = matches!(
                tool.delay,
                Some(api::message::tool_call::read_shell_command_output::Delay::OnCompletion(_))
            );
            (
                "read_shell_command_output".to_string(),
                serde_json::json!({
                    "block_id": tool.command_id,
                    "wait_until_complete": wait_until_complete,
                }),
            )
        }
        Tool::WriteToLongRunningShellCommand(tool) => {
            use api::message::tool_call::write_to_long_running_shell_command::mode::Mode as ModeVariant;

            let mode = match tool.mode.as_ref().and_then(|mode| mode.mode.as_ref()) {
                Some(ModeVariant::Line(_)) => "line",
                Some(ModeVariant::Block(_)) => "block",
                Some(ModeVariant::Raw(_)) | None => "raw",
            };
            (
                "write_to_long_running_shell_command".to_string(),
                serde_json::json!({
                    "block_id": tool.command_id,
                    "input": String::from_utf8_lossy(&tool.input),
                    "mode": mode,
                }),
            )
        }
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
                    | LocalRuntimeToolRouteKind::ListSkills
                    | LocalRuntimeToolRouteKind::LocalWeb
                    | LocalRuntimeToolRouteKind::LocalTodo
                    | LocalRuntimeToolRouteKind::LocalGit
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
        Tool::RunAgents(tool) => {
            let agents = tool
                .agent_run_configs
                .iter()
                .map(|config| {
                    let mut agent = serde_json::json!({
                        "name": config.name,
                        "prompt": config.prompt,
                    });
                    if !config.title.is_empty() {
                        agent["title"] = Value::String(config.title.clone());
                    }
                    agent
                })
                .collect::<Vec<_>>();
            let mut arguments = serde_json::json!({
                "summary": tool.summary,
                "agents": agents,
            });
            if !tool.base_prompt.is_empty() {
                arguments["base_prompt"] = Value::String(tool.base_prompt.clone());
            }
            ("run_agents".to_string(), arguments)
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
        use warp_core::command::ExitCode;

        use crate::ai::agent::RequestCommandOutputResult;
        use crate::terminal::model::block::BlockId;

        let completed =
            AIAgentActionResultType::RequestCommandOutput(RequestCommandOutputResult::Completed {
                block_id: BlockId::from("blk_1".to_string()),
                command: r#"python3 -c "print(sum(range(1, 101)))""#.to_string(),
                output: "5050\n".to_string(),
                exit_code: ExitCode::from(0),
                start_ts: None,
                completed_ts: None,
            });
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
    fn run_agents_is_gated_by_orchestration_and_root_depth() {
        let mut params = RequestParams::new_for_test();
        params.orchestration_enabled = true;
        let registry = LocalRuntimeToolRegistry::from_request(&params);
        assert!(registry.contains_tool("run_agents"));

        params.parent_agent_id = Some("parent-1".to_string());
        let child_registry = LocalRuntimeToolRegistry::from_request(&params);
        assert!(!child_registry.contains_tool("run_agents"));

        let mut disabled = RequestParams::new_for_test();
        disabled.orchestration_enabled = false;
        assert!(!LocalRuntimeToolRegistry::from_request(&disabled).contains_tool("run_agents"));
    }

    #[test]
    fn run_agents_maps_to_local_warp_action_with_child_bounds() {
        let mut registry = LocalRuntimeToolRegistry::built_ins();
        registry.add_run_agents();
        let call = ToolCall {
            id: "call_orch_1".to_string(),
            name: "run_agents".to_string(),
            arguments: serde_json::json!({
                "summary": "Parallel research",
                "base_prompt": "Stay in-repo",
                "agents": [
                    { "name": "explorer", "prompt": "Find the runtime entrypoint", "title": "Explore" },
                    { "name": "tester", "prompt": "List focused tests" }
                ],
            }),
        };

        let action = tool_call_to_ai_action_with_registry(
            &call,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap();
        let AIAgentActionType::RunAgents(request) = action.action else {
            panic!("expected run_agents action");
        };
        assert_eq!(request.summary, "Parallel research");
        assert_eq!(request.base_prompt, "Stay in-repo");
        assert!(matches!(
            request.execution_mode,
            RunAgentsExecutionMode::Local
        ));
        assert_eq!(request.agent_run_configs.len(), 2);
        assert_eq!(request.agent_run_configs[0].name, "explorer");
        assert_eq!(request.agent_run_configs[0].title, "Explore");
        assert_eq!(request.agent_run_configs[1].prompt, "List focused tests");

        let mut too_many = call.clone();
        too_many.arguments = serde_json::json!({
            "summary": "Too many",
            "agents": (0..=LOCAL_RUN_AGENTS_MAX_CHILDREN)
                .map(|i| serde_json::json!({ "name": format!("a{i}"), "prompt": "p" }))
                .collect::<Vec<_>>(),
        });
        let err = tool_call_to_ai_action_with_registry(
            &too_many,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::InvalidInput { .. }));

        let proto = tool_call_to_proto_tool_with_registry(&call, &registry).unwrap();
        let restored = proto_tool_call_to_runtime_with_registry(
            &api::message::ToolCall {
                tool_call_id: call.id.clone(),
                tool: Some(proto),
            },
            &registry,
        )
        .unwrap();
        assert_eq!(restored.name, "run_agents");
        assert_eq!(restored.arguments["summary"], "Parallel research");
        assert_eq!(restored.arguments["agents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn skill_catalog_registers_list_and_read_tools() {
        use ai::skills::{SkillProvider, SkillScope};

        use crate::ai::skills::SkillDescriptor;

        let empty = LocalRuntimeToolRegistry::from_request(&RequestParams::new_for_test());
        assert!(!empty.contains_tool("list_skills"));
        assert!(!empty.contains_tool("read_skill"));

        let skills = vec![SkillDescriptor {
            reference: SkillReference::BundledSkillId("demo".to_string()),
            name: "demo-skill".to_string(),
            description: "A demo skill".to_string(),
            scope: SkillScope::Bundled,
            provider: SkillProvider::Warp,
            icon_override: None,
        }];
        let registry = LocalRuntimeToolRegistry::from_request_with_available_skills(
            &RequestParams::new_for_test(),
            &skills,
        );
        assert!(registry.contains_tool("list_skills"));
        assert!(registry.contains_tool("read_skill"));
        assert_eq!(registry.skill_catalog().len(), 1);
        let listed = registry.list_skills_json();
        assert!(listed.contains("demo-skill"));
        assert!(listed.contains("@warp-skill:demo"));
        assert!(listed.contains("read_skill"));

        let mut plan_params = RequestParams::new_for_test();
        plan_params.input = vec![AIAgentInput::UserQuery {
            query: "Plan".to_string(),
            context: std::sync::Arc::from([]),
            static_query_type: None,
            referenced_attachments: std::collections::HashMap::new(),
            user_query_mode: crate::ai::agent::UserQueryMode::Plan,
            running_command: None,
            intended_agent: None,
        }];
        let plan_registry =
            LocalRuntimeToolRegistry::from_request_with_available_skills(&plan_params, &skills);
        assert_eq!(
            plan_registry.permission_mode(),
            LocalRuntimePermissionMode::Plan
        );
        assert!(plan_registry.contains_tool("list_skills"));
        assert!(plan_registry.contains_tool("read_skill"));
        assert!(plan_registry.contains_tool("read_files"));
        assert!(!plan_registry.contains_tool("edit_files"));
    }

    #[test]
    fn todo_tools_are_always_available_and_kept_in_plan_mode() {
        let registry = LocalRuntimeToolRegistry::from_request(&RequestParams::new_for_test());
        assert!(registry.contains_tool("update_todos"));
        assert!(registry.contains_tool("mark_todos_completed"));
        assert_eq!(
            registry.safety_class("update_todos"),
            ToolSafetyClass::ReadOnly
        );

        let mut plan_params = RequestParams::new_for_test();
        plan_params.input = vec![AIAgentInput::UserQuery {
            query: "Plan".to_string(),
            context: std::sync::Arc::from([]),
            static_query_type: None,
            referenced_attachments: std::collections::HashMap::new(),
            user_query_mode: crate::ai::agent::UserQueryMode::Plan,
            running_command: None,
            intended_agent: None,
        }];
        let plan = LocalRuntimeToolRegistry::from_request(&plan_params);
        assert!(plan.contains_tool("update_todos"));
        assert!(plan.contains_tool("mark_todos_completed"));
        assert!(!plan.contains_tool("edit_files"));

        let call = ToolCall {
            id: "todo_call_1".to_string(),
            name: "update_todos".to_string(),
            arguments: serde_json::json!({
                "todos": [{"id": "1", "title": "Ship feat-020"}]
            }),
        };
        let err = tool_call_to_ai_action_with_registry(
            &call,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::ExecutionFailed(_)));
    }

    #[test]
    fn git_tools_are_always_available_and_kept_in_plan_mode() {
        let registry = LocalRuntimeToolRegistry::from_request(&RequestParams::new_for_test());
        assert!(registry.contains_tool("git_status"));
        assert!(registry.contains_tool("draft_commit_message_context"));
        assert!(registry.contains_tool("draft_pr_summary_context"));
        assert_eq!(
            registry.safety_class("git_status"),
            ToolSafetyClass::ReadOnly
        );
        assert_eq!(
            registry.safety_class("draft_commit_message_context"),
            ToolSafetyClass::ReadOnly
        );
        assert_eq!(
            registry.safety_class("draft_pr_summary_context"),
            ToolSafetyClass::ReadOnly
        );

        let mut plan_params = RequestParams::new_for_test();
        plan_params.input = vec![AIAgentInput::UserQuery {
            query: "Plan".to_string(),
            context: std::sync::Arc::from([]),
            static_query_type: None,
            referenced_attachments: std::collections::HashMap::new(),
            user_query_mode: crate::ai::agent::UserQueryMode::Plan,
            running_command: None,
            intended_agent: None,
        }];
        let plan = LocalRuntimeToolRegistry::from_request(&plan_params);
        assert!(plan.contains_tool("git_status"));
        assert!(plan.contains_tool("draft_commit_message_context"));
        assert!(plan.contains_tool("draft_pr_summary_context"));
        assert!(!plan.contains_tool("edit_files"));

        let call = ToolCall {
            id: "git_call_1".to_string(),
            name: "git_status".to_string(),
            arguments: serde_json::json!({}),
        };
        let err = tool_call_to_ai_action_with_registry(
            &call,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::ExecutionFailed(_)));
    }

    #[test]
    fn web_tools_are_gated_by_web_search_enabled_and_kept_in_plan_mode() {
        let off = LocalRuntimeToolRegistry::from_request(&RequestParams::new_for_test());
        assert!(!off.contains_tool("web_search"));
        assert!(!off.contains_tool("web_fetch"));

        let mut on = RequestParams::new_for_test();
        on.web_search_enabled = true;
        let registry = LocalRuntimeToolRegistry::from_request(&on);
        assert!(registry.contains_tool("web_search"));
        assert!(registry.contains_tool("web_fetch"));
        assert_eq!(
            registry.safety_class("web_search"),
            ToolSafetyClass::ReadOnly
        );
        assert_eq!(
            registry.safety_class("web_fetch"),
            ToolSafetyClass::ReadOnly
        );

        on.input = vec![AIAgentInput::UserQuery {
            query: "Plan research".to_string(),
            context: std::sync::Arc::from([]),
            static_query_type: None,
            referenced_attachments: std::collections::HashMap::new(),
            user_query_mode: crate::ai::agent::UserQueryMode::Plan,
            running_command: None,
            intended_agent: None,
        }];
        let plan = LocalRuntimeToolRegistry::from_request(&on);
        assert_eq!(plan.permission_mode(), LocalRuntimePermissionMode::Plan);
        assert!(plan.contains_tool("web_search"));
        assert!(plan.contains_tool("web_fetch"));
        assert!(plan.contains_tool("read_files"));
        assert!(!plan.contains_tool("edit_files"));

        let call = ToolCall {
            id: "call_web_1".to_string(),
            name: "web_search".to_string(),
            arguments: serde_json::json!({ "query": "rust async" }),
        };
        let err = tool_call_to_ai_action_with_registry(
            &call,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap_err();
        assert!(matches!(err, ToolExecutionError::ExecutionFailed(_)));
    }

    #[test]
    fn document_and_computer_use_tools_are_gated_and_map() {
        let base = LocalRuntimeToolRegistry::from_request(&RequestParams::new_for_test());
        assert!(base.contains_tool("read_documents"));
        assert!(base.contains_tool("edit_documents"));
        assert!(base.contains_tool("create_documents"));
        assert!(!base.contains_tool("request_computer_use"));
        assert!(!base.contains_tool("use_computer"));

        let mut cu_params = RequestParams::new_for_test();
        cu_params.computer_use_enabled = true;
        let with_cu = LocalRuntimeToolRegistry::from_request(&cu_params);
        assert!(with_cu.contains_tool("request_computer_use"));
        assert!(with_cu.contains_tool("use_computer"));

        let doc_id = uuid::Uuid::new_v4().to_string();
        let create = ToolCall {
            id: "call_doc".to_string(),
            name: "create_documents".to_string(),
            arguments: serde_json::json!({
                "documents": [{ "title": "Plan", "content": "# Hello" }]
            }),
        };
        let action = tool_call_to_ai_action_with_registry(
            &create,
            &TaskId::new("task_1".to_string()),
            &base,
        )
        .unwrap();
        match action.action {
            AIAgentActionType::CreateDocuments(CreateDocumentsRequest { documents }) => {
                assert_eq!(documents.len(), 1);
                assert_eq!(documents[0].title, "Plan");
                assert_eq!(documents[0].content, "# Hello");
            }
            other => panic!("expected create_documents, got {other:?}"),
        }

        let read = ToolCall {
            id: "call_read_doc".to_string(),
            name: "read_documents".to_string(),
            arguments: serde_json::json!({ "document_ids": [doc_id] }),
        };
        let action =
            tool_call_to_ai_action_with_registry(&read, &TaskId::new("task_1".to_string()), &base)
                .unwrap();
        assert!(matches!(
            action.action,
            AIAgentActionType::ReadDocuments(ReadDocumentsRequest { ref document_ids })
                if document_ids.len() == 1
        ));

        let req = ToolCall {
            id: "call_rcu".to_string(),
            name: "request_computer_use".to_string(),
            arguments: serde_json::json!({ "task_summary": "Open settings" }),
        };
        let action = tool_call_to_ai_action_with_registry(
            &req,
            &TaskId::new("task_1".to_string()),
            &with_cu,
        )
        .unwrap();
        match action.action {
            AIAgentActionType::RequestComputerUse(RequestComputerUseRequest {
                task_summary,
                ..
            }) => assert_eq!(task_summary, "Open settings"),
            other => panic!("expected request_computer_use, got {other:?}"),
        }

        let use_cu = ToolCall {
            id: "call_uc".to_string(),
            name: "use_computer".to_string(),
            arguments: serde_json::json!({
                "action_summary": "type hello",
                "actions": [{ "TypeText": { "text": "hello" } }],
                "take_screenshot": false
            }),
        };
        let action = tool_call_to_ai_action_with_registry(
            &use_cu,
            &TaskId::new("task_1".to_string()),
            &with_cu,
        )
        .unwrap();
        match action.action {
            AIAgentActionType::UseComputer(UseComputerRequest {
                action_summary,
                actions,
                screenshot_params,
            }) => {
                assert_eq!(action_summary, "type hello");
                assert_eq!(actions.len(), 1);
                assert!(screenshot_params.is_none());
                assert!(matches!(
                    &actions[0],
                    computer_use::Action::TypeText { text } if text == "hello"
                ));
            }
            other => panic!("expected use_computer, got {other:?}"),
        }

        let content = action_result_to_content(&AIAgentActionResultType::CreateDocuments(
            CreateDocumentsResult::Success {
                created_documents: vec![],
            },
        ));
        assert!(content.contains("\"status\":\"created\""));
    }

    #[test]
    fn background_shell_tools_map_and_round_trip() {
        let registry = LocalRuntimeToolRegistry::built_ins();
        assert!(registry.contains_tool("read_shell_command_output"));
        assert!(registry.contains_tool("write_to_long_running_shell_command"));
        assert_eq!(
            registry.safety_class("read_shell_command_output"),
            ToolSafetyClass::ReadOnly
        );
        assert_eq!(
            registry.safety_class("write_to_long_running_shell_command"),
            ToolSafetyClass::Interactive
        );

        let shell = ToolCall {
            id: "call_bg".to_string(),
            name: "run_shell_command".to_string(),
            arguments: serde_json::json!({
                "command": "npm run dev",
                "wait_until_complete": false,
            }),
        };
        let action = tool_call_to_ai_action_with_registry(
            &shell,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap();
        match action.action {
            AIAgentActionType::RequestCommandOutput {
                wait_until_completion: false,
                command,
                ..
            } => assert_eq!(command, "npm run dev"),
            other => panic!("expected background shell action, got {other:?}"),
        }

        let poll = ToolCall {
            id: "call_poll".to_string(),
            name: "read_shell_command_output".to_string(),
            arguments: serde_json::json!({
                "block_id": "blk_42",
                "wait_until_complete": true,
            }),
        };
        let action = tool_call_to_ai_action_with_registry(
            &poll,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap();
        match action.action {
            AIAgentActionType::ReadShellCommandOutput {
                block_id,
                delay: Some(ShellCommandDelay::OnCompletion),
            } => assert_eq!(block_id.to_string(), "blk_42"),
            other => panic!("expected read LRC action, got {other:?}"),
        }

        let write = ToolCall {
            id: "call_write".to_string(),
            name: "write_to_long_running_shell_command".to_string(),
            arguments: serde_json::json!({
                "command_id": "blk_42",
                "input": "help",
                "mode": "line",
            }),
        };
        let action = tool_call_to_ai_action_with_registry(
            &write,
            &TaskId::new("task_1".to_string()),
            &registry,
        )
        .unwrap();
        match action.action {
            AIAgentActionType::WriteToLongRunningShellCommand {
                block_id,
                input,
                mode: AIAgentPtyWriteMode::Line,
            } => {
                assert_eq!(block_id.to_string(), "blk_42");
                assert_eq!(input.as_ref(), b"help");
            }
            other => panic!("expected write LRC action, got {other:?}"),
        }

        let proto = tool_call_to_proto_tool_with_registry(&write, &registry).unwrap();
        let restored = proto_tool_call_to_runtime_with_registry(
            &api::message::ToolCall {
                tool_call_id: write.id.clone(),
                tool: Some(proto),
            },
            &registry,
        )
        .unwrap();
        assert_eq!(restored.name, "write_to_long_running_shell_command");
        assert_eq!(restored.arguments["block_id"], "blk_42");
        assert_eq!(restored.arguments["mode"], "line");

        use warp_core::command::ExitCode;

        let finished = AIAgentActionResultType::ReadShellCommandOutput(
            ReadShellCommandOutputResult::CommandFinished {
                block_id: BlockId::from("blk_42".to_string()),
                command: "npm run dev".to_string(),
                output: "ready\n".to_string(),
                exit_code: ExitCode::from(0),
                start_ts: None,
                completed_ts: None,
            },
        );
        let content = action_result_to_content(&finished);
        assert!(content.contains("\"status\":\"completed\""));
        assert!(content.contains("ready"));
        assert!(matches!(
            action_result_to_proto_tool_call_result_type(&finished),
            Some(api::message::tool_call_result::Result::ReadShellCommandOutput(_))
        ));
    }

    #[test]
    fn run_agents_result_content_and_proto_cover_launch_and_cancel() {
        use crate::ai::agent::RunAgentsAgentOutcome;

        let launched = AIAgentActionResultType::RunAgents(RunAgentsResult::Launched {
            model_id: "local-model".to_string(),
            harness_type: "oz".to_string(),
            execution_mode: RunAgentsLaunchedExecutionMode::Local,
            agents: vec![
                RunAgentsAgentOutcome {
                    name: "explorer".to_string(),
                    kind: RunAgentsAgentOutcomeKind::Launched {
                        agent_id: "agent-1".to_string(),
                    },
                },
                RunAgentsAgentOutcome {
                    name: "tester".to_string(),
                    kind: RunAgentsAgentOutcomeKind::Failed {
                        error: "spawn failed".to_string(),
                    },
                },
            ],
        });
        let content = action_result_to_content(&launched);
        assert!(content.contains("\"status\":\"launched\""));
        assert!(content.contains("agent-1"));
        assert!(content.contains("spawn failed"));

        assert!(matches!(
            action_result_to_proto_tool_call_result_type(&launched),
            Some(api::message::tool_call_result::Result::RunAgentsResult(_))
        ));

        let cancelled = AIAgentActionResultType::RunAgents(RunAgentsResult::Cancelled);
        assert_eq!(
            action_result_to_content(&cancelled),
            r#"{"status":"cancelled"}"#
        );
        // Cancelled maps to generic cancel marker, not a typed RunAgentsResult.
        assert!(action_result_to_proto_tool_call_result_type(&cancelled).is_none());
    }

    #[test]
    fn request_permission_modes_filter_plan_tools_without_weakening_accept_edits() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut plan_params = RequestParams::new_for_test();
        plan_params.ask_user_question_enabled = true;
        plan_params.orchestration_enabled = true;
        plan_params.computer_use_enabled = true;
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
        assert!(plan_registry.contains_tool("read_shell_command_output"));
        assert!(plan_registry.contains_tool("read_documents"));
        assert!(!plan_registry.contains_tool("run_shell_command"));
        assert!(!plan_registry.contains_tool("write_to_long_running_shell_command"));
        assert!(!plan_registry.contains_tool("edit_files"));
        assert!(!plan_registry.contains_tool("run_agents"));
        assert!(!plan_registry.contains_tool("edit_documents"));
        assert!(!plan_registry.contains_tool("create_documents"));
        assert!(!plan_registry.contains_tool("use_computer"));
        assert!(!plan_registry.contains_tool("request_computer_use"));

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
                "read_shell_command_output",
                "write_to_long_running_shell_command",
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
                "read_shell_command_output",
                "write_to_long_running_shell_command",
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
        encode_local_runtime_tool_call_data, encode_local_runtime_tool_result_data,
        tool_call_to_proto_tool_with_registry, LocalRuntimeToolRegistry,
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
                RuntimeEvent::ToolResult { call_id, result } => {
                    // In-process tools (todos, and any future local-only tools) register
                    // persistence when they execute. Warp-action tools persist via the
                    // controller and leave no pending entry.
                    let Some(persistence) = self.registry.take_local_tool_persistence(call_id)
                    else {
                        return vec![];
                    };

                    let mut actions = Vec::new();
                    actions.push(begin_transaction());
                    if !self.task_created {
                        actions.push(create_task(&self.task_id));
                        self.task_created = true;
                    }

                    if let Some(update) = persistence.todo_update {
                        actions.push(api::ClientAction {
                            action: Some(api::client_action::Action::AddMessagesToTask(
                                api::client_action::AddMessagesToTask {
                                    task_id: self.task_id.clone(),
                                    messages: vec![api::Message {
                                        id: Uuid::new_v4().to_string(),
                                        task_id: self.task_id.clone(),
                                        request_id: self.request_id.clone(),
                                        timestamp: None,
                                        server_message_data: String::new(),
                                        citations: vec![],
                                        fetched_memories: vec![],
                                        message: Some(api::message::Message::UpdateTodos(update)),
                                    }],
                                },
                            )),
                        });
                    }

                    actions.push(api::ClientAction {
                        action: Some(api::client_action::Action::AddMessagesToTask(
                            api::client_action::AddMessagesToTask {
                                task_id: self.task_id.clone(),
                                messages: vec![api::Message {
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
                            },
                        )),
                    });
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
                // Surface recoverable runtime warnings (model/tool issues) as agent text so
                // the conversation isn't stuck on "Warping..." with no detail.
                RuntimeEvent::Warning { message } => {
                    if message.is_empty() {
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
                        &format!("**Runtime warning:** {message}"),
                    ));
                    actions.push(commit_transaction());
                    vec![api::ResponseEvent {
                        r#type: Some(api::response_event::Type::ClientActions(
                            api::response_event::ClientActions { actions },
                        )),
                    }]
                }
                // PermissionRequired / ToolExecutionStarted for Warp-action tools are handled by
                // the runtime + controller (action cards). Local-only tools surface via ToolCall /
                // ToolResult transcript messages instead.
                _ => vec![],
            }
        }
    }

    /// Convert a runtime ToolCall to a proto ClientAction (AddMessagesToTask with ToolCall message).
    ///
    /// Local todo tools have no wire proto tool form; we still persist a ToolCall message with
    /// the runtime transcript envelope so pairs restore. Other local-only tools (web, list_skills)
    /// keep results in the runtime loop only unless they gain similar persistence later.
    fn tool_call_to_proto_action(
        task_id: &str,
        request_id: &str,
        call: &local_agent_runtime::ToolCall,
        registry: &LocalRuntimeToolRegistry,
    ) -> Option<api::ClientAction> {
        if !registry.contains_tool(&call.name) {
            return None;
        }
        let proto_tool = tool_call_to_proto_tool_with_registry(call, registry).ok();
        if proto_tool.is_none()
            && call.name != "update_todos"
            && call.name != "mark_todos_completed"
        {
            return None;
        }

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
                tool: proto_tool,
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
