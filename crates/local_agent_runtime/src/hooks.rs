//! Trusted lifecycle hooks for the local agent runtime.
//!
//! Hooks let hosts enforce policy and observe tool lifecycle without forking
//! the core loop. Built-in implementations are in-process only (no untrusted
//! script execution).

use std::sync::Arc;

use crate::events::FinishReason;
use crate::tools::{PermissionDecision, ToolCall, ToolCallResult};

/// Decision from a pre-tool hook before permission checks and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolDecision {
    /// Continue with the normal permission + execute path.
    Allow,
    /// Block execution; the runtime synthesizes an error tool result.
    Deny { reason: String },
}

/// In-process lifecycle intercept points for tool use and run completion.
///
/// Default methods are no-ops so hosts can implement only the hooks they need.
#[async_trait::async_trait]
pub trait LifecycleHooks: Send + Sync {
    /// Called before permission check and tool execution.
    async fn pre_tool(&self, _call: &ToolCall) -> PreToolDecision {
        PreToolDecision::Allow
    }

    /// Called after a permission decision is obtained, before execute.
    /// Return value replaces the decision (for example force-deny).
    async fn on_permission(
        &self,
        _call: &ToolCall,
        decision: PermissionDecision,
    ) -> PermissionDecision {
        decision
    }

    /// Called after a tool finishes (or is denied/skipped with a result).
    async fn post_tool(&self, _call: &ToolCall, _result: &ToolCallResult) {}

    /// Called when the agent loop is about to emit [`RuntimeEvent::Finished`].
    async fn on_stop(&self, _reason: &FinishReason) {}
}

/// Default no-op hooks.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHooks;

#[async_trait::async_trait]
impl LifecycleHooks for NoopHooks {}

/// Logs hook points via `tracing` (trusted observability).
#[derive(Debug, Default, Clone, Copy)]
pub struct LoggingHooks;

#[async_trait::async_trait]
impl LifecycleHooks for LoggingHooks {
    async fn pre_tool(&self, call: &ToolCall) -> PreToolDecision {
        tracing::debug!(
            tool_call_id = %call.id,
            tool_name = %call.name,
            "lifecycle hook pre_tool"
        );
        PreToolDecision::Allow
    }

    async fn on_permission(
        &self,
        call: &ToolCall,
        decision: PermissionDecision,
    ) -> PermissionDecision {
        tracing::debug!(
            tool_call_id = %call.id,
            tool_name = %call.name,
            ?decision,
            "lifecycle hook on_permission"
        );
        decision
    }

    async fn post_tool(&self, call: &ToolCall, result: &ToolCallResult) {
        tracing::debug!(
            tool_call_id = %call.id,
            tool_name = %call.name,
            is_error = result.is_error,
            "lifecycle hook post_tool"
        );
    }

    async fn on_stop(&self, reason: &FinishReason) {
        tracing::debug!(?reason, "lifecycle hook on_stop");
    }
}

/// Fan-out to multiple hooks. Pre-tool denials short-circuit; permission
/// decisions are applied left-to-right; post/stop always run all hooks.
#[derive(Clone, Default)]
pub struct CompositeHooks {
    hooks: Vec<Arc<dyn LifecycleHooks>>,
}

impl CompositeHooks {
    pub fn new(hooks: Vec<Arc<dyn LifecycleHooks>>) -> Self {
        Self { hooks }
    }

    pub fn push(&mut self, hook: Arc<dyn LifecycleHooks>) {
        self.hooks.push(hook);
    }
}

#[async_trait::async_trait]
impl LifecycleHooks for CompositeHooks {
    async fn pre_tool(&self, call: &ToolCall) -> PreToolDecision {
        for hook in &self.hooks {
            match hook.pre_tool(call).await {
                PreToolDecision::Deny { reason } => {
                    return PreToolDecision::Deny { reason };
                }
                PreToolDecision::Allow => {}
            }
        }
        PreToolDecision::Allow
    }

    async fn on_permission(
        &self,
        call: &ToolCall,
        mut decision: PermissionDecision,
    ) -> PermissionDecision {
        for hook in &self.hooks {
            decision = hook.on_permission(call, decision).await;
        }
        decision
    }

    async fn post_tool(&self, call: &ToolCall, result: &ToolCallResult) {
        for hook in &self.hooks {
            hook.post_tool(call, result).await;
        }
    }

    async fn on_stop(&self, reason: &FinishReason) {
        for hook in &self.hooks {
            hook.on_stop(reason).await;
        }
    }
}

/// Denies tools whose names appear in `denied_tools` (exact match).
#[derive(Debug, Clone, Default)]
pub struct ToolNameDenyHooks {
    pub denied_tools: Vec<String>,
}

#[async_trait::async_trait]
impl LifecycleHooks for ToolNameDenyHooks {
    async fn pre_tool(&self, call: &ToolCall) -> PreToolDecision {
        if self.denied_tools.iter().any(|name| name == &call.name) {
            PreToolDecision::Deny {
                reason: format!("Tool `{}` blocked by trusted lifecycle policy", call.name),
            }
        } else {
            PreToolDecision::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: name.to_string(),
            arguments: json!({}),
        }
    }

    #[tokio::test]
    async fn tool_name_deny_hooks_block_listed_tools() {
        let hooks = ToolNameDenyHooks {
            denied_tools: vec!["run_shell_command".to_string()],
        };
        assert!(matches!(
            hooks.pre_tool(&sample_call("run_shell_command")).await,
            PreToolDecision::Deny { .. }
        ));
        assert_eq!(
            hooks.pre_tool(&sample_call("read_files")).await,
            PreToolDecision::Allow
        );
    }

    #[tokio::test]
    async fn composite_hooks_short_circuit_on_deny() {
        let hooks = CompositeHooks::new(vec![
            Arc::new(LoggingHooks),
            Arc::new(ToolNameDenyHooks {
                denied_tools: vec!["edit_files".to_string()],
            }),
        ]);
        assert!(matches!(
            hooks.pre_tool(&sample_call("edit_files")).await,
            PreToolDecision::Deny { .. }
        ));
        assert_eq!(
            hooks.pre_tool(&sample_call("read_files")).await,
            PreToolDecision::Allow
        );
    }
}
