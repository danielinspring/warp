//! Internal message types for the runtime's conversation history.
//!
//! These are provider-agnostic. The provider layer translates them
//! to/from the specific format each LLM API expects.

pub mod normalize;

use crate::tools::{ToolCall, ToolCallResult};

/// A message in the conversation history.
#[derive(Debug, Clone)]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

#[derive(Debug, Clone)]
pub struct SystemMessage {
    pub content: String,
}

/// A single piece of user-provided content within a [`UserMessage`].
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text(String),
    Image {
        mime_type: String,
        data_base64: String,
        file_name: Option<String>,
    },
}

/// A user message, which may contain text and/or image content parts.
#[derive(Debug, Clone)]
pub struct UserMessage {
    pub parts: Vec<ContentPart>,
}

impl UserMessage {
    /// Build a text-only user message.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            parts: vec![ContentPart::Text(content.into())],
        }
    }

    /// Concatenated text from all text parts (image parts are omitted).
    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// True when this message carries at least one image part.
    pub fn has_images(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, ContentPart::Image { .. }))
    }
}

impl From<String> for UserMessage {
    fn from(content: String) -> Self {
        Self::text(content)
    }
}

impl From<&str> for UserMessage {
    fn from(content: &str) -> Self {
        Self::text(content)
    }
}

#[derive(Debug, Clone)]
pub struct AssistantMessage {
    /// Text content from the assistant (may be empty if only tool calls).
    pub content: String,
    /// Tool calls the assistant requested (empty if text-only reply).
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
pub struct ToolResultMessage {
    /// The tool_call_id this result corresponds to.
    pub call_id: String,
    /// The result content.
    pub result: ToolCallResult,
}

/// Conversation history with helper methods.
#[derive(Debug, Clone, Default)]
pub struct ConversationHistory {
    messages: Vec<Message>,
}

impl ConversationHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize with a system prompt.
    pub fn with_system_prompt(prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::System(SystemMessage {
                content: prompt.into(),
            })],
        }
    }

    /// Add a text-only user message.
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages
            .push(Message::User(UserMessage::text(content)));
    }

    /// Add a user message, which may include image content parts.
    pub fn push_user_message(&mut self, message: UserMessage) {
        self.messages.push(Message::User(message));
    }

    /// Add an assistant message.
    pub fn push_assistant(&mut self, content: impl Into<String>, tool_calls: Vec<ToolCall>) {
        self.messages.push(Message::Assistant(AssistantMessage {
            content: content.into(),
            tool_calls,
        }));
    }

    /// Add a tool result message.
    pub fn push_tool_result(&mut self, call_id: impl Into<String>, result: ToolCallResult) {
        self.messages.push(Message::ToolResult(ToolResultMessage {
            call_id: call_id.into(),
            result,
        }));
    }

    /// Get all messages.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Get total message count.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}
