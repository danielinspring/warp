//! Request-scoped tool registry: which tools the model may call this turn, how each one is
//! routed (client-executed or in-process) and the permission mode derived from the request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use local_agent_runtime::ToolSafetyClass;
use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use uuid::Uuid;
use warp_multi_agent_api as api;

use crate::todos::LocalTodoState;
use crate::tool_proto::{
    SkillRef, ask_user_question_schema, build_tool_schemas, create_documents_schema,
    edit_documents_schema, prost_struct_to_json, read_documents_schema,
    request_computer_use_schema, run_agents_schema, sanitize_function_name, unique_tool_name,
    use_computer_schema,
};
use crate::{git, todos, web};

/// Compact skill entry for list_skills and system-prompt discovery.
#[derive(Debug, Clone)]
pub struct LocalRuntimeSkillInfo {
    pub name: String,
    pub description: String,
    pub reference: String,
    pub scope: String,
}

/// Persistence payload registered during in-process tool execution and consumed when mapping
/// the runtime's tool result into Warp task messages.
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
    todo_state: Arc<Mutex<LocalTodoState>>,
    /// call_id → side effects to emit when the runtime reports ToolResult.
    pending_local_persistence: Arc<Mutex<HashMap<String, LocalToolPersistence>>>,
    /// Session working directory, used to resolve local git tool calls that omit `repo_path`.
    working_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRuntimePermissionMode {
    Default,
    AcceptEdits,
    Plan,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalRuntimeToolRoute {
    pub(crate) safety_class: ToolSafetyClass,
    pub(crate) kind: LocalRuntimeToolRouteKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LocalRuntimeToolRouteKind {
    BuiltIn,
    McpTool {
        server_id: Option<Uuid>,
        name: String,
    },
    ReadMcpResource,
    ReadSkill {
        skill_lookup: HashMap<String, SkillRef>,
    },
    /// Catalog listing answered in-process from `skill_catalog`.
    ListSkills,
    /// Local HTTP web_search / web_fetch (in-process).
    LocalWeb,
    /// Local durable todos (in-process + UpdateTodos task messages).
    LocalTodo,
    /// Local read-only git workflow tools (in-process).
    LocalGit,
}

impl LocalRuntimeToolRouteKind {
    fn is_in_process(&self) -> bool {
        match self {
            Self::ListSkills | Self::LocalWeb | Self::LocalTodo | Self::LocalGit => true,
            Self::BuiltIn
            | Self::McpTool { .. }
            | Self::ReadMcpResource
            | Self::ReadSkill { .. } => false,
        }
    }
}

impl LocalRuntimeToolRegistry {
    /// Build the registry for one request from the capabilities and context the client sent.
    pub fn from_request(request: &api::Request) -> Self {
        let mut registry = Self::built_ins();
        registry.permission_mode = LocalRuntimePermissionMode::from_request(request);

        let context = request
            .input
            .as_ref()
            .and_then(|input| input.context.as_ref());
        registry.working_directory = context
            .and_then(|context| context.directory.as_ref())
            .map(|directory| directory.pwd.as_str())
            .filter(|pwd| !pwd.is_empty())
            .map(PathBuf::from);

        if let Some(mcp_context) = &request.mcp_context {
            registry.add_mcp_context(mcp_context);
        }

        let mut skills = HashMap::new();
        let mut catalog = Vec::new();
        let available_skills = context
            .and_then(|context| context.updated_skills_context.as_ref())
            .map(|skills| skills.available_skills.as_slice())
            .unwrap_or_default();
        for skill in available_skills {
            let Some(reference) = skill
                .skill_reference
                .as_ref()
                .map(SkillRef::from_descriptor)
            else {
                continue;
            };
            let reference_string = reference.reference_string();
            skills.insert(skill.name.clone(), reference.clone());
            skills.insert(reference_string.clone(), reference);
            catalog.push(LocalRuntimeSkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
                reference: reference_string,
                scope: skill_scope_label(skill.scope.as_ref()),
            });
        }
        if !skills.is_empty() {
            registry.skill_catalog = catalog;
            registry.add_list_skills();
            registry.add_read_skill(skills);
        }

        let settings = request.settings.as_ref();
        let supported_tools = settings
            .map(|settings| settings.supported_tools.as_slice())
            .unwrap_or_default();
        let supports = |tool: api::ToolType| supported_tools.contains(&(tool as i32));
        if supports(api::ToolType::AskUserQuestion) {
            registry.add_ask_user_question();
        }
        // Depth bound: only root agents may advertise run_agents so children cannot recursively
        // fan out further local orchestration.
        let is_root_agent = request
            .metadata
            .as_ref()
            .is_none_or(|metadata| metadata.parent_agent_id.is_empty());
        if supports(api::ToolType::RunAgents) && is_root_agent {
            registry.add_run_agents();
        }
        if supports(api::ToolType::ReadDocuments) {
            registry.add_tool(
                read_documents_schema(),
                ToolSafetyClass::ReadOnly,
                LocalRuntimeToolRouteKind::BuiltIn,
            );
        }
        if supports(api::ToolType::EditDocuments) {
            registry.add_tool(
                edit_documents_schema(),
                ToolSafetyClass::Interactive,
                LocalRuntimeToolRouteKind::BuiltIn,
            );
        }
        if supports(api::ToolType::CreateDocuments) {
            registry.add_tool(
                create_documents_schema(),
                ToolSafetyClass::Interactive,
                LocalRuntimeToolRouteKind::BuiltIn,
            );
        }
        if supports(api::ToolType::RequestComputerUse) || supports(api::ToolType::UseComputer) {
            registry.add_computer_use_tools();
        }
        if settings.is_some_and(|settings| settings.web_search_enabled) {
            registry.add_web_tools();
        }
        registry.add_todo_tools();
        registry.add_git_tools();
        if let Some(task_context) = &request.task_context {
            registry.with_todos(|todo_state| todo_state.hydrate_from_tasks(&task_context.tasks));
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
            todo_state: Arc::new(Mutex::new(LocalTodoState::default())),
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

    /// Whether the service executes this tool itself instead of deferring it to the client.
    pub fn is_in_process(&self, name: &str) -> bool {
        self.routes
            .get(name)
            .is_some_and(|route| route.kind.is_in_process())
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

    pub fn working_directory(&self) -> Option<&Path> {
        self.working_directory.as_deref()
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

    pub(crate) fn route(&self, name: &str) -> Option<&LocalRuntimeToolRoute> {
        self.routes.get(name)
    }

    /// The advertised function name for an MCP tool restored from a persisted proto call.
    pub(crate) fn mcp_function_name(&self, server_id: &str, tool_name: &str) -> Option<String> {
        self.routes
            .iter()
            .find_map(|(function_name, route)| match &route.kind {
                LocalRuntimeToolRouteKind::McpTool {
                    server_id: id,
                    name,
                } if name == tool_name
                    && id.map(|id| id.to_string()).unwrap_or_default() == server_id =>
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
    }

    pub(crate) fn add_tool(
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

    fn add_mcp_context(&mut self, context: &api::request::McpContext) {
        let mut has_resources = false;

        for server in &context.servers {
            let server_slug = sanitize_function_name(&server.name);
            let server_id = Uuid::parse_str(&server.id).ok();
            has_resources |= !server.resources.is_empty();

            for tool in &server.tools {
                let function_name = unique_tool_name(
                    &self.routes,
                    &format!("mcp__{server_slug}__{}", sanitize_function_name(&tool.name)),
                );
                let description = if tool.description.is_empty() {
                    format!("Call MCP tool `{}` on server `{}`", tool.name, server.name)
                } else {
                    tool.description.clone()
                };
                self.add_tool(
                    ToolSchema {
                        name: function_name,
                        description,
                        parameters: mcp_parameters(tool.input_schema.as_ref()),
                    },
                    ToolSafetyClass::Interactive,
                    LocalRuntimeToolRouteKind::McpTool {
                        server_id,
                        name: tool.name.clone(),
                    },
                );
            }
        }

        #[allow(deprecated)]
        {
            has_resources |= !context.resources.is_empty();
            for tool in &context.tools {
                let function_name = unique_tool_name(
                    &self.routes,
                    &format!("mcp__default__{}", sanitize_function_name(&tool.name)),
                );
                let description = if tool.description.is_empty() {
                    format!("Call MCP tool `{}`", tool.name)
                } else {
                    tool.description.clone()
                };
                self.add_tool(
                    ToolSchema {
                        name: function_name,
                        description,
                        parameters: mcp_parameters(tool.input_schema.as_ref()),
                    },
                    ToolSafetyClass::Interactive,
                    LocalRuntimeToolRouteKind::McpTool {
                        server_id: None,
                        name: tool.name.clone(),
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
            web::web_search_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalWeb,
        );
        self.add_tool(
            web::web_fetch_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalWeb,
        );
    }

    fn add_todo_tools(&mut self) {
        self.add_tool(
            todos::update_todos_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalTodo,
        );
        self.add_tool(
            todos::mark_todos_completed_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalTodo,
        );
    }

    fn add_git_tools(&mut self) {
        self.add_tool(
            git::git_status_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalGit,
        );
        self.add_tool(
            git::draft_commit_message_context_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalGit,
        );
        self.add_tool(
            git::draft_pr_summary_context_schema(),
            ToolSafetyClass::ReadOnly,
            LocalRuntimeToolRouteKind::LocalGit,
        );
    }

    pub fn todo_prompt_section(&self) -> Option<String> {
        self.with_todos(|todo_state| todo_state.prompt_section())
    }

    /// Run `f` against the session todo state under its lock.
    pub(crate) fn with_todos<R>(&self, f: impl FnOnce(&mut LocalTodoState) -> R) -> R {
        let mut todo_state = self.todo_state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut todo_state)
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

    pub(crate) fn add_read_skill(&mut self, skill_lookup: HashMap<String, SkillRef>) {
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

fn mcp_parameters(input_schema: Option<&prost_types::Struct>) -> serde_json::Value {
    input_schema
        .map(prost_struct_to_json)
        .unwrap_or_else(|| serde_json::json!({ "type": "object" }))
}

fn skill_scope_label(scope: Option<&api::skill_descriptor::Scope>) -> String {
    match scope.and_then(|scope| scope.r#type.as_ref()) {
        Some(api::skill_descriptor::scope::Type::Home(())) => "Home",
        Some(api::skill_descriptor::scope::Type::Project(())) => "Project",
        Some(api::skill_descriptor::scope::Type::Bundled(())) => "Bundled",
        None => "Unknown",
    }
    .to_string()
}

impl LocalRuntimePermissionMode {
    /// Plan mode follows the most recent user query (this turn's inputs first, then history);
    /// otherwise unsupervised autonomy maps to AcceptEdits.
    fn from_request(request: &api::Request) -> Self {
        if plan_mode_requested(request) {
            Self::Plan
        } else if request.settings.as_ref().is_some_and(|settings| {
            settings.autonomy_level == api::AutonomyLevel::Unsupervised as i32
        }) {
            Self::AcceptEdits
        } else {
            Self::Default
        }
    }
}

fn plan_mode_requested(request: &api::Request) -> bool {
    let this_turn = request
        .input
        .as_ref()
        .and_then(|input| input.r#type.as_ref())
        .and_then(|kind| {
            let api::request::input::Type::UserInputs(user_inputs) = kind else {
                return None;
            };
            user_inputs.inputs.iter().rev().find_map(|input| {
                if let Some(api::request::input::user_inputs::user_input::Input::UserQuery(query)) =
                    &input.input
                {
                    Some(is_plan_mode(query.mode.as_ref()))
                } else {
                    None
                }
            })
        });
    let from_history = || {
        request
            .task_context
            .as_ref()?
            .tasks
            .iter()
            .rev()
            .find_map(|task| {
                task.messages.iter().rev().find_map(|message| {
                    if let Some(api::message::Message::UserQuery(query)) = &message.message {
                        Some(is_plan_mode(query.mode.as_ref()))
                    } else {
                        None
                    }
                })
            })
    };
    this_turn.or_else(from_history).unwrap_or(false)
}

fn is_plan_mode(mode: Option<&api::UserQueryMode>) -> bool {
    matches!(
        mode.and_then(|mode| mode.r#type.as_ref()),
        Some(api::user_query_mode::Type::Plan(()))
    )
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
