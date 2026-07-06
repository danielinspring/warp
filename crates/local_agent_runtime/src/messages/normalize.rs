//! Message normalization utilities.
//!
//! Handles truncation, merging, and other transformations needed
//! before sending messages to the LLM provider.

use crate::config::ContextBudget;

use super::{ConversationHistory, Message};

/// Build the model-facing view of a transcript without mutating persisted history.
pub fn model_messages(messages: &[Message], budget: &ContextBudget) -> Vec<Message> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if let Message::ToolResult(tool_msg) = &mut message {
                tool_msg.result.content =
                    budget_tool_result_content(&tool_msg.result.content, budget);
            }
            message
        })
        .collect()
}

fn budget_tool_result_content(content: &str, budget: &ContextBudget) -> String {
    let original_chars = content.chars().count();
    if original_chars <= budget.max_tool_result_chars {
        return content.to_string();
    }

    let kept = content
        .chars()
        .take(budget.max_tool_result_chars)
        .collect::<String>();
    serde_json::json!({
        "truncated": true,
        "original_chars": original_chars,
        "kept_chars": budget.max_tool_result_chars,
        "content": kept,
    })
    .to_string()
}

impl ConversationHistory {
    /// Mutable access to messages (for normalization passes).
    pub(crate) fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.messages
    }
}
