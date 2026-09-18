//! Structured telemetry for the local agent runtime.
//!
//! Hosts implement [`RuntimeTelemetrySink`] to forward events to product
//! analytics. The runtime never depends on Warp telemetry types.

use std::sync::Arc;

use instant::Instant;

use crate::events::FinishReason;
use crate::hooks::{LifecycleHooks, PreToolDecision};
use crate::tools::{PermissionDecision, ToolCall, ToolCallResult};

/// Coarse lifecycle events for observability and debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTelemetryEvent {
    /// A full agent run is starting.
    RunStarted {
        model: String,
        history_message_count: usize,
    },
    /// One provider (LLM) turn finished.
    ProviderTurn {
        turn: u32,
        latency_ms: u64,
        has_tool_calls: bool,
        streamed_text: bool,
    },
    /// Provider call failed after retries (before the run finishes).
    ProviderError { turn: u32, message: String },
    /// Permission decision for a tool (after hooks).
    ToolPermission {
        call_id: String,
        tool_name: String,
        decision: String,
    },
    /// Tool finished (executed, denied, or skipped).
    ToolResult {
        call_id: String,
        tool_name: String,
        is_error: bool,
        started: bool,
        latency_ms: u64,
        denied_by_hook: bool,
    },
    /// Run finished.
    RunFinished { reason: String, turns: u32 },
}

/// Sink for runtime telemetry (no-op by default).
pub trait RuntimeTelemetrySink: Send + Sync {
    fn emit(&self, event: RuntimeTelemetryEvent);
}

/// Discards all events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTelemetrySink;

impl RuntimeTelemetrySink for NoopTelemetrySink {
    fn emit(&self, _event: RuntimeTelemetryEvent) {}
}

/// Forwards events to a function (tests and simple hosts).
pub struct FnTelemetrySink<F>(pub F)
where
    F: Fn(RuntimeTelemetryEvent) + Send + Sync;

impl<F> RuntimeTelemetrySink for FnTelemetrySink<F>
where
    F: Fn(RuntimeTelemetryEvent) + Send + Sync,
{
    fn emit(&self, event: RuntimeTelemetryEvent) {
        (self.0)(event);
    }
}

/// Channel-based sink for bridging async runtime → UI thread.
pub struct ChannelTelemetrySink {
    tx: async_channel::Sender<RuntimeTelemetryEvent>,
}

impl ChannelTelemetrySink {
    pub fn new(tx: async_channel::Sender<RuntimeTelemetryEvent>) -> Self {
        Self { tx }
    }
}

impl RuntimeTelemetrySink for ChannelTelemetrySink {
    fn emit(&self, event: RuntimeTelemetryEvent) {
        let _ = self.tx.try_send(event);
    }
}

/// Lifecycle hooks that also emit tool-level telemetry with latency.
pub struct TelemetryLifecycleHooks {
    sink: Arc<dyn RuntimeTelemetrySink>,
    tool_started_at: std::sync::Mutex<Option<Instant>>,
}

impl TelemetryLifecycleHooks {
    pub fn new(sink: Arc<dyn RuntimeTelemetrySink>) -> Self {
        Self {
            sink,
            tool_started_at: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LifecycleHooks for TelemetryLifecycleHooks {
    async fn pre_tool(&self, call: &ToolCall) -> PreToolDecision {
        if let Ok(mut guard) = self.tool_started_at.lock() {
            *guard = Some(Instant::now());
        }
        tracing::trace!(
            tool_call_id = %call.id,
            tool_name = %call.name,
            "telemetry pre_tool"
        );
        PreToolDecision::Allow
    }

    async fn on_permission(
        &self,
        call: &ToolCall,
        decision: PermissionDecision,
    ) -> PermissionDecision {
        let decision_label = match &decision {
            PermissionDecision::Allow => "allow",
            PermissionDecision::Ask => "ask",
            PermissionDecision::Deny { .. } => "deny",
        };
        self.sink.emit(RuntimeTelemetryEvent::ToolPermission {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            decision: decision_label.to_string(),
        });
        decision
    }

    async fn post_tool(&self, call: &ToolCall, result: &ToolCallResult) {
        let latency_ms = self
            .tool_started_at
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
            .map(|started| started.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let denied_by_hook = result.is_error
            && result
                .content
                .contains("blocked by trusted lifecycle policy");
        self.sink.emit(RuntimeTelemetryEvent::ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            is_error: result.is_error,
            started: !result.content.contains("Permission required")
                && !result.content.starts_with("Permission denied:"),
            latency_ms,
            denied_by_hook,
        });
    }

    async fn on_stop(&self, _reason: &FinishReason) {
        // RunFinished is emitted by the runtime with turn counts.
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::hooks::LifecycleHooks;

    #[tokio::test]
    async fn telemetry_hooks_emit_permission_and_result() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let sink = Arc::new(FnTelemetrySink(move |e| {
            events_clone.lock().unwrap().push(e);
        }));
        let hooks = TelemetryLifecycleHooks::new(sink);
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_files".to_string(),
            arguments: json!({}),
        };
        assert_eq!(hooks.pre_tool(&call).await, PreToolDecision::Allow);
        let decision = hooks.on_permission(&call, PermissionDecision::Allow).await;
        assert_eq!(decision, PermissionDecision::Allow);
        hooks.post_tool(&call, &ToolCallResult::success("ok")).await;

        let recorded = events.lock().unwrap().clone();
        assert!(recorded.iter().any(|e| matches!(
            e,
            RuntimeTelemetryEvent::ToolPermission {
                decision,
                ..
            } if decision == "allow"
        )));
        assert!(recorded.iter().any(|e| matches!(
            e,
            RuntimeTelemetryEvent::ToolResult {
                is_error: false,
                ..
            }
        )));
    }
}
