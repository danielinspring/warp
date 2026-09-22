//! Single source of truth for the local runtime's "agent configuration": system prompt and tool
//! schemas.
//!
//! The request-scoped tool registry feeds both the system prompt and runtime so advertised
//! capabilities cannot drift from executable tools.

use std::fmt::Write as _;

use local_agent_runtime::ToolSchema;
use warp_multi_agent_api as api;
use warp_multi_agent_api::input_context::git::pull_request::State as PullRequestState;
use warp_multi_agent_api::skill_descriptor::SkillReference;
use warp_multi_agent_api::skill_descriptor::provider::Type as SkillProvider;
use warp_multi_agent_api::skill_descriptor::scope::Type as SkillScope;

use crate::model_packs::{self, ModelFamily};
use crate::registry::{LocalRuntimePermissionMode, LocalRuntimeToolRegistry};

pub const SYSTEM_PROMPT: &str = "You are a coding assistant running locally via Ollama, integrated into the Warp terminal. Reply concisely. When you need to take an action (run a command, read a file, etc.), prefer to call the matching tool; otherwise reply with plain text.\n\nIMPORTANT AFTER TOOLS: When you receive a tool result, treat it as ground truth. If status is completed (or exit_code is 0) and output answers the user, give that answer immediately — do not apologize, do not invent timeouts, and do not re-derive the answer with math unless the tool failed.\n\nIMPORTANT FOR SHELL: Prefer python3 over python on macOS. For quick calculations use run_shell_command with is_read_only=true, e.g. python3 -c 'print(sum(range(1, 101)))'. Only report command failure from the tool result exit_code/output — never invent timeouts or claim Python is missing unless the tool output says so.\n\nIMPORTANT FOR SEARCH: file_glob_v2 only searches under the current project/working directory and matches files. To locate a directory by name elsewhere, use run_shell_command with is_read_only=true and a FAST command. Prefer on macOS: mdfind 'kMDItemFSName == \"folder-name\"c' | head -20. Or scope find: find ~/codish -maxdepth 5 -type d -name 'folder-name' 2>/dev/null. Never run unbounded find ~ without -maxdepth (slow; blocks later tools). When a tool result includes exit_code/output, trust it and answer from that output — do not claim the environment is broken if the tool succeeded.\n\nIMPORTANT FOR EDITS: To change any file contents, you MUST call the 'edit_files' tool (never use shell commands like 'cat >', 'echo', or 'sed' to write files). Use 'edit_files' with an 'edits' array. Each edit is an object with 'type' ('replace', 'create', or 'delete'), 'file', and the relevant fields (search+replace for edits, or content for new files). The user will review the diff in the UI before it is applied. After reading a file, if the task requires a change, call edit_files in your next response instead of describing the change in text. Keep calling tools until the user's full request is satisfied.";

pub fn system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

pub fn system_prompt_for_request(
    request: &api::Request,
    registry: &LocalRuntimeToolRegistry,
) -> String {
    system_prompt_for_request_with_model(request, registry, "")
}

/// Build the request system prompt, applying a model-family pack when `model` is set.
pub fn system_prompt_for_request_with_model(
    request: &api::Request,
    registry: &LocalRuntimeToolRegistry,
    model: &str,
) -> String {
    let vision_enabled = local_agent_runtime::provider::ollama::model_supports_vision(model);
    let mut input = PromptBuildInput::from_request(request, registry, vision_enabled);
    input.model_family = model_packs::detect_model_family(model);
    format_system_prompt(&input)
}

pub fn local_tools() -> Vec<ToolSchema> {
    LocalRuntimeToolRegistry::built_ins().schemas()
}

#[derive(Debug, Clone, Default)]
struct PromptBuildInput {
    working_directory: Option<String>,
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
    todo_section: Option<String>,
    model_family: ModelFamily,
    vision_enabled: bool,
}

impl PromptBuildInput {
    fn from_request(
        request: &api::Request,
        registry: &LocalRuntimeToolRegistry,
        vision_enabled: bool,
    ) -> Self {
        let mut input = Self::from_request_context(request, vision_enabled);
        input.permission_mode = Some(permission_mode_label(registry.permission_mode()).to_string());
        input.local_tool_names = registry
            .schemas()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        input.available_skills = registry
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
            .collect();
        input.todo_section = registry.todo_prompt_section();
        input
    }

    /// The registry-independent part of the prompt input: everything derived from the request
    /// itself.
    fn from_request_context(request: &api::Request, vision_enabled: bool) -> Self {
        let context = request
            .input
            .as_ref()
            .and_then(|input| input.context.as_ref());
        let settings = request.settings.as_ref();

        let mut input = Self {
            vision_enabled,
            working_directory: context
                .and_then(|context| context.directory.as_ref())
                .map(|directory| directory.pwd.as_str())
                .filter(|pwd| !pwd.is_empty())
                .map(str::to_string),
            shell: context
                .and_then(|context| context.shell.as_ref())
                .and_then(shell_label),
            memory_enabled: settings.is_some_and(|settings| settings.rules_enabled),
            warp_drive_context_enabled: settings
                .is_some_and(|settings| settings.warp_drive_context_enabled),
            context_lines: context
                .map(|context| render_request_context(context, vision_enabled))
                .unwrap_or_default(),
            ..Default::default()
        };

        if let Some(mcp_context) = &request.mcp_context {
            let (servers, tools, resources) = count_mcp_context(mcp_context);
            input.mcp_server_count = servers;
            input.mcp_tool_count = tools;
            input.mcp_resource_count = resources;
        }

        input
    }
}

fn permission_mode_label(mode: LocalRuntimePermissionMode) -> &'static str {
    match mode {
        LocalRuntimePermissionMode::Default => "default",
        LocalRuntimePermissionMode::AcceptEdits => "accept-edits",
        LocalRuntimePermissionMode::Plan => "plan",
    }
}

fn shell_label(shell: &api::input_context::Shell) -> Option<String> {
    if shell.name.is_empty() {
        return None;
    }
    if shell.version.is_empty() {
        Some(shell.name.clone())
    } else {
        Some(format!("{} {}", shell.name, shell.version))
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
    writeln!(
        prompt,
        "- Vision (image understanding): {}",
        if input.vision_enabled {
            "enabled; attached images are sent as visible image content"
        } else {
            "disabled; this model cannot see image pixels, only attachment metadata"
        }
    )
    .ok();
    prompt.push_str(
        "- Capabilities without a matching executable schema are unavailable in this local run. Do not claim or attempt planning, web search, computer use, research, or orchestration unless such a tool appears above. When `web_search` / `web_fetch` appear above, use them for current docs and URLs; do not invent sources.\n",
    );
    writeln!(
        prompt,
        "- MCP context visible: {} servers, {} tools, {} resources. MCP execution is not connected to this local runtime unless an active local tool schema advertises it.",
        input.mcp_server_count, input.mcp_tool_count, input.mcp_resource_count
    )
    .ok();

    if input.local_tool_names.iter().any(|n| n == "update_todos") {
        prompt.push_str(
            "\n## Planning / todos\n\
Use update_todos to REPLACE the full pending list (not merge). \
Ids you omit are removed from pending — they are not completed. \
Use mark_todos_completed to finish items. Keep ids stable when only titles change.\n",
        );
    }

    if input.local_tool_names.iter().any(|n| n == "git_status") {
        prompt.push_str(
            "\n## Git\n\
Prefer git_status, draft_commit_message_context, and draft_pr_summary_context over \
run_shell_command for git status/diff/commit-message/PR context. These are read-only \
and never commit, push, or open a PR.\n",
        );
    }

    if let Some(todo_section) = &input.todo_section {
        prompt.push('\n');
        prompt.push_str(todo_section);
        prompt.push('\n');
    }

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

    model_packs::append_prompt_addendum(&mut prompt, input.model_family);

    prompt
}

/// Render one prompt line per populated piece of request context.
pub(crate) fn render_request_context(
    context: &api::InputContext,
    vision_enabled: bool,
) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(directory) = &context.directory {
        lines.push(format!(
            "Directory: pwd={}, home_dir={}, file_symbols_indexed={}",
            option_or_unknown(Some(directory.pwd.as_str())),
            option_or_unknown(Some(directory.home.as_str())),
            directory.pwd_file_symbols_indexed
        ));
    }

    if let Some(os) = &context.operating_system {
        lines.push(format!(
            "Operating system: platform={}, distribution={}",
            option_or_unknown(Some(os.platform.as_str())),
            option_or_unknown(Some(os.distribution.as_str()))
        ));
    }

    for selected in &context.selected_text {
        lines.push(format!(
            "Selected text: {}",
            truncate_for_prompt(&selected.text, 600)
        ));
    }

    if let Some(current_time) = &context.current_time {
        lines.push(format!(
            "Current time: {}",
            timestamp_for_prompt(current_time)
        ));
    }

    for image in &context.images {
        lines.push(format!(
            "Image attachment: mime_type={}, size={} bytes, visibility={}",
            option_or_unknown(Some(image.mime_type.as_str())),
            image.data.len(),
            if vision_enabled {
                "sent to the model as image content; you can see its pixels"
            } else {
                "metadata only; this model cannot see image pixels, only this description"
            }
        ));
    }

    for codebase in &context.codebases {
        lines.push(format!(
            "Codebase: name={}, path={}",
            codebase.name, codebase.path
        ));
    }

    for rules in &context.project_rules {
        lines.push(format!(
            "Project rules: root_path={}, active_rules={}, additional_rule_paths={}",
            rules.root_path,
            join_or_none(
                &rules
                    .active_rule_files
                    .iter()
                    .map(|rule| rule.file_path.clone())
                    .collect::<Vec<_>>()
            ),
            join_or_none(&rules.additional_rule_file_paths)
        ));
    }

    for content in context
        .files
        .iter()
        .filter_map(|file| file.content.as_ref())
    {
        lines.push(format!(
            "File context: file={}, line_range={}, content={}",
            content.file_path,
            content
                .line_range
                .as_ref()
                .map(|range| format!("{}-{}", range.start, range.end))
                .unwrap_or_else(|| "all".to_string()),
            file_content_for_prompt(content)
        ));
    }

    if let Some(git) = &context.git {
        lines.push(format!(
            "Git: branch={}, head={}",
            option_or_unknown(Some(git.branch.as_str())),
            git.head
        ));
        if let Some(repository) = &git.repository {
            lines.push(format!(
                "Repository: name={}, owner={}, host={}",
                repository.name,
                option_or_unknown(Some(repository.owner.as_str())),
                option_or_unknown(Some(repository.host.as_str()))
            ));
        }
        if let Some(pull_request) = &git.pull_request {
            let (state, draft) = pull_request_state_for_prompt(pull_request.state);
            lines.push(format!(
                "Pull request: number={}, state={state}, draft={draft}, base_branch={}, url={}",
                pull_request.number, pull_request.base_branch, pull_request.url
            ));
        }
    }

    if let Some(skills_context) = &context.updated_skills_context {
        lines.push(format!(
            "Skills available in request context: {}",
            join_or_none(
                &skills_context
                    .available_skills
                    .iter()
                    .map(skill_descriptor_for_prompt)
                    .collect::<Vec<_>>()
            )
        ));
    }

    lines
}

fn timestamp_for_prompt(timestamp: &prost_types::Timestamp) -> String {
    u32::try_from(timestamp.nanos)
        .ok()
        .and_then(|nanos| chrono::DateTime::from_timestamp(timestamp.seconds, nanos))
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| format!("{}s since epoch", timestamp.seconds))
}

/// The proto folds draft status into the state enum; the prompt shows them as separate fields.
fn pull_request_state_for_prompt(state: i32) -> (&'static str, bool) {
    match PullRequestState::try_from(state).unwrap_or(PullRequestState::Unspecified) {
        PullRequestState::Unspecified => ("unknown", false),
        PullRequestState::OpenDraft => ("OPEN", true),
        PullRequestState::Open => ("OPEN", false),
        PullRequestState::Closed => ("CLOSED", false),
        PullRequestState::Merged => ("MERGED", false),
    }
}

fn skill_descriptor_for_prompt(skill: &api::SkillDescriptor) -> String {
    let reference = match &skill.skill_reference {
        Some(SkillReference::Path(path)) => path.as_str(),
        Some(SkillReference::BundledSkillId(id)) => id.as_str(),
        None => "unknown",
    };
    let scope = match skill.scope.as_ref().and_then(|scope| scope.r#type) {
        Some(SkillScope::Home(())) => "Home",
        Some(SkillScope::Project(())) => "Project",
        Some(SkillScope::Bundled(())) => "Bundled",
        None => "unknown",
    };
    let provider = match skill.provider.as_ref().and_then(|provider| provider.r#type) {
        Some(SkillProvider::Warp(())) => "Warp",
        Some(SkillProvider::Agents(())) => "Agents",
        Some(SkillProvider::Claude(())) => "Claude",
        Some(SkillProvider::Codex(())) => "Codex",
        Some(SkillProvider::Cursor(())) => "Cursor",
        Some(SkillProvider::Gemini(())) => "Gemini",
        Some(SkillProvider::Copilot(())) => "Copilot",
        Some(SkillProvider::Droid(())) => "Droid",
        Some(SkillProvider::Github(())) => "Github",
        Some(SkillProvider::OpenCode(())) => "OpenCode",
        Some(SkillProvider::Kiro(())) => "Kiro",
        None => "unknown",
    };
    format!(
        "{} (reference: {reference}, scope: {scope}, provider: {provider}): {}",
        skill.name, skill.description
    )
}

fn file_content_for_prompt(content: &api::FileContent) -> String {
    truncate_for_prompt(&content.content, 600)
}

#[allow(deprecated)]
fn count_mcp_context(context: &api::request::McpContext) -> (usize, usize, usize) {
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
    if enabled { "enabled" } else { "disabled" }
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
