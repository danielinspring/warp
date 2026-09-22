//! Provider construction for a turn. The Warp client forwards the OpenAI-compatible endpoint it
//! has configured inside every request; tests substitute a scripted provider through the same
//! factory seam.

use local_agent_runtime::provider::ollama::{OllamaProvider, OllamaProviderConfig};
use local_agent_runtime::{
    ChatRequest, ChatResponse, ChatStreamEvent, LLMProvider, ProviderCapabilities, ProviderError,
};

/// Endpoint and model selected for one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

/// Builds the LLM provider used for a turn.
pub trait ProviderFactory: Send + Sync {
    fn create(&self, config: &ProviderConfig) -> Box<dyn LLMProvider>;
}

/// Talks to Ollama or any OpenAI-compatible chat-completions endpoint.
#[derive(Debug, Default, Clone, Copy)]
pub struct OllamaProviderFactory;

impl ProviderFactory for OllamaProviderFactory {
    fn create(&self, config: &ProviderConfig) -> Box<dyn LLMProvider> {
        Box::new(OllamaProvider::new(OllamaProviderConfig {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            timeout_secs: 300,
            ..Default::default()
        }))
    }
}

/// Adapter that lets a factory-produced provider satisfy `AgentRuntime`'s generic provider bound.
/// Orphan rules prevent implementing the runtime's trait for `Box<dyn LLMProvider>` here.
pub struct DynProvider(pub Box<dyn LLMProvider>);

#[async_trait::async_trait]
impl LLMProvider for DynProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.0.chat(request).await
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        event_tx: async_channel::Sender<ChatStreamEvent>,
    ) -> Result<ChatResponse, ProviderError> {
        self.0.chat_stream(request, event_tx).await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }

    fn name(&self) -> &str {
        self.0.name()
    }
}
