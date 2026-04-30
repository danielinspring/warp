//! Ollama LLM provider implementation.
//!
//! Talks to Ollama's OpenAI-compatible `/v1/chat/completions` endpoint.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProviderError;
use crate::messages::Message;
use crate::tools::ToolCall;

use super::{ChatRequest, ChatResponse, ChatStopReason, LLMProvider, ProviderCapabilities};

/// Configuration for connecting to an Ollama server.
#[derive(Debug, Clone)]
pub struct OllamaProviderConfig {
    /// Base URL of the Ollama server (e.g., "http://localhost:11434").
    pub base_url: String,
    /// Optional API key for authentication.
    pub api_key: Option<String>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
}

impl Default for OllamaProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            timeout_secs: 300,
        }
    }
}

/// Ollama LLM provider.
pub struct OllamaProvider {
    config: OllamaProviderConfig,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(config: OllamaProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// Fetch the list of locally-installed models.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/api/tags", self.base_url());
        let mut req = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5));
        if let Some(key) = &self.config.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(anyhow!("Could not reach Ollama: {}", e)))?;

        if !resp.status().is_success() {
            return Err(ProviderError::RequestFailed(anyhow!(
                "Ollama returned status {}",
                resp.status()
            )));
        }

        let tags: TagsResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.into()))?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    fn base_url(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    fn translate_messages(messages: &[Message]) -> Vec<OllamaChatMessage> {
        messages
            .iter()
            .map(|msg| match msg {
                Message::System(s) => OllamaChatMessage {
                    role: "system".to_string(),
                    content: Some(s.content.clone()),
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message::User(u) => OllamaChatMessage {
                    role: "user".to_string(),
                    content: Some(u.content.clone()),
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message::Assistant(a) => {
                    let tool_calls = if a.tool_calls.is_empty() {
                        None
                    } else {
                        Some(
                            a.tool_calls
                                .iter()
                                .map(|tc| OllamaToolCall {
                                    id: tc.id.clone(),
                                    r#type: "function".to_string(),
                                    function: OllamaFunction {
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.to_string(),
                                    },
                                })
                                .collect(),
                        )
                    };
                    OllamaChatMessage {
                        role: "assistant".to_string(),
                        content: if a.content.is_empty() {
                            None
                        } else {
                            Some(a.content.clone())
                        },
                        tool_call_id: None,
                        tool_calls,
                    }
                }
                Message::ToolResult(t) => OllamaChatMessage {
                    role: "tool".to_string(),
                    content: Some(t.result.content.clone()),
                    tool_call_id: Some(t.call_id.clone()),
                    tool_calls: None,
                },
            })
            .collect()
    }

    fn parse_tool_calls(raw: &[RawToolCall]) -> Vec<ToolCall> {
        raw.iter()
            .map(|tc| {
                let arguments = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(Value::String(tc.function.arguments.clone()));
                ToolCall {
                    id: if tc.id.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        tc.id.clone()
                    },
                    name: tc.function.name.clone(),
                    arguments,
                }
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl LLMProvider for OllamaProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let url = format!("{}/v1/chat/completions", self.base_url());

        let tools: Option<Vec<Value>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(|t| t.to_openai_tool()).collect())
        };

        let body = OllamaChatRequest {
            model: request.model,
            messages: Self::translate_messages(&request.messages),
            stream: false,
            tools,
        };

        let mut req = self
            .client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(self.config.timeout_secs));
        if let Some(key) = &self.config.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Timeout {
                    seconds: self.config.timeout_secs,
                }
            } else {
                ProviderError::RequestFailed(anyhow!("Ollama request failed: {}", e))
            }
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimited);
            }
            return Err(ProviderError::RequestFailed(anyhow!(
                "Ollama returned {}: {}",
                status,
                body_text
            )));
        }

        let chat_resp: OllamaChatResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.into()))?;

        let choice = chat_resp
            .choices
            .into_iter()
            .next()
            .ok_or(ProviderError::EmptyResponse)?;

        let text = choice.message.content.unwrap_or_default();
        let tool_calls = choice
            .message
            .tool_calls
            .map(|tcs| Self::parse_tool_calls(&tcs))
            .unwrap_or_default();

        let stop_reason = if !tool_calls.is_empty() {
            ChatStopReason::ToolUse
        } else {
            match choice.finish_reason.as_deref() {
                Some("length") => ChatStopReason::MaxTokens,
                _ => ChatStopReason::Stop,
            }
        };

        Ok(ChatResponse {
            text,
            tool_calls,
            stop_reason,
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false, // TODO: add streaming support
            tool_calling: true,
            vision: false,
        }
    }

    fn name(&self) -> &str {
        "ollama"
    }
}

// --- Wire types for Ollama's OpenAI-compatible API ---

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
struct OllamaChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Serialize)]
struct OllamaToolCall {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    function: OllamaFunction,
}

#[derive(Debug, Serialize)]
struct OllamaFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    choices: Vec<OllamaChatChoice>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatChoice {
    message: OllamaChatMessageResponse,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatMessageResponse {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<RawToolCall>>,
}

#[derive(Debug, Deserialize)]
struct RawToolCall {
    #[serde(default)]
    id: String,
    function: RawFunction,
}

#[derive(Debug, Deserialize)]
struct RawFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<OllamaModelInfo>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelInfo {
    name: String,
}
