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

    #[error("Authentication failed: {message}")]
    Unauthorized { message: String },

    #[error("Transient provider failure: {message}")]
    Transient { message: String },

    #[error("Provider context window exceeded: {message}")]
    ContextWindowExceeded { message: String },
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::Timeout { .. }
                | ProviderError::RateLimited
                | ProviderError::Transient { .. }
        )
    }

    pub fn is_context_window_exceeded(&self) -> bool {
        matches!(self, ProviderError::ContextWindowExceeded { .. })
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ProviderError::Unauthorized { .. })
    }
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

    #[error(
        "Model context budget exceeded: estimated {estimated_tokens} tokens for a {limit}-token limit"
    )]
    ContextBudgetExceeded {
        estimated_tokens: usize,
        limit: usize,
    },

    #[error("Maximum output continuations exceeded: {max_continuations}")]
    MaxContinuationsExceeded { max_continuations: u32 },

    #[error("Agent stalled after {repeated_cycles} identical tool-call cycles")]
    RepeatedToolCallStall { repeated_cycles: u32 },

    #[error("Runtime cancelled")]
    Cancelled,

    #[error("Internal error: {0}")]
    Internal(String),
}
