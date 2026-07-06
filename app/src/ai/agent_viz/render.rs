//! Renders [`AgentVizModel`] as a text snapshot suitable for a
//! `CodeEditorView`-backed pane. The "office" layout is a 3-column ASCII
//! grid: tool rooms across the top, and special rooms (Idle, Thinking,
//! Permission, Done) in the bottom row. The active agent appears as `*`
//! inside whichever room it currently occupies.
//!
//! Below the grid, four collapsed-by-default sections list the agent's
//! configuration: system prompt, tools, MCP servers, skills.

use local_agent_runtime::ToolSchema;

use super::model::{AgentVizModel, Room};
use crate::ai::local_runtime_spec::{self, LocalRuntimeAttachment, McpServerInfo, SkillInfo};

const ROOM_WIDTH: usize = 24;
const ROOM_HEIGHT: usize = 3;

/// Build the full text snapshot for the visualization pane.
pub fn render_snapshot<F>(
    model: &AgentVizModel,
    mcp: &[McpServerInfo],
    skills: &[SkillInfo],
    tools_provider: F,
) -> String
where
    F: FnOnce() -> Vec<ToolSchema>,
{
    let tools = tools_provider();
    let mut out = String::new();

    out.push_str("Agent Office\n");
    out.push_str("============\n\n");

    out.push_str(&render_office(model, &tools));
    out.push('\n');

    out.push_str(&render_status_line(model));
    out.push('\n');

    out.push_str(&render_section(
        "System prompt",
        local_runtime_spec::system_prompt(),
    ));
    out.push('\n');

    out.push_str(&render_tools_section(&tools));
    out.push('\n');

    out.push_str(&render_mcp_section(mcp));
    out.push('\n');

    out.push_str(&render_skills_section(skills));

    out
}

fn render_office(model: &AgentVizModel, tools: &[ToolSchema]) -> String {
    // Top row: one room per tool. Bottom row: Thinking | Permission | Idle/Done.
    let top_rooms: Vec<Room> = tools.iter().map(|t| Room::Tool(t.name.clone())).collect();

    let bottom_rooms: Vec<Room> = vec![
        Room::Thinking,
        Room::Permission,
        if model.agents.values().any(|m| m.current_room == Room::Done) {
            Room::Done
        } else {
            Room::Idle
        },
    ];

    let mut out = String::new();
    out.push_str(&render_row(&top_rooms, model));
    out.push('\n');
    out.push_str(&render_row(&bottom_rooms, model));
    out
}

fn render_row(rooms: &[Room], model: &AgentVizModel) -> String {
    let mut lines: Vec<String> = (0..ROOM_HEIGHT).map(|_| String::new()).collect();

    for (i, room) in rooms.iter().enumerate() {
        let occupants: Vec<&str> = model
            .agents
            .values()
            .filter(|m| &m.current_room == room)
            .map(|_| "*")
            .collect();
        let dot = if occupants.is_empty() { ' ' } else { '*' };

        let label = truncate(&room.label(), ROOM_WIDTH - 2);
        let top = format!("┌{:─<width$}┐", "", width = ROOM_WIDTH - 2);
        let mid = format!("│{:^width$}│", label, width = ROOM_WIDTH - 2);
        let bot = format!("│{:^width$}│", format!(" {dot} "), width = ROOM_WIDTH - 2);

        if i > 0 {
            for line in &mut lines {
                line.push(' ');
            }
        }
        lines[0].push_str(&top);
        lines[1].push_str(&mid);
        lines[2].push_str(&bot);
    }

    let _ = ROOM_HEIGHT; // silence unused warning if ROOM_HEIGHT is later increased
    let footer: String = rooms
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let prefix = if i > 0 { " " } else { "" };
            format!("{prefix}└{:─<width$}┘", "", width = ROOM_WIDTH - 2)
        })
        .collect();

    let mut out = lines.join("\n");
    out.push('\n');
    out.push_str(&footer);
    out
}

fn render_status_line(model: &AgentVizModel) -> String {
    if model.agents.is_empty() {
        return "Status: idle (no agent runs observed yet)\n".to_string();
    }
    let mut lines = String::new();
    for marker in model.agents.values() {
        lines.push_str(&format!(
            "Agent {}: {} (was: {})\n",
            marker.id.0,
            marker.current_room.label(),
            marker.prev_room.label()
        ));
    }
    if let Some(text) = &model.last_text_delta {
        lines.push_str(&format!("Last text: {}\n", truncate(text, 120)));
    }
    if let Some(warn) = &model.last_warning {
        lines.push_str(&format!("Last warning: {}\n", truncate(warn, 200)));
    }
    lines
}

fn render_section(title: &str, body: &str) -> String {
    format!("── {title} ──\n{body}\n")
}

fn render_tools_section(tools: &[ToolSchema]) -> String {
    let mut s = format!("── Tools ({}) ──\n", tools.len());
    if tools.is_empty() {
        s.push_str("  (none)\n");
        return s;
    }
    for tool in tools {
        s.push_str(&format!("  • {} — {}\n", tool.name, tool.description));
    }
    s
}

fn render_mcp_section(servers: &[McpServerInfo]) -> String {
    let mut s = format!("── MCP servers ({}) ──\n", servers.len());
    if servers.is_empty() {
        s.push_str("  (no MCP servers configured)\n");
        return s;
    }
    for server in servers {
        s.push_str(&format!(
            "  • {} [{}]\n",
            server.name,
            attachment_badge(server.status)
        ));
    }
    s
}

fn render_skills_section(skills: &[SkillInfo]) -> String {
    let mut s = format!("── Skills ({}) ──\n", skills.len());
    if skills.is_empty() {
        s.push_str("  (no skills available in this scope)\n");
        return s;
    }
    for skill in skills {
        s.push_str(&format!(
            "  • {} ({}) [{}] — {}\n",
            skill.name,
            skill.source,
            attachment_badge(skill.status),
            truncate(&skill.description, 80)
        ));
    }
    s
}

fn attachment_badge(status: LocalRuntimeAttachment) -> &'static str {
    status.label()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use local_agent_runtime::RuntimeEvent;

    use super::*;
    use crate::ai::agent_viz::model::AgentVizModel;

    #[test]
    fn snapshot_contains_room_labels_and_sections() {
        let mut model = AgentVizModel::default();
        model.apply("run-1", &RuntimeEvent::TurnStarted { turn: 1 });

        let mcp = vec![McpServerInfo {
            name: "github".into(),
            status: LocalRuntimeAttachment::NotConnectedToLocalRuntime,
        }];
        let skills = vec![SkillInfo {
            name: "review-pr".into(),
            description: "Review pull requests".into(),
            source: "Bundled".into(),
            status: LocalRuntimeAttachment::NotConnectedToLocalRuntime,
        }];

        let snap = render_snapshot(&model, &mcp, &skills, Vec::new);

        assert!(snap.contains("Agent Office"));
        assert!(snap.contains("Thinking"));
        assert!(snap.contains("Permission"));
        assert!(snap.contains("System prompt"));
        assert!(snap.contains("Tools (0)"));
        assert!(snap.contains("MCP servers (1)"));
        assert!(snap.contains("github"));
        assert!(snap.contains("Skills (1)"));
        assert!(snap.contains("review-pr"));
        assert!(snap.contains("not connected to local runtime"));
    }
}
