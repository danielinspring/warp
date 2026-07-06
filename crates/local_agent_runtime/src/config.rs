//! Configuration for the local agent runtime.

use std::time::Duration;

/// Model-facing context budget.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Maximum characters to include from one tool result before replacing it with a compact
    /// metadata wrapper.
    pub max_tool_result_chars: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_tool_result_chars: 100_000,
        }
    }
}

/// Configuration for a runtime session.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Maximum number of LLM turns (user→assistant→tool→assistant counts as turns with tools).
    pub max_turns: u32,

    /// Timeout for a single LLM call.
    pub llm_timeout: Duration,

    /// Timeout for a single tool execution.
    pub tool_timeout: Duration,

    /// Maximum characters to include from a tool result before truncating.
    pub max_tool_result_chars: usize,

    /// Budget applied only to the model-facing copy of the transcript.
    pub context_budget: ContextBudget,

    /// Whether to stop the loop when permission is denied (vs. skip the tool).
    pub stop_on_permission_denied: bool,

    /// System prompt to prepend to conversations.
    pub system_prompt: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_turns: 25,
            llm_timeout: Duration::from_secs(300),
            tool_timeout: Duration::from_secs(120),
            max_tool_result_chars: 100_000,
            context_budget: ContextBudget::default(),
            stop_on_permission_denied: false,
            system_prompt: None,
        }
    }
}
