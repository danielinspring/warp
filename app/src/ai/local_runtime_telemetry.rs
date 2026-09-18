//! Product telemetry for the local Ollama / LiteLLM agent runtime.

use local_agent_runtime::RuntimeTelemetryEvent;
use serde::Serialize;
use serde_json::json;
use strum_macros::{EnumDiscriminants, EnumIter};
use warp_core::telemetry::{EnablementState, TelemetryEvent, TelemetryEventDesc};

/// Warp-facing telemetry events for local agent runtime runs.
#[derive(Debug, EnumDiscriminants)]
#[strum_discriminants(derive(EnumIter))]
pub(crate) enum LocalRuntimeTelemetryEvent {
    RunStarted(LocalRuntimeRunStarted),
    ProviderTurn(LocalRuntimeProviderTurn),
    ProviderError(LocalRuntimeProviderError),
    ToolPermission(LocalRuntimeToolPermission),
    ToolResult(LocalRuntimeToolResult),
    RunFinished(LocalRuntimeRunFinished),
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalRuntimeRunStarted {
    pub model: String,
    pub history_message_count: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalRuntimeProviderTurn {
    pub turn: u32,
    pub latency_ms: u64,
    pub has_tool_calls: bool,
    pub streamed_text: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalRuntimeProviderError {
    pub turn: u32,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalRuntimeToolPermission {
    pub call_id: String,
    pub tool_name: String,
    pub decision: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalRuntimeToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub is_error: bool,
    pub started: bool,
    pub latency_ms: u64,
    pub denied_by_hook: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalRuntimeRunFinished {
    pub reason: String,
    pub turns: u32,
}

impl From<RuntimeTelemetryEvent> for LocalRuntimeTelemetryEvent {
    fn from(event: RuntimeTelemetryEvent) -> Self {
        match event {
            RuntimeTelemetryEvent::RunStarted {
                model,
                history_message_count,
            } => Self::RunStarted(LocalRuntimeRunStarted {
                model,
                history_message_count,
            }),
            RuntimeTelemetryEvent::ProviderTurn {
                turn,
                latency_ms,
                has_tool_calls,
                streamed_text,
            } => Self::ProviderTurn(LocalRuntimeProviderTurn {
                turn,
                latency_ms,
                has_tool_calls,
                streamed_text,
            }),
            RuntimeTelemetryEvent::ProviderError { turn, message } => {
                Self::ProviderError(LocalRuntimeProviderError { turn, message })
            }
            RuntimeTelemetryEvent::ToolPermission {
                call_id,
                tool_name,
                decision,
            } => Self::ToolPermission(LocalRuntimeToolPermission {
                call_id,
                tool_name,
                decision,
            }),
            RuntimeTelemetryEvent::ToolResult {
                call_id,
                tool_name,
                is_error,
                started,
                latency_ms,
                denied_by_hook,
            } => Self::ToolResult(LocalRuntimeToolResult {
                call_id,
                tool_name,
                is_error,
                started,
                latency_ms,
                denied_by_hook,
            }),
            RuntimeTelemetryEvent::RunFinished { reason, turns } => {
                Self::RunFinished(LocalRuntimeRunFinished { reason, turns })
            }
        }
    }
}

impl TelemetryEvent for LocalRuntimeTelemetryEvent {
    fn name(&self) -> &'static str {
        LocalRuntimeTelemetryEventDiscriminants::from(self).name()
    }

    fn payload(&self) -> Option<serde_json::Value> {
        match self {
            Self::RunStarted(e) => Some(json!(e)),
            Self::ProviderTurn(e) => Some(json!(e)),
            Self::ProviderError(e) => Some(json!(e)),
            Self::ToolPermission(e) => Some(json!(e)),
            Self::ToolResult(e) => Some(json!(e)),
            Self::RunFinished(e) => Some(json!(e)),
        }
    }

    fn description(&self) -> &'static str {
        LocalRuntimeTelemetryEventDiscriminants::from(self).description()
    }

    fn enablement_state(&self) -> EnablementState {
        LocalRuntimeTelemetryEventDiscriminants::from(self).enablement_state()
    }

    fn contains_ugc(&self) -> bool {
        // Tool names / model ids are product schema, not free-form user content.
        false
    }

    fn event_descs() -> impl Iterator<Item = Box<dyn TelemetryEventDesc>> {
        warp_core::telemetry::enum_events::<Self>()
    }
}

impl TelemetryEventDesc for LocalRuntimeTelemetryEventDiscriminants {
    fn name(&self) -> &'static str {
        match self {
            Self::RunStarted => "AgentMode.LocalRuntime.RunStarted",
            Self::ProviderTurn => "AgentMode.LocalRuntime.ProviderTurn",
            Self::ProviderError => "AgentMode.LocalRuntime.ProviderError",
            Self::ToolPermission => "AgentMode.LocalRuntime.ToolPermission",
            Self::ToolResult => "AgentMode.LocalRuntime.ToolResult",
            Self::RunFinished => "AgentMode.LocalRuntime.RunFinished",
        }
    }

    fn description(&self) -> &'static str {
        match self {
            Self::RunStarted => "Local agent runtime run started",
            Self::ProviderTurn => "Local agent runtime provider turn completed",
            Self::ProviderError => "Local agent runtime provider error",
            Self::ToolPermission => "Local agent runtime tool permission decision",
            Self::ToolResult => "Local agent runtime tool result",
            Self::RunFinished => "Local agent runtime run finished",
        }
    }

    fn enablement_state(&self) -> EnablementState {
        EnablementState::Always
    }
}

warp_core::register_telemetry_event!(LocalRuntimeTelemetryEvent);
