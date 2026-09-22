//! Session context shown alongside the office grid: the MCP servers and skills available to a run.

use warpui::{AppContext, SingletonEntity};

use crate::ai::skills::SkillManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attachment {
    Active,
}

impl Attachment {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub status: Attachment,
}

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: String,
    pub status: Attachment,
}

#[cfg(not(target_family = "wasm"))]
pub fn local_mcp_servers(ctx: &AppContext) -> Vec<McpServerInfo> {
    use crate::ai::mcp::TemplatableMCPServerManager;
    TemplatableMCPServerManager::get_all_runnable_mcp_servers(ctx)
        .into_iter()
        .map(|(_uuid, name)| McpServerInfo {
            name,
            status: Attachment::Active,
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
            status: Attachment::Active,
        })
        .collect()
}
