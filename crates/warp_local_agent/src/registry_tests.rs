use local_agent_runtime::ToolCall;

use super::*;
use crate::test_support::{
    plan_mode, query_request, request, task, tool_call_result, user_query, user_query_message,
};
use crate::tool_proto::tool_call_to_proto_tool_with_registry;

fn with_supported_tools(mut request: api::Request, tools: &[api::ToolType]) -> api::Request {
    request
        .settings
        .get_or_insert_with(Default::default)
        .supported_tools = tools.iter().map(|tool| *tool as i32).collect();
    request
}

fn plan_request(query: &str) -> api::Request {
    request(vec![user_query(query, Some(plan_mode()))])
}

fn skill_context(name: &str, id: &str) -> api::input_context::SkillsContext {
    api::input_context::SkillsContext {
        available_skills: vec![api::SkillDescriptor {
            name: name.to_string(),
            description: "A demo skill".to_string(),
            provider: None,
            scope: Some(api::skill_descriptor::Scope {
                r#type: Some(api::skill_descriptor::scope::Type::Bundled(())),
            }),
            skill_reference: Some(api::skill_descriptor::SkillReference::BundledSkillId(
                id.to_string(),
            )),
        }],
    }
}

fn with_skills(
    mut request: api::Request,
    skills: api::input_context::SkillsContext,
) -> api::Request {
    request
        .input
        .get_or_insert_with(Default::default)
        .context
        .get_or_insert_with(Default::default)
        .updated_skills_context = Some(skills);
    request
}

#[test]
fn run_agents_is_gated_by_supported_tools_and_root_depth() {
    let root = with_supported_tools(query_request("go"), &[api::ToolType::RunAgents]);
    assert!(LocalRuntimeToolRegistry::from_request(&root).contains_tool("run_agents"));

    let mut child = root.clone();
    child
        .metadata
        .get_or_insert_with(Default::default)
        .parent_agent_id = "parent-1".to_string();
    assert!(!LocalRuntimeToolRegistry::from_request(&child).contains_tool("run_agents"));

    assert!(
        !LocalRuntimeToolRegistry::from_request(&query_request("go")).contains_tool("run_agents")
    );
}

#[test]
fn skill_catalog_registers_list_and_read_tools() {
    let empty = LocalRuntimeToolRegistry::from_request(&query_request("go"));
    assert!(!empty.contains_tool("list_skills"));
    assert!(!empty.contains_tool("read_skill"));

    let registry = LocalRuntimeToolRegistry::from_request(&with_skills(
        query_request("go"),
        skill_context("demo-skill", "demo"),
    ));
    assert!(registry.contains_tool("list_skills"));
    assert!(registry.contains_tool("read_skill"));
    assert!(registry.is_in_process("list_skills"));
    assert!(!registry.is_in_process("read_skill"));
    assert_eq!(registry.skill_catalog().len(), 1);
    assert_eq!(registry.skill_catalog()[0].scope, "Bundled");
    let listed = registry.list_skills_json();
    assert!(listed.contains("demo-skill"));
    assert!(listed.contains("@warp-skill:demo"));
    assert!(listed.contains("read_skill"));

    let plan_registry = LocalRuntimeToolRegistry::from_request(&with_skills(
        plan_request("Plan"),
        skill_context("demo-skill", "demo"),
    ));
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
    let registry = LocalRuntimeToolRegistry::from_request(&query_request("go"));
    assert!(registry.contains_tool("update_todos"));
    assert!(registry.contains_tool("mark_todos_completed"));
    assert!(registry.is_in_process("update_todos"));
    assert_eq!(
        registry.safety_class("update_todos"),
        ToolSafetyClass::ReadOnly
    );

    let plan = LocalRuntimeToolRegistry::from_request(&plan_request("Plan"));
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
    let err = tool_call_to_proto_tool_with_registry(&call, &registry).unwrap_err();
    assert!(matches!(
        err,
        local_agent_runtime::ToolExecutionError::InvalidInput { .. }
    ));
}

#[test]
fn git_tools_are_always_available_and_kept_in_plan_mode() {
    let registry = LocalRuntimeToolRegistry::from_request(&query_request("go"));
    for name in [
        "git_status",
        "draft_commit_message_context",
        "draft_pr_summary_context",
    ] {
        assert!(registry.contains_tool(name), "{name} missing");
        assert!(registry.is_in_process(name), "{name} should run in-process");
        assert_eq!(registry.safety_class(name), ToolSafetyClass::ReadOnly);
    }

    let plan = LocalRuntimeToolRegistry::from_request(&plan_request("Plan"));
    assert!(plan.contains_tool("git_status"));
    assert!(plan.contains_tool("draft_commit_message_context"));
    assert!(plan.contains_tool("draft_pr_summary_context"));
    assert!(!plan.contains_tool("edit_files"));
}

#[test]
fn web_tools_are_gated_by_web_search_enabled_and_kept_in_plan_mode() {
    let off = LocalRuntimeToolRegistry::from_request(&query_request("go"));
    assert!(!off.contains_tool("web_search"));
    assert!(!off.contains_tool("web_fetch"));

    let mut on = query_request("go");
    on.settings
        .get_or_insert_with(Default::default)
        .web_search_enabled = true;
    let registry = LocalRuntimeToolRegistry::from_request(&on);
    assert!(registry.contains_tool("web_search"));
    assert!(registry.contains_tool("web_fetch"));
    assert!(registry.is_in_process("web_search"));
    assert_eq!(
        registry.safety_class("web_search"),
        ToolSafetyClass::ReadOnly
    );

    let mut plan = plan_request("Plan research");
    plan.settings
        .get_or_insert_with(Default::default)
        .web_search_enabled = true;
    let plan = LocalRuntimeToolRegistry::from_request(&plan);
    assert_eq!(plan.permission_mode(), LocalRuntimePermissionMode::Plan);
    assert!(plan.contains_tool("web_search"));
    assert!(plan.contains_tool("web_fetch"));
    assert!(plan.contains_tool("read_files"));
    assert!(!plan.contains_tool("edit_files"));
}

#[test]
fn document_and_computer_use_tools_are_gated_by_supported_tools() {
    let none = LocalRuntimeToolRegistry::from_request(&query_request("go"));
    for name in [
        "read_documents",
        "edit_documents",
        "create_documents",
        "request_computer_use",
        "use_computer",
        "ask_user_question",
    ] {
        assert!(!none.contains_tool(name), "{name} should be gated");
    }

    let all = LocalRuntimeToolRegistry::from_request(&with_supported_tools(
        query_request("go"),
        &[
            api::ToolType::ReadDocuments,
            api::ToolType::EditDocuments,
            api::ToolType::CreateDocuments,
            api::ToolType::UseComputer,
            api::ToolType::AskUserQuestion,
        ],
    ));
    assert_eq!(
        all.safety_class("read_documents"),
        ToolSafetyClass::ReadOnly
    );
    assert_eq!(
        all.safety_class("edit_documents"),
        ToolSafetyClass::Interactive
    );
    assert!(all.contains_tool("create_documents"));
    assert!(all.contains_tool("request_computer_use"));
    assert!(all.contains_tool("use_computer"));
    assert!(all.contains_tool("ask_user_question"));
    assert!(!all.is_in_process("use_computer"));
}

#[test]
fn request_permission_modes_filter_plan_tools_without_weakening_accept_edits() {
    let plan_registry = LocalRuntimeToolRegistry::from_request(&with_supported_tools(
        plan_request("Plan this"),
        &[
            api::ToolType::AskUserQuestion,
            api::ToolType::RunAgents,
            api::ToolType::UseComputer,
            api::ToolType::RequestComputerUse,
            api::ToolType::ReadDocuments,
            api::ToolType::EditDocuments,
            api::ToolType::CreateDocuments,
        ],
    ));
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

    let mut accept = query_request("go");
    accept
        .settings
        .get_or_insert_with(Default::default)
        .autonomy_level = api::AutonomyLevel::Unsupervised as i32;
    let accept_registry = LocalRuntimeToolRegistry::from_request(&accept);
    assert_eq!(
        accept_registry.permission_mode(),
        LocalRuntimePermissionMode::AcceptEdits
    );
    assert!(accept_registry.contains_tool("edit_files"));
    assert!(accept_registry.contains_tool("run_shell_command"));
}

#[test]
fn plan_mode_is_recovered_from_history_on_tool_result_continuation() {
    let mut continuation = request(vec![tool_call_result("call_1", None)]);
    continuation.task_context = Some(api::request::TaskContext {
        tasks: vec![task(
            "task_1",
            vec![user_query_message("task_1", "Plan it", Some(plan_mode()))],
        )],
    });
    let registry = LocalRuntimeToolRegistry::from_request(&continuation);
    assert_eq!(registry.permission_mode(), LocalRuntimePermissionMode::Plan);

    // A newer non-plan query in this turn wins over plan history.
    let mut resumed = continuation.clone();
    resumed.input = Some(crate::test_support::input(vec![user_query(
        "Now do it",
        None,
    )]));
    assert_eq!(
        LocalRuntimeToolRegistry::from_request(&resumed).permission_mode(),
        LocalRuntimePermissionMode::Default
    );
}

#[test]
fn working_directory_comes_from_input_context() {
    assert!(
        LocalRuntimeToolRegistry::from_request(&query_request("go"))
            .working_directory()
            .is_none()
    );

    let mut with_pwd = query_request("go");
    with_pwd
        .input
        .as_mut()
        .unwrap()
        .context
        .get_or_insert_with(Default::default)
        .directory = Some(api::input_context::Directory {
        pwd: "/tmp/project".to_string(),
        ..Default::default()
    });
    assert_eq!(
        LocalRuntimeToolRegistry::from_request(&with_pwd)
            .working_directory()
            .map(|path| path.to_string_lossy().into_owned()),
        Some("/tmp/project".to_string())
    );
}

#[test]
fn mcp_context_registers_namespaced_tools_and_resource_reader() {
    let server_id = Uuid::new_v4();
    let mut with_mcp = query_request("go");
    with_mcp.mcp_context = Some(api::request::McpContext {
        servers: vec![api::request::mcp_context::McpServer {
            name: "GitHub".to_string(),
            description: String::new(),
            id: server_id.to_string(),
            resources: vec![api::request::mcp_context::McpResource {
                uri: "github://readme".to_string(),
                name: "readme".to_string(),
                description: String::new(),
                mime_type: String::new(),
            }],
            tools: vec![api::request::mcp_context::McpTool {
                name: "search_issues".to_string(),
                description: String::new(),
                input_schema: None,
            }],
        }],
        ..Default::default()
    });
    let registry = LocalRuntimeToolRegistry::from_request(&with_mcp);

    assert!(registry.contains_tool("mcp__github__search_issues"));
    assert!(!registry.is_in_process("mcp__github__search_issues"));
    assert!(registry.contains_tool("read_mcp_resource"));
    assert_eq!(
        registry.mcp_function_name(&server_id.to_string(), "search_issues"),
        Some("mcp__github__search_issues".to_string())
    );
    let schema = registry
        .schemas()
        .into_iter()
        .find(|schema| schema.name == "mcp__github__search_issues")
        .unwrap();
    assert_eq!(
        schema.description,
        "Call MCP tool `search_issues` on server `GitHub`"
    );
    assert_eq!(schema.parameters, serde_json::json!({ "type": "object" }));
}
