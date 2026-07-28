//! Single source of truth for the local Ollama runtime's "agent configuration":
//! system prompt, tool schemas, and the (display-only) view of MCP servers
//! and skills.
//!
//! The request-scoped tool registry feeds both the system prompt and runtime so
//! advertised capabilities cannot drift from executable tools.

use std::fmt::Write as _;

use local_agent_runtime::ToolSchema;
use warpui::{AppContext, SingletonEntity};

use crate::ai::agent::api::RequestParams;
use crate::ai::agent::AIAgentContext;
use crate::ai::local_runtime_bridge::{LocalRuntimePermissionMode, LocalRuntimeToolRegistry};
use crate::ai::skills::SkillManager;

pub const SYSTEM_PROMPT: &str = "You are a coding assistant running locally via Ollama, integrated into the Warp terminal. Reply concisely. When you need to take an action (run a command, read a file, etc.), prefer to call the matching tool; otherwise reply with plain text.\n\nIMPORTANT AFTER TOOLS: When you receive a tool result, treat it as ground truth. If status is completed (or exit_code is 0) and output answers the user, give that answer immediately — do not apologize, do not invent timeouts, and do not re-derive the answer with math unless the tool failed.\n\nIMPORTANT FOR SHELL: Prefer python3 over python on macOS. For quick calculations use run_shell_command with is_read_only=true, e.g. python3 -c 'print(sum(range(1, 101)))'. Only report command failure from the tool result exit_code/output — never invent timeouts or claim Python is missing unless the tool output says so.\n\nIMPORTANT FOR SEARCH: file_glob_v2 only searches under the current project/working directory and matches files. To locate a directory by name elsewhere, use run_shell_command with is_read_only=true and a FAST command. Prefer on macOS: mdfind 'kMDItemFSName == \"folder-name\"c' | head -20. Or scope find: find ~/codish -maxdepth 5 -type d -name 'folder-name' 2>/dev/null. Never run unbounded find ~ without -maxdepth (slow; blocks later tools). When a tool result includes exit_code/output, trust it and answer from that output — do not claim the environment is broken if the tool succeeded.\n\nIMPORTANT FOR EDITS: To change any file contents, you MUST call the 'edit_files' tool (never use shell commands like 'cat >', 'echo', or 'sed' to write files). Use 'edit_files' with an 'edits' array. Each edit is an object with 'type' ('replace', 'create', or 'delete'), 'file', and the relevant fields (search+replace for edits, or content for new files). The user will review the diff in the UI before it is applied. After reading a file, if the task requires a change, call edit_files in your next response instead of describing the change in text. Keep calling tools until the user's full request is satisfied.";

pub fn system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

pub fn system_prompt_for_request(
    params: &RequestParams,
    registry: &LocalRuntimeToolRegistry,
) -> String {
    format_system_prompt(&PromptBuildInput::from_request(params, registry))
}

pub fn local_tools() -> Vec<ToolSchema> {
    LocalRuntimeToolRegistry::built_ins().schemas()
}

#[derive(Debug, Clone, Default)]
struct PromptBuildInput {
    working_directory: Option<String>,
    session_type: Option<String>,
    shell: Option<String>,
    memory_enabled: bool,
    warp_drive_context_enabled: bool,
    permission_mode: Option<String>,
    local_tool_names: Vec<String>,
    available_skills: Vec<String>,
    mcp_server_count: usize,
    mcp_tool_count: usize,
    mcp_resource_count: usize,
    context_lines: Vec<String>,
}

impl PromptBuildInput {
    fn from_request(params: &RequestParams, registry: &LocalRuntimeToolRegistry) -> Self {
        let mut input = Self {
            working_directory: params.session_context.current_working_directory().clone(),
            session_type: params
                .session_context
                .session_type()
                .as_ref()
                .map(|session_type| format!("{session_type:?}")),
            shell: params
                .session_context
                .shell()
                .as_ref()
                .map(|shell| format!("{shell:?}")),
            memory_enabled: params.is_memory_enabled,
            warp_drive_context_enabled: params.warp_drive_context_enabled,
            permission_mode: Some(
                match registry.permission_mode() {
                    LocalRuntimePermissionMode::Default => "default",
                    LocalRuntimePermissionMode::AcceptEdits => "accept-edits",
                    LocalRuntimePermissionMode::Plan => "plan",
                }
                .to_string(),
            ),
            local_tool_names: registry
                .schemas()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            available_skills: registry
                .skill_catalog()
                .iter()
                .map(|skill| {
                    if skill.description.is_empty() {
                        format!("{} ({})", skill.name, skill.reference)
                    } else {
                        format!(
                            "{} ({}) — {}",
                            skill.name,
                            skill.reference,
                            truncate_for_prompt(&skill.description, 160)
                        )
                    }
                })
                .collect(),
            context_lines: render_request_context(params),
            ..Default::default()
        };

        if let Some(mcp_context) = &params.mcp_context {
            let (servers, tools, resources) = count_mcp_context(mcp_context);
            input.mcp_server_count = servers;
            input.mcp_tool_count = tools;
            input.mcp_resource_count = resources;
        }

        input
    }
}

fn format_system_prompt(input: &PromptBuildInput) -> String {
    let mut prompt = String::from(SYSTEM_PROMPT);
    prompt.push_str("\n\n## Local Runtime Context\n");

    writeln!(
        prompt,
        "- Working directory: {}",
        input.working_directory.as_deref().unwrap_or("unknown")
    )
    .ok();
    writeln!(
        prompt,
        "- Session type: {}",
        input.session_type.as_deref().unwrap_or("unknown")
    )
    .ok();
    if let Some(shell) = &input.shell {
        writeln!(prompt, "- Shell: {}", truncate_for_prompt(shell, 240)).ok();
    }

    writeln!(
        prompt,
        "- Memory: {}",
        if input.memory_enabled {
            "enabled; respect user memory and rules already present in the conversation context"
        } else {
            "disabled; do not assume stored user memory or rules"
        }
    )
    .ok();
    writeln!(
        prompt,
        "- Warp Drive context: {}",
        enabled_label(input.warp_drive_context_enabled)
    )
    .ok();

    prompt.push_str("\n## Runtime Capabilities and Settings\n");
    writeln!(
        prompt,
        "- Executable local tools: {}",
        join_or_none(&input.local_tool_names)
    )
    .ok();

    if input.local_tool_names.iter().any(|n| n == "edit_files") {
        prompt.push_str("\nTo edit or create files you must use the edit_files tool with the exact schema (edits array of {type, file, ...}). Do not write files with shell commands.");
        prompt.push_str("\nExample edit_files call (as JSON arguments):\n{\n  \"title\": \"Update greeting\",\n  \"edits\": [ { \"type\": \"replace\", \"file\": \"hello.rs\", \"search\": \"println!(\\\"Hello\\\");\", \"replace\": \"println!(\\\"Hello from the local agent!\\\");\", } ]\n}");
    }
    writeln!(
        prompt,
        "- Permission mode: {}",
        input.permission_mode.as_deref().unwrap_or("default")
    )
    .ok();
    prompt.push_str(
        "- Capabilities without a matching executable schema are unavailable in this local run. Do not claim or attempt planning, web search, computer use, research, or orchestration unless such a tool appears above.\n",
    );
    writeln!(
        prompt,
        "- MCP context visible: {} servers, {} tools, {} resources. MCP execution is not connected to this local runtime unless an active local tool schema advertises it.",
        input.mcp_server_count, input.mcp_tool_count, input.mcp_resource_count
    )
    .ok();

    if !input.available_skills.is_empty() {
        prompt.push_str("\n## Available Skills\n");
        prompt.push_str(
            "Call list_skills for the full catalog, then read_skill before following a skill. Bundled skill scripts/assets referenced in the skill body should be read with read_files.\n",
        );
        for line in input.available_skills.iter().take(40) {
            writeln!(prompt, "- {line}").ok();
        }
        if input.available_skills.len() > 40 {
            writeln!(
                prompt,
                "- ... {} additional skills omitted; use list_skills",
                input.available_skills.len() - 40
            )
            .ok();
        }
    }

    if !input.context_lines.is_empty() {
        prompt.push_str("\n## Request Context\n");
        for line in input.context_lines.iter().take(MAX_CONTEXT_LINES) {
            writeln!(prompt, "- {line}").ok();
        }
        if input.context_lines.len() > MAX_CONTEXT_LINES {
            writeln!(
                prompt,
                "- ... {} additional context items omitted",
                input.context_lines.len() - MAX_CONTEXT_LINES
            )
            .ok();
        }
    }

    prompt
}

fn render_request_context(params: &RequestParams) -> Vec<String> {
    let mut lines = Vec::new();
    for input in &params.input {
        let Some(contexts) = input.context() else {
            continue;
        };
        for context in contexts {
            lines.push(render_context_line(context));
        }
    }
    lines
}

fn render_context_line(context: &AIAgentContext) -> String {
    match context {
        AIAgentContext::Directory {
            pwd,
            home_dir,
            are_file_symbols_indexed,
        } => format!(
            "Directory: pwd={}, home_dir={}, file_symbols_indexed={}",
            option_or_unknown(pwd.as_deref()),
            option_or_unknown(home_dir.as_deref()),
            are_file_symbols_indexed
        ),
        AIAgentContext::SelectedText(text) => {
            format!("Selected text: {}", truncate_for_prompt(text, 600))
        }
        AIAgentContext::ExecutionEnvironment(env) => format!(
            "Execution environment: {}",
            env.to_json_string().unwrap_or_else(|| format!("{env:?}"))
        ),
        AIAgentContext::CurrentTime { current_time } => {
            format!("Current time: {}", current_time.to_rfc3339())
        }
        AIAgentContext::Image(image) => format!(
            "Image attachment: file={}, mime_type={}, figma={}",
            image.file_name, image.mime_type, image.is_figma
        ),
        AIAgentContext::Codebase { path, name } => {
            format!("Codebase: name={name}, path={path}")
        }
        AIAgentContext::ProjectRules {
            root_path,
            active_rules,
            additional_rule_paths,
        } => format!(
            "Project rules: root_path={}, active_rules={}, additional_rule_paths={}",
            root_path,
            join_or_none(
                &active_rules
                    .iter()
                    .map(|rule| rule.file_name.clone())
                    .collect::<Vec<_>>()
            ),
            join_or_none(additional_rule_paths)
        ),
        AIAgentContext::File(file) => format!(
            "File context: file={}, line_range={}, content={}",
            file.file_name,
            file.line_range
                .as_ref()
                .map(|range| format!("{}-{}", range.start, range.end))
                .unwrap_or_else(|| "all".to_string()),
            file_content_for_prompt(&file.content)
        ),
        AIAgentContext::Git { head, branch } => format!(
            "Git: branch={}, head={}",
            option_or_unknown(branch.as_deref()),
            head
        ),
        AIAgentContext::Repository { name, owner } => format!(
            "Repository: name={}, owner={}",
            name,
            option_or_unknown(owner.as_deref())
        ),
        AIAgentContext::PullRequest {
            number,
            state,
            draft,
            base_branch,
        } => format!(
            "Pull request: number={}, state={}, draft={}, base_branch={}",
            number, state, draft, base_branch
        ),
        AIAgentContext::Skills { skills } => format!(
            "Skills available in request context: {}",
            join_or_none(
                &skills
                    .iter()
                    .map(|skill| {
                        format!(
                            "{} (reference: {}, scope: {:?}, provider: {:?}): {}",
                            skill.name,
                            skill.reference,
                            skill.scope,
                            skill.provider,
                            skill.description
                        )
                    })
                    .collect::<Vec<_>>()
            )
        ),
        AIAgentContext::Block(block) => format!(
            "Terminal block: command={}, exit_code={}, pwd={}, output={}",
            block.command,
            block.exit_code.value(),
            option_or_unknown(block.pwd.as_deref()),
            truncate_for_prompt(&block.output, 800)
        ),
    }
}

fn file_content_for_prompt(content: &crate::ai::agent::AnyFileContent) -> String {
    match content {
        crate::ai::agent::AnyFileContent::StringContent(content) => {
            truncate_for_prompt(content, 600)
        }
        crate::ai::agent::AnyFileContent::BinaryContent(bytes) => {
            format!("<binary content, {} bytes>", bytes.len())
        }
    }
}

#[allow(deprecated)]
fn count_mcp_context(context: &crate::ai::agent::MCPContext) -> (usize, usize, usize) {
    if context.servers.is_empty() {
        return (0, context.tools.len(), context.resources.len());
    }

    let tool_count = context
        .servers
        .iter()
        .map(|server| server.tools.len())
        .sum::<usize>();
    let resource_count = context
        .servers
        .iter()
        .map(|server| server.resources.len())
        .sum::<usize>();
    (context.servers.len(), tool_count, resource_count)
}

const MAX_CONTEXT_LINES: usize = 24;
const TRUNCATION_MARKER: &str = "...[truncated]";

fn truncate_for_prompt(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}{TRUNCATION_MARKER}")
    } else {
        truncated
    }
}

fn option_or_unknown(value: Option<&str>) -> &str {
    value.filter(|value| !value.is_empty()).unwrap_or("unknown")
}

fn enabled_label(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRuntimeAttachment {
    Active,
    NotConnectedToLocalRuntime,
}

impl LocalRuntimeAttachment {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NotConnectedToLocalRuntime => "not connected to local runtime",
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub status: LocalRuntimeAttachment,
}

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: String,
    pub status: LocalRuntimeAttachment,
}

#[cfg(not(target_family = "wasm"))]
pub fn local_mcp_servers(ctx: &AppContext) -> Vec<McpServerInfo> {
    use crate::ai::mcp::TemplatableMCPServerManager;
    TemplatableMCPServerManager::get_all_runnable_mcp_servers(ctx)
        .into_iter()
        .map(|(_uuid, name)| McpServerInfo {
            name,
            status: LocalRuntimeAttachment::Active,
        })
        .collect()
}

#[cfg(target_family = "wasm")]
pub fn local_mcp_servers(_ctx: &AppContext) -> Vec<McpServerInfo> {
    Vec::new()
}

pub fn local_skills(ctx: &AppContext) -> Vec<SkillInfo> {
    SkillManager::as_ref(ctx)
        .get_skills_for_working_directory(None, ctx)
        .into_iter()
        .map(|skill| SkillInfo {
            name: skill.name,
            description: skill.description,
            source: format!("{:?}", skill.scope),
            status: LocalRuntimeAttachment::Active,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_prompt_includes_dynamic_runtime_context() {
        let prompt = format_system_prompt(&PromptBuildInput {
            working_directory: Some("/repo/warp".to_string()),
            session_type: Some("Local".to_string()),
            memory_enabled: true,
            warp_drive_context_enabled: true,
            permission_mode: Some("default".to_string()),
            local_tool_names: vec![
                "read_files".to_string(),
                "grep".to_string(),
                "ask_user_question".to_string(),
            ],
            context_lines: vec!["Git: branch=main, head=abc123".to_string()],
            ..Default::default()
        });

        assert!(prompt.contains(SYSTEM_PROMPT));
        assert!(prompt.contains("Working directory: /repo/warp"));
        assert!(prompt.contains("Memory: enabled"));
        assert!(prompt.contains("Warp Drive context: enabled"));
        assert!(prompt.contains("Executable local tools: read_files, grep, ask_user_question"));
        assert!(prompt.contains("Permission mode: default"));
        assert!(prompt.contains("Capabilities without a matching executable schema"));
        assert!(prompt.contains("Git: branch=main, head=abc123"));
    }

    #[test]
    fn request_prompt_respects_disabled_memory() {
        let prompt = format_system_prompt(&PromptBuildInput {
            memory_enabled: false,
            local_tool_names: vec!["run_shell_command".to_string()],
            ..Default::default()
        });

        assert!(prompt.contains("Memory: disabled; do not assume stored user memory or rules"));
        assert!(!prompt.contains("Memory: enabled"));
    }

    #[test]
    fn request_prompt_marks_mcp_context_as_visible_but_not_connected() {
        let prompt = format_system_prompt(&PromptBuildInput {
            mcp_server_count: 2,
            mcp_tool_count: 3,
            mcp_resource_count: 4,
            ..Default::default()
        });

        assert!(prompt.contains("MCP context visible: 2 servers, 3 tools, 4 resources"));
        assert!(prompt.contains("MCP execution is not connected to this local runtime"));
    }
}
