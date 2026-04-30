//! Error types for the local agent runtime.

use thiserror::Error;

/// Errors from the LLM provider layer.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("Provider request failed: {0}")]
    RequestFailed(#[from] anyhow::Error),

    #[error("Provider returned no response")]
    EmptyResponse,

    #[error("Model not found: {model}")]
    ModelNotFound { model: String },

    #[error("Provider timeout after {seconds}s")]
    Timeout { seconds: u64 },

    #[error("Provider rate limited")]
    RateLimited,
}

/// Errors from tool execution.
#[derive(Debug, Error)]
pub enum ToolExecutionError {
    #[error("Tool not found: {name}")]
    NotFound { name: String },

    #[error("Tool execution failed: {0}")]
    ExecutionFailed(#[from] anyhow::Error),

    #[error("Tool execution timed out")]
    Timeout,

    #[error("Permission denied for tool: {name}")]
    PermissionDenied { name: String },

    #[error("Invalid tool input: {reason}")]
    InvalidInput { reason: String },
}

/// Top-level runtime errors.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("Tool execution error: {0}")]
    ToolExecution(#[from] ToolExecutionError),

    #[error("Max turns exceeded: {max_turns}")]
    MaxTurnsExceeded { max_turns: u32 },

    #[error("Runtime cancelled")]
    Cancelled,

    #[error("Internal error: {0}")]
    Internal(String),
}
