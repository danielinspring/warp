//! Configuration for the local agent runtime.

use std::time::Duration;

/// Model-facing context budget.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Maximum model context size in approximate tokens. `None` disables whole-request
    /// budgeting while retaining per-tool-result limits.
    pub max_input_tokens: Option<usize>,

    /// Tokens reserved for the model's response.
    pub reserved_output_tokens: usize,

    /// Conservative character-to-token estimate used when no provider tokenizer is available.
    pub approximate_chars_per_token: usize,

    /// Maximum characters to include from one tool result before replacing it with a compact
    /// metadata wrapper.
    pub max_tool_result_chars: usize,

    /// Number of most recent complete user turns that compaction must preserve.
    pub preserve_recent_turns: usize,

    /// Maximum characters retained in the structured summary of omitted turns.
    pub max_compaction_summary_chars: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_input_tokens: None,
            reserved_output_tokens: 2_048,
            approximate_chars_per_token: 4,
            max_tool_result_chars: 100_000,
            preserve_recent_turns: 2,
            max_compaction_summary_chars: 4_000,
        }
    }
}

/// Configuration for a runtime session.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Maximum number of LLM turns (user→assistant→tool→assistant counts as turns with tools).
    /// This is an absolute safety ceiling, not the normal completion mechanism.
    pub max_turns: u32,

    /// Number of retry attempts after the initial provider call for transient failures.
    pub max_provider_retries: u32,

    /// Initial exponential backoff between provider retries.
    pub provider_retry_initial_backoff: Duration,

    /// Maximum number of automatic continuations after output-token termination.
    pub max_continuations: u32,

    /// Maximum number of identical consecutive tool-call cycles before declaring a stall.
    pub max_repeated_tool_cycles: u32,

    /// Timeout for a single LLM call.
    pub llm_timeout: Duration,

    /// Timeout for a single tool execution.
    pub tool_timeout: Duration,

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
            max_turns: 100,
            max_provider_retries: 2,
            provider_retry_initial_backoff: Duration::from_millis(250),
            max_continuations: 3,
            max_repeated_tool_cycles: 3,
            llm_timeout: Duration::from_secs(300),
            tool_timeout: Duration::from_secs(120),
            context_budget: ContextBudget::default(),
            stop_on_permission_denied: false,
            system_prompt: None,
        }
    }
}
