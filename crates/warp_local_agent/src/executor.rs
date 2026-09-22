//! The runtime's tool executor for the service: skill catalog, web, git and todo tools run here;
//! every other tool is deferred to the Warp client.

use std::sync::Arc;

use local_agent_runtime::tools::schema::ToolSchema;
use local_agent_runtime::{
    ExecutionSite, PermissionDecision, ToolCall, ToolCallResult, ToolExecutionError, ToolExecutor,
    ToolSafetyClass,
};

use crate::model_packs::{ModelFamily, apply_schema_tweaks};
use crate::registry::{LocalRuntimePermissionMode, LocalRuntimeToolRegistry, LocalToolPersistence};
use crate::todos::LocalTodoSideEffect;
use crate::tool_proto::shell_command_is_read_only;
use crate::{git, todos, web};

pub struct ServiceToolExecutor {
    registry: Arc<LocalRuntimeToolRegistry>,
    model_family: ModelFamily,
}

impl ServiceToolExecutor {
    pub fn new(registry: Arc<LocalRuntimeToolRegistry>, model_family: ModelFamily) -> Self {
        Self {
            registry,
            model_family,
        }
    }

    fn ensure_registered(&self, call: &ToolCall) -> Result<(), ToolExecutionError> {
        if self.registry.contains_tool(&call.name) {
            Ok(())
        } else {
            Err(ToolExecutionError::NotFound {
                name: call.name.clone(),
            })
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ServiceToolExecutor {
    fn available_tools(&self) -> Vec<ToolSchema> {
        apply_schema_tweaks(self.model_family, self.registry.schemas())
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

    fn execution_site(&self, call: &ToolCall) -> ExecutionSite {
        if self.registry.is_in_process(&call.name) {
            ExecutionSite::InProcess
        } else {
            ExecutionSite::Client
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
            // The Warp client owns the real permission decision for the tools it executes;
            // AcceptEdits and Default only differ in the client's UI behaviour.
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny {
                reason: format!("Unsupported local agent tool: {}", call.name),
            }
        }
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
        match call.name.as_str() {
            "list_skills" => {
                self.ensure_registered(call)?;
                Ok(ToolCallResult::success(self.registry.list_skills_json()))
            }
            "web_search" | "web_fetch" => {
                self.ensure_registered(call)?;
                web::execute_web_tool(call).await
            }
            "git_status" | "draft_commit_message_context" | "draft_pr_summary_context" => {
                self.ensure_registered(call)?;
                git::execute_git_tool(call, self.registry.working_directory()).await
            }
            "update_todos" | "mark_todos_completed" => {
                self.ensure_registered(call)?;
                let (result, side_effect) = self
                    .registry
                    .with_todos(|todo_state| todos::execute_todo_tool(call, todo_state))?;
                let todo_update =
                    side_effect.map(|LocalTodoSideEffect::UpdateTodos(update)| update);
                // Register even without an update so the result message is still persisted.
                self.registry.register_local_tool_persistence(
                    call.id.clone(),
                    LocalToolPersistence { todo_update },
                );
                Ok(result)
            }
            _ => Err(ToolExecutionError::ExecutionFailed(anyhow::anyhow!(
                "client tool `{}` reached the in-process executor",
                call.name
            ))),
        }
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
