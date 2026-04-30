//! Tool-related types and the `ToolExecutor` trait.
//!
//! The runtime uses these types to communicate tool calls and results.
//! The actual execution is delegated to the `ToolExecutor` implementor
//! (provided by the app layer).

pub mod schema;

use serde::{Deserialize, Serialize};

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call (for correlating with results).
    pub id: String,
    /// The tool name.
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: serde_json::Value,
}

/// The result of executing a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// Text content of the result.
    pub content: String,
    /// Whether the tool execution resulted in an error.
    pub is_error: bool,
}

impl ToolCallResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Permission decision for a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Tool can execute without user confirmation.
    Allow,
    /// Tool requires user confirmation before execution.
    Ask,
    /// Tool execution is denied.
    Deny { reason: String },
}

/// The executor trait — implemented by the app layer to bridge into
/// Warp's existing tool execution pipeline.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Get all available tool schemas for the current session.
    fn available_tools(&self) -> Vec<schema::ToolSchema>;

    /// Check if a tool call can auto-execute (permission check).
    async fn check_permission(&self, call: &ToolCall) -> PermissionDecision;

    /// Execute a tool call, returning the result.
    async fn execute(&self, call: &ToolCall) -> Result<ToolCallResult, crate::error::ToolExecutionError>;

    /// Notify that a permission request was answered by the user.
    /// Returns the updated decision.
    async fn on_permission_response(
        &self,
        call: &ToolCall,
        granted: bool,
    ) -> PermissionDecision;
}
