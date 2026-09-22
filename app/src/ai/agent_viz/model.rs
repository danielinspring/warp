//! State model for the agent office visualization.
//!
//! Maps incoming [`AgentVizEvent`]s to discrete rooms an agent can occupy.
//! Today the runtime is single-agent, but the model already keys agents by
//! `AgentId` so adding a second tracked agent later is a render change, not
//! a model change.

use std::collections::HashMap;

use super::event::AgentVizEvent;

/// Identifier for a tracked agent. Today derived from the runtime's `run_id`.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct AgentId(pub String);

/// The discrete "rooms" an agent can be in. Each tool gets its own room
/// keyed by tool name; everything else is one of the fixed variants.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Room {
    Idle,
    Thinking,
    Tool(String),
    Permission,
    Done,
}

impl Room {
    pub fn label(&self) -> String {
        match self {
            Room::Idle => "Idle".to_string(),
            Room::Thinking => "Thinking".to_string(),
            Room::Tool(name) => format!("Tool: {name}"),
            Room::Permission => "Permission".to_string(),
            Room::Done => "Done".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentMarker {
    pub id: AgentId,
    pub current_room: Room,
    pub prev_room: Room,
}

impl AgentMarker {
    fn new(id: AgentId) -> Self {
        Self {
            id,
            current_room: Room::Idle,
            prev_room: Room::Idle,
        }
    }

    fn move_to(&mut self, room: Room) {
        if room == self.current_room {
            return;
        }
        self.prev_room = std::mem::replace(&mut self.current_room, room);
    }
}

#[derive(Debug, Default)]
pub struct AgentVizModel {
    pub agents: HashMap<AgentId, AgentMarker>,
    pub last_text_delta: Option<String>,
    pub last_warning: Option<String>,
}

impl AgentVizModel {
    pub fn apply(&mut self, run_id: &str, event: &AgentVizEvent) {
        let id = AgentId(run_id.to_string());
        let marker = self
            .agents
            .entry(id.clone())
            .or_insert_with(|| AgentMarker::new(id));

        match event {
            AgentVizEvent::TurnStarted => marker.move_to(Room::Thinking),
            AgentVizEvent::ToolRequested { .. } => {
                // No room change; the agent is still thinking until the tool actually starts.
            }
            AgentVizEvent::ToolStarted { tool_name } => {
                marker.move_to(Room::Tool(tool_name.clone()))
            }
            AgentVizEvent::ToolFinished => marker.move_to(Room::Thinking),
            AgentVizEvent::PermissionRequired => marker.move_to(Room::Permission),
            AgentVizEvent::Text { preview } => {
                marker.move_to(Room::Thinking);
                self.last_text_delta = Some(preview.clone());
            }
            AgentVizEvent::Finished { error } => {
                marker.move_to(if error.is_none() {
                    Room::Done
                } else {
                    Room::Idle
                });
                self.last_warning = error.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_id() -> &'static str {
        "run-1"
    }

    fn id() -> AgentId {
        AgentId(run_id().to_string())
    }

    #[test]
    fn turn_started_moves_to_thinking() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &AgentVizEvent::TurnStarted);
        assert_eq!(m.agents[&id()].current_room, Room::Thinking);
    }

    #[test]
    fn tool_execution_moves_to_tool_room_then_back() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &AgentVizEvent::TurnStarted);
        m.apply(
            run_id(),
            &AgentVizEvent::ToolStarted {
                tool_name: "grep".into(),
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Tool("grep".into()));

        m.apply(run_id(), &AgentVizEvent::ToolFinished);
        assert_eq!(m.agents[&id()].current_room, Room::Thinking);
    }

    /// A requested tool is not a started one; the client may still be asking the user about it.
    #[test]
    fn requesting_a_tool_leaves_the_marker_where_it_is() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &AgentVizEvent::TurnStarted);
        m.apply(
            run_id(),
            &AgentVizEvent::ToolRequested {
                tool_name: "run_shell_command".into(),
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Thinking);
    }

    #[test]
    fn permission_required_parks_dot() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &AgentVizEvent::PermissionRequired);
        assert_eq!(m.agents[&id()].current_room, Room::Permission);
    }

    #[test]
    fn finished_done_moves_to_done() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &AgentVizEvent::Finished { error: None });
        assert_eq!(m.agents[&id()].current_room, Room::Done);
        assert!(m.last_warning.is_none());
    }

    #[test]
    fn a_failed_run_parks_idle_and_keeps_the_error() {
        let mut m = AgentVizModel::default();
        m.apply(
            run_id(),
            &AgentVizEvent::Finished {
                error: Some("stream died".into()),
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Idle);
        assert_eq!(m.last_warning.as_deref(), Some("stream died"));
    }

    #[test]
    fn text_updates_the_status_preview() {
        let mut m = AgentVizModel::default();
        m.apply(
            run_id(),
            &AgentVizEvent::Text {
                preview: "hello".into(),
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Thinking);
        assert_eq!(m.last_text_delta.as_deref(), Some("hello"));
    }
}
