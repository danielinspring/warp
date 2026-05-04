//! Single source of truth for the local Ollama runtime's "agent configuration":
//! system prompt, tool schemas, and the (display-only) view of MCP servers
//! and skills.
//!
//! Today only `system_prompt()` and `local_tools()` feed the runtime; MCP and
//! skills are surfaced for the agent visualization but `local_mcp_servers` and
//! `local_skills` always tag entries with
//! `LocalRuntimeAttachment::NotConnectedToLocalRuntime`. When MCP/skills are
//! plumbed into `local_agent_runtime`, those two functions flip the relevant
//! entries to `Active`.

use local_agent_runtime::ToolSchema;
use warpui::{AppContext, SingletonEntity};

use crate::ai::local_runtime_bridge::build_tool_schemas;
use crate::ai::skills::SkillManager;

pub const SYSTEM_PROMPT: &str = "You are a coding assistant running locally via Ollama, integrated into the Warp terminal. Reply concisely. When you need to take an action (run a command, read a file, etc.), prefer to call the matching tool; otherwise reply with plain text.";

pub fn system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

pub fn local_tools() -> Vec<ToolSchema> {
    build_tool_schemas()
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
            status: LocalRuntimeAttachment::NotConnectedToLocalRuntime,
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
            status: LocalRuntimeAttachment::NotConnectedToLocalRuntime,
        })
        .collect()
}
