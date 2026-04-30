//! Runtime events — the output stream of the agent loop.
//!
//! The consumer (Warp's bridge layer) maps these to `ResponseEvent` proto events.
//! The runtime itself never touches proto types.

use crate::messages::Message;
use crate::tools::{ToolCall, ToolCallResult};

/// Events yielded by [`AgentRuntime::run`].
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// A new LLM turn is starting.
    TurnStarted {
        turn: u32,
    },

    /// The LLM produced a text chunk (for streaming).
    TextDelta {
        text: String,
    },

    /// The LLM finished producing text for this turn (non-streaming fallback).
    TextCompleted {
        text: String,
    },

    /// The LLM requested one or more tool calls.
    ToolCallsRequested {
        calls: Vec<ToolCall>,
    },

    /// A tool call requires user permission before execution.
    PermissionRequired {
        call: ToolCall,
    },

    /// Permission was granted (or auto-approved), tool execution starting.
    ToolExecutionStarted {
        call_id: String,
        tool_name: String,
    },

    /// Tool execution completed with a result.
    ToolResult {
        call_id: String,
        result: ToolCallResult,
    },

    /// The LLM turn completed (no more tool calls requested).
    TurnCompleted {
        reason: StopReason,
    },

    /// A recoverable error occurred; runtime will continue.
    Warning {
        message: String,
    },

    /// The agent loop finished.
    Finished {
        reason: FinishReason,
    },
}

/// Why a single LLM turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Model issued a natural stop (end_turn / stop token).
    EndTurn,
    /// Model hit max output tokens.
    MaxTokens,
    /// Model produced tool calls (turn continues after execution).
    ToolUse,
}

/// Why the entire agent loop finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Model completed without requesting more tool calls.
    Done,
    /// Max turns reached.
    MaxTurns,
    /// Cancelled by caller.
    Cancelled,
    /// Fatal error.
    Error(String),
}

/// The complete history at the end of a run — useful for the caller.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub finish_reason: FinishReason,
    pub messages: Vec<Message>,
    pub turns_used: u32,
}
