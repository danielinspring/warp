//! State model for the agent office visualization.
//!
//! Maps incoming [`RuntimeEvent`]s to discrete rooms an agent can occupy.
//! Today the runtime is single-agent, but the model already keys agents by
//! `AgentId` so adding a second tracked agent later is a render change, not
//! a model change.

use std::collections::HashMap;

use local_agent_runtime::events::{FinishReason, StopReason};
use local_agent_runtime::RuntimeEvent;

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
    pub fn apply(&mut self, run_id: &str, event: &RuntimeEvent) {
        let id = AgentId(run_id.to_string());
        let marker = self
            .agents
            .entry(id.clone())
            .or_insert_with(|| AgentMarker::new(id));

        match event {
            RuntimeEvent::TurnStarted { .. } => marker.move_to(Room::Thinking),
            RuntimeEvent::ToolExecutionStarted { tool_name, .. } => {
                marker.move_to(Room::Tool(tool_name.clone()))
            }
            RuntimeEvent::ToolResult { .. } => marker.move_to(Room::Thinking),
            RuntimeEvent::PermissionRequired { .. } => marker.move_to(Room::Permission),
            RuntimeEvent::TextDelta { text } => {
                marker.move_to(Room::Thinking);
                self.last_text_delta = Some(text.clone());
            }
            RuntimeEvent::TextCompleted { text } => {
                marker.move_to(Room::Thinking);
                self.last_text_delta = Some(text.clone());
            }
            RuntimeEvent::TurnCompleted { reason } => match reason {
                StopReason::ToolUse => {} // stay in current room; tool execution will follow
                StopReason::EndTurn | StopReason::MaxTokens => marker.move_to(Room::Idle),
            },
            RuntimeEvent::Finished { reason } => {
                let final_room = match reason {
                    FinishReason::Done => Room::Done,
                    _ => Room::Idle,
                };
                marker.move_to(final_room);
            }
            RuntimeEvent::Warning { message } => {
                self.last_warning = Some(message.clone());
            }
            RuntimeEvent::ToolCallsRequested { .. } => {
                // No room change here; ToolExecutionStarted is the canonical signal.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use local_agent_runtime::tools::{ToolCall, ToolCallResult};

    use super::*;

    fn run_id() -> &'static str {
        "test-run"
    }

    fn id() -> AgentId {
        AgentId(run_id().to_string())
    }

    #[test]
    fn turn_started_moves_to_thinking() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &RuntimeEvent::TurnStarted { turn: 1 });
        assert_eq!(m.agents[&id()].current_room, Room::Thinking);
    }

    #[test]
    fn tool_execution_moves_to_tool_room_then_back() {
        let mut m = AgentVizModel::default();
        m.apply(run_id(), &RuntimeEvent::TurnStarted { turn: 1 });
        m.apply(
            run_id(),
            &RuntimeEvent::ToolExecutionStarted {
                call_id: "c1".into(),
                tool_name: "grep".into(),
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Tool("grep".into()));

        m.apply(
            run_id(),
            &RuntimeEvent::ToolResult {
                call_id: "c1".into(),
                result: ToolCallResult {
                    content: "ok".into(),
                    is_error: false,
                },
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Thinking);
    }

    #[test]
    fn permission_required_parks_dot() {
        let mut m = AgentVizModel::default();
        m.apply(
            run_id(),
            &RuntimeEvent::PermissionRequired {
                call: ToolCall {
                    id: "c1".into(),
                    name: "run_shell_command".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Permission);
    }

    #[test]
    fn finished_done_moves_to_done() {
        let mut m = AgentVizModel::default();
        m.apply(
            run_id(),
            &RuntimeEvent::Finished {
                reason: FinishReason::Done,
            },
        );
        assert_eq!(m.agents[&id()].current_room, Room::Done);
    }
}
