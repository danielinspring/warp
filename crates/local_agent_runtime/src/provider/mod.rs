//! LLM provider abstraction layer.
//!
//! The `LLMProvider` trait defines how the runtime talks to any LLM.
//! Each provider (Ollama, OpenAI-compatible, etc.) implements this trait.

pub mod ollama;

use crate::error::ProviderError;
use crate::messages::Message;
use crate::tools::schema::ToolSchema;

/// A chat request sent to the LLM.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// The model identifier (e.g., "qwen2.5-coder:7b").
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Tool schemas to advertise (empty = no tools).
    pub tools: Vec<ToolSchema>,
}

/// The LLM's response to a chat request.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Text content from the model (may be empty if only tool calls).
    pub text: String,
    /// Tool calls requested by the model (empty if text-only).
    pub tool_calls: Vec<crate::tools::ToolCall>,
    /// Why the model stopped generating.
    pub stop_reason: ChatStopReason,
}

/// Incremental events emitted by a provider while a chat response is streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatStreamEvent {
    /// A text chunk from the model.
    TextDelta { text: String },
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatStopReason {
    /// Natural end of response.
    Stop,
    /// Model wants to call tools.
    ToolUse,
    /// Hit output token limit.
    MaxTokens,
}

/// Provider capabilities — what the LLM supports.
#[derive(Debug, Clone, Default)]
pub struct ProviderCapabilities {
    /// Whether the provider supports streaming responses.
    pub streaming: bool,
    /// Whether the provider supports tool/function calling.
    pub tool_calling: bool,
    /// Whether the provider supports vision/image inputs.
    pub vision: bool,
}

/// The core trait for any LLM provider.
///
/// Implementations translate between the runtime's generic message format
/// and the provider-specific API format.
#[async_trait::async_trait]
pub trait LLMProvider: Send + Sync {
    /// Send a chat request and return the response.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError>;

    /// Send a chat request and emit incremental stream events as they arrive.
    ///
    /// Providers that don't support streaming can rely on this default
    /// implementation, which preserves the existing non-streaming behavior.
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _event_tx: async_channel::Sender<ChatStreamEvent>,
    ) -> Result<ChatResponse, ProviderError> {
        self.chat(request).await
    }

    /// Query provider capabilities.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Get the provider name (for logging/diagnostics).
    fn name(&self) -> &str;
}
