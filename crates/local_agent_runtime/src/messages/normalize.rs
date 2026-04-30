//! Message normalization utilities.
//!
//! Handles truncation, merging, and other transformations needed
//! before sending messages to the LLM provider.

use super::{ConversationHistory, Message};

/// Truncate tool result content that exceeds the character budget.
pub fn truncate_tool_results(history: &mut ConversationHistory, max_chars: usize) {
    for msg in history.messages_mut() {
        if let Message::ToolResult(ref mut tool_msg) = msg {
            if tool_msg.result.content.len() > max_chars {
                tool_msg.result.content.truncate(max_chars);
                tool_msg
                    .result
                    .content
                    .push_str("\n\n... [output truncated]");
            }
        }
    }
}

impl ConversationHistory {
    /// Mutable access to messages (for normalization passes).
    pub(crate) fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.messages
    }
}
