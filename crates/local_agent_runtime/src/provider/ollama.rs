//! Ollama / LiteLLM / OpenAI-compatible LLM provider implementation.
//!
//! Talks to the OpenAI-compatible `/v1/chat/completions` endpoint.
//! Model discovery tries Ollama's native `/api/tags` first, then falls back to
//! `/v1/models` for LiteLLM and other OpenAI-compatible proxies.

use std::collections::BTreeMap;

use anyhow::anyhow;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::text_tool_calls::extract_qwen_style_tool_calls;
use super::{
    ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent, LLMProvider, ProviderCapabilities,
};
use crate::error::ProviderError;
use crate::messages::{ContentPart, Message, UserMessage};
use crate::tools::ToolCall;

/// Normalize a user-entered Ollama / LiteLLM / OpenAI-compatible base URL.
///
/// Strips trailing slashes and a trailing `/v1` so callers can always append
/// `/v1/chat/completions` or `/v1/models` without doubling the prefix.
pub fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    match trimmed.strip_suffix("/v1") {
        Some(without_v1) => without_v1.trim_end_matches('/').to_string(),
        None => trimmed.to_string(),
    }
}

/// True when the user-entered URL ends with `/v1`, signalling an
/// OpenAI-compatible proxy (LiteLLM, LM Studio, Groq, etc.).
pub fn url_prefers_openai_discovery(url: &str) -> bool {
    url.trim().trim_end_matches('/').ends_with("/v1")
}

/// True when the host does not look like a stock local Ollama server.
///
/// Used after `/v1` has already been stripped from a persisted URL so discovery
/// still prefers `/v1/models` for LiteLLM / LM Studio / remote proxies.
pub fn host_prefers_openai_discovery(url: &str) -> bool {
    let normalized = normalize_base_url(url).to_ascii_lowercase();
    !(normalized.contains(":11434")
        || normalized == "http://localhost"
        || normalized == "https://localhost"
        || normalized == "http://127.0.0.1"
        || normalized == "https://127.0.0.1")
}

/// True when a model id looks like a vision-capable (multimodal) model.
///
/// Matches common vision model families: `llava`, `bakllava`, anything with
/// `vision` in the name (e.g. `llama3.2-vision`), `minicpm-v`, and Qwen VL
/// variants (e.g. `qwen2-vl`, `qwen2.5-vl`). Used to infer
/// [`ProviderCapabilities::vision`] when no explicit override is configured.
pub fn model_supports_vision(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    const VISION_NEEDLES: &[&str] = &["llava", "bakllava", "vision", "minicpm-v"];
    if VISION_NEEDLES.iter().any(|needle| lower.contains(needle)) {
        return true;
    }
    lower.contains("qwen")
        && lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token == "vl")
}

/// Configuration for connecting to an Ollama or OpenAI-compatible server.
#[derive(Debug, Clone)]
pub struct OllamaProviderConfig {
    /// Base URL of the server (e.g., "http://localhost:11434" or a LiteLLM
    /// proxy such as "http://host:4000"). Trailing `/v1` is accepted and
    /// normalized away.
    pub base_url: String,
    /// Optional API key for authentication.
    pub api_key: Option<String>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Prefer `/v1/models` before Ollama `/api/tags` during discovery.
    /// Set automatically when the input URL ends with `/v1`.
    pub prefer_openai_discovery: bool,
    /// Explicit override for vision support. `None` infers from the request
    /// model id via [`model_supports_vision`] on each chat request.
    pub vision: Option<bool>,
}

impl Default for OllamaProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            timeout_secs: 300,
            prefer_openai_discovery: false,
            vision: None,
        }
    }
}

/// Ollama / OpenAI-compatible LLM provider.
pub struct OllamaProvider {
    config: OllamaProviderConfig,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(config: OllamaProviderConfig) -> Self {
        let prefer_openai_discovery = config.prefer_openai_discovery
            || url_prefers_openai_discovery(&config.base_url)
            || host_prefers_openai_discovery(&config.base_url);
        let api_key = config
            .api_key
            .filter(|key| !key.trim().is_empty())
            .map(|key| key.trim().to_string());
        Self {
            config: OllamaProviderConfig {
                base_url: normalize_base_url(&config.base_url),
                api_key,
                prefer_openai_discovery,
                timeout_secs: config.timeout_secs,
                vision: config.vision,
            },
            client: reqwest::Client::new(),
        }
    }

    /// Fetch available models from an Ollama or OpenAI-compatible server.
    ///
    /// Local Ollama typically uses `/api/tags` first. When an API key is set or
    /// the user entered a `/v1` base URL, prefer OpenAI-compatible `/v1/models`
    /// (LiteLLM, LM Studio, Groq, etc.) and fall back to `/api/tags`.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        if self.prefers_openai_discovery() {
            self.list_models_openai_then_tags().await
        } else {
            self.list_models_tags_then_openai().await
        }
    }

    fn prefers_openai_discovery(&self) -> bool {
        self.config.prefer_openai_discovery || self.config.api_key.is_some()
    }

    async fn list_models_tags_then_openai(&self) -> Result<Vec<String>, ProviderError> {
        match self.list_models_via_ollama_tags().await {
            Ok(models) => Ok(models),
            Err(ollama_err) => match self.list_models_via_openai().await {
                Ok(models) => Ok(models),
                Err(openai_err) => Err(Self::combine_list_models_errors(ollama_err, openai_err)),
            },
        }
    }

    async fn list_models_openai_then_tags(&self) -> Result<Vec<String>, ProviderError> {
        match self.list_models_via_openai().await {
            Ok(models) => Ok(models),
            Err(openai_err) => match self.list_models_via_ollama_tags().await {
                Ok(models) => Ok(models),
                Err(ollama_err) => Err(Self::combine_list_models_errors(ollama_err, openai_err)),
            },
        }
    }

    fn combine_list_models_errors(
        ollama_err: ProviderError,
        openai_err: ProviderError,
    ) -> ProviderError {
        if openai_err.is_unauthorized() {
            return openai_err;
        }
        if ollama_err.is_unauthorized() {
            return ollama_err;
        }
        ProviderError::RequestFailed(anyhow!(
            "Could not list models via /api/tags ({ollama_err}) or /v1/models ({openai_err})"
        ))
    }

    fn map_list_models_status(status: reqwest::StatusCode, body: &str) -> ProviderError {
        let code = status.as_u16();
        if code == 401 || code == 403 {
            return ProviderError::Unauthorized {
                message: if body.trim().is_empty() {
                    "check the API key for this OpenAI-compatible server".to_string()
                } else {
                    body.to_string()
                },
            };
        }
        ProviderError::RequestFailed(anyhow!("server returned status {status}"))
    }

    async fn list_models_via_ollama_tags(&self) -> Result<Vec<String>, ProviderError> {
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
            .map_err(|e| ProviderError::RequestFailed(anyhow!("Could not reach server: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_list_models_status(status, &body));
        }

        let tags: TagsResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.into()))?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    async fn list_models_via_openai(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/v1/models", self.base_url());
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
            .map_err(|e| ProviderError::RequestFailed(anyhow!("Could not reach server: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_list_models_status(status, &body));
        }

        let models: OpenAiModelsResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.into()))?;
        Ok(models.data.into_iter().map(|m| m.id).collect())
    }

    fn base_url(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    /// Whether image content parts should be serialized on the wire for `model`.
    ///
    /// Uses the explicit `vision` config override when set, otherwise infers
    /// from the model id via [`model_supports_vision`].
    fn vision_enabled_for_model(&self, model: &str) -> bool {
        self.config
            .vision
            .unwrap_or_else(|| model_supports_vision(model))
    }

    /// Prefer structured `tool_calls` from the API; fall back to Qwen-style XML
    /// embedded in the assistant text (common with some Ollama/LiteLLM models).
    fn recover_tool_calls_from_text(mut response: ChatResponse) -> ChatResponse {
        // Drop empty-name structured calls so text recovery is not blocked by junk.
        response.tool_calls.retain(|call| !call.name.is_empty());

        if !response.tool_calls.is_empty() {
            // Still strip raw markup from prose so the UI never shows the XML
            // next to a real structured tool card.
            if super::text_tool_calls::contains_tool_markup(&response.text) {
                let (cleaned_text, _) = extract_qwen_style_tool_calls(&response.text);
                response.text = cleaned_text;
            }
            return response;
        }

        let (cleaned_text, calls) = extract_qwen_style_tool_calls(&response.text);
        if calls.is_empty() {
            return response;
        }
        tracing::info!(
            recovered_tool_count = calls.len(),
            tools = %calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(","),
            "recovered Qwen-style tool calls from assistant text"
        );
        response.text = cleaned_text;
        response.tool_calls = calls;
        response.stop_reason = ChatStopReason::ToolUse;
        response
    }

    fn translate_messages(messages: &[Message], vision_enabled: bool) -> Vec<OllamaChatMessage> {
        messages
            .iter()
            .map(|msg| match msg {
                Message::System(s) => OllamaChatMessage {
                    role: "system".to_string(),
                    content: Some(OllamaMessageContent::Text(s.content.clone())),
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message::User(u) => OllamaChatMessage {
                    role: "user".to_string(),
                    content: Some(Self::translate_user_content(u, vision_enabled)),
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
                                        arguments: serde_json::to_string(&tc.arguments)
                                            .unwrap_or_else(|_| "{}".to_string()),
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
                            Some(OllamaMessageContent::Text(a.content.clone()))
                        },
                        tool_call_id: None,
                        tool_calls,
                    }
                }
                Message::ToolResult(t) => OllamaChatMessage {
                    role: "tool".to_string(),
                    // Prefix makes success/failure hard for weak models to ignore when
                    // they would otherwise invent timeouts after a green shell card.
                    content: Some(OllamaMessageContent::Text(format_tool_result_content(
                        &t.result,
                    ))),
                    tool_call_id: Some(t.call_id.clone()),
                    tool_calls: None,
                },
            })
            .collect()
    }

    /// Serialize a user message's content parts.
    ///
    /// When vision is enabled and the message carries image parts, emits an
    /// OpenAI-style content array with `text` and `image_url` parts so the
    /// model can see the attached images. Otherwise (vision disabled, or a
    /// vision-capable model with a text-only message) images are stripped
    /// and only text reaches the wire.
    fn translate_user_content(message: &UserMessage, vision_enabled: bool) -> OllamaMessageContent {
        if vision_enabled && message.has_images() {
            let parts = message
                .parts
                .iter()
                .map(|part| match part {
                    ContentPart::Text(text) => OllamaContentPart::Text { text: text.clone() },
                    ContentPart::Image {
                        mime_type,
                        data_base64,
                        ..
                    } => OllamaContentPart::ImageUrl {
                        image_url: OllamaImageUrl {
                            url: format!("data:{mime_type};base64,{data_base64}"),
                        },
                    },
                })
                .collect();
            OllamaMessageContent::Parts(parts)
        } else {
            OllamaMessageContent::Text(message.text_content())
        }
    }

    fn parse_tool_calls(raw: &[RawToolCall]) -> Vec<ToolCall> {
        raw.iter()
            .map(|tc| {
                let arguments = tc.function.arguments.as_value();
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

    async fn send_chat_request(
        &self,
        request: ChatRequest,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/v1/chat/completions", self.base_url());
        let model = request.model.clone();
        let vision_enabled = self.vision_enabled_for_model(&model);

        let tools: Option<Vec<Value>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(|t| t.to_openai_tool()).collect())
        };

        let body = OllamaChatRequest {
            model: request.model,
            messages: Self::translate_messages(&request.messages, vision_enabled),
            stream,
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
            } else if e.is_connect() || e.is_request() {
                ProviderError::Transient {
                    message: format!("OpenAI-compatible request failed: {e}"),
                }
            } else {
                ProviderError::RequestFailed(anyhow!("OpenAI-compatible request failed: {e}"))
            }
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(Self::map_chat_http_error(status, body_text, &model));
        }

        Ok(resp)
    }

    /// Map non-success HTTP statuses from `/v1/chat/completions`.
    pub fn map_chat_http_error(
        status: reqwest::StatusCode,
        body_text: String,
        model: &str,
    ) -> ProviderError {
        let body_lower = body_text.to_ascii_lowercase();
        let code = status.as_u16();
        if code == 401 || code == 403 {
            return ProviderError::Unauthorized {
                message: if body_text.trim().is_empty() {
                    "check the API key for this OpenAI-compatible server".to_string()
                } else {
                    body_text
                },
            };
        }
        if code == 429 {
            return ProviderError::RateLimited;
        }
        if code == 404 {
            return ProviderError::ModelNotFound {
                model: model.to_string(),
            };
        }
        if code == 408 || status.is_server_error() {
            return ProviderError::Transient {
                message: format!("Server returned {status}: {body_text}"),
            };
        }
        if code == 400
            && ["context", "token limit", "too many tokens"]
                .iter()
                .any(|needle| body_lower.contains(needle))
        {
            return ProviderError::ContextWindowExceeded { message: body_text };
        }
        ProviderError::RequestFailed(anyhow!("Server returned {status}: {body_text}"))
    }

    async fn process_stream_line(
        line: &str,
        assembly: &mut StreamAssembly,
        event_tx: &async_channel::Sender<ChatStreamEvent>,
    ) -> Result<bool, ProviderError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }

        let Some(data) = trimmed.strip_prefix("data:") else {
            return Ok(false);
        };
        let data = data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }

        let chunk: OllamaChatStreamChunk =
            serde_json::from_str(data).map_err(|e| ProviderError::RequestFailed(e.into()))?;
        for choice in chunk.choices {
            if let Some(finish_reason) = choice.finish_reason {
                assembly.finish_reason = Some(finish_reason);
            }

            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    assembly.text.push_str(&content);
                    if let Some(safe) = assembly.drain_streamable_text_prefix() {
                        let _ = event_tx
                            .send(ChatStreamEvent::TextDelta { text: safe })
                            .await;
                    }
                }
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                assembly.apply_tool_call_deltas(tool_calls);
            }
        }

        Ok(false)
    }
}

#[async_trait::async_trait]
impl LLMProvider for OllamaProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let resp = self.send_chat_request(request, false).await?;

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

        Ok(Self::recover_tool_calls_from_text(ChatResponse {
            text,
            tool_calls,
            stop_reason,
        }))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        event_tx: async_channel::Sender<ChatStreamEvent>,
    ) -> Result<ChatResponse, ProviderError> {
        let resp = self.send_chat_request(request, true).await?;
        let mut byte_stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut assembly = StreamAssembly::default();

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(|e| {
                ProviderError::RequestFailed(anyhow!("OpenAI-compatible stream failed: {}", e))
            })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_index) = buffer.find('\n') {
                let mut line = buffer[..newline_index].to_string();
                if line.ends_with('\r') {
                    line.pop();
                }
                buffer.drain(..=newline_index);

                if Self::process_stream_line(&line, &mut assembly, &event_tx).await? {
                    if let Some(safe) = assembly.flush_streamable_text_prefix() {
                        let _ = event_tx
                            .send(ChatStreamEvent::TextDelta { text: safe })
                            .await;
                    }
                    return Ok(Self::recover_tool_calls_from_text(assembly.into_response()));
                }
            }
        }

        if !buffer.trim().is_empty() {
            let _ = Self::process_stream_line(&buffer, &mut assembly, &event_tx).await?;
        }

        if let Some(safe) = assembly.flush_streamable_text_prefix() {
            let _ = event_tx
                .send(ChatStreamEvent::TextDelta { text: safe })
                .await;
        }

        Ok(Self::recover_tool_calls_from_text(assembly.into_response()))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            vision: self.config.vision.unwrap_or(false),
        }
    }

    fn name(&self) -> &str {
        "openai-compatible"
    }
}

fn format_tool_result_content(result: &crate::tools::ToolCallResult) -> String {
    if result.is_error {
        format!(
            "TOOL_ERROR\n{}\n\nReport this error to the user. Do not invent a different failure reason.",
            result.content
        )
    } else {
        format!(
            "TOOL_SUCCESS (authoritative)\n{}\n\nAnswer the user from this result now. Do not invent timeouts, environment failures, or claim the tool failed.",
            result.content
        )
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
    content: Option<OllamaMessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

/// A chat message's content: plain text, or (for vision requests with image
/// parts) an OpenAI-style array of typed content parts.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OllamaMessageContent {
    Text(String),
    Parts(Vec<OllamaContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OllamaContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OllamaImageUrl },
}

#[derive(Debug, Serialize)]
struct OllamaImageUrl {
    url: String,
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
struct OllamaChatStreamChunk {
    #[serde(default)]
    choices: Vec<OllamaChatStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatStreamChoice {
    #[serde(default)]
    delta: OllamaChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OllamaChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<RawStreamToolCall>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawStreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: RawStreamFunction,
}

#[derive(Debug, Default, Deserialize)]
struct RawStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<RawArguments>,
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
    #[serde(default)]
    arguments: RawArguments,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawArguments {
    String(String),
    Value(Value),
}

impl Default for RawArguments {
    fn default() -> Self {
        RawArguments::Value(Value::Object(Default::default()))
    }
}

impl RawArguments {
    fn as_value(&self) -> Value {
        match self {
            RawArguments::String(arguments) => {
                serde_json::from_str(arguments).unwrap_or(Value::String(arguments.clone()))
            }
            RawArguments::Value(arguments) => arguments.clone(),
        }
    }

    fn as_argument_fragment(&self) -> String {
        match self {
            RawArguments::String(arguments) => arguments.clone(),
            RawArguments::Value(Value::String(arguments)) => arguments.clone(),
            RawArguments::Value(arguments) => arguments.to_string(),
        }
    }
}

#[derive(Debug, Default)]
struct StreamAssembly {
    text: String,
    /// How much of `text` has already been emitted as streamable (non-tool) text.
    streamed_text_len: usize,
    tool_calls: BTreeMap<usize, StreamToolCallAssembly>,
    finish_reason: Option<String>,
}

impl StreamAssembly {
    /// Emit only assistant prose that cannot still become Qwen-style tool markup.
    ///
    /// Once `<function=` / `<tool_call>` appears, withhold the remainder so the
    /// UI does not show raw tool XML while we recover structured tool calls.
    fn drain_streamable_text_prefix(&mut self) -> Option<String> {
        self.drain_streamable_text_prefix_inner(true)
    }

    /// Flush any remaining withheld prose at end-of-stream (no partial-marker holdback).
    fn flush_streamable_text_prefix(&mut self) -> Option<String> {
        self.drain_streamable_text_prefix_inner(false)
    }

    fn drain_streamable_text_prefix_inner(&mut self, hold_partial_marker: bool) -> Option<String> {
        let pending = &self.text[self.streamed_text_len..];
        if pending.is_empty() {
            return None;
        }

        let markup_rel = ["<function=", "<function name=", "<tool_call>"]
            .iter()
            .filter_map(|marker| pending.find(marker))
            .min();

        let emit_len = match markup_rel {
            Some(0) => {
                // Already inside tool markup — withhold everything.
                return None;
            }
            Some(index) => index,
            None if hold_partial_marker => {
                // Hold back a short suffix that could be the start of a marker.
                const HOLD: usize = "<function name=".len().saturating_sub(1);
                if pending.len() <= HOLD {
                    return None;
                }
                floor_char_boundary(pending, pending.len() - HOLD)
            }
            None => pending.len(),
        };

        if emit_len == 0 {
            return None;
        }

        let emit_len = floor_char_boundary(pending, emit_len);
        if emit_len == 0 {
            return None;
        }

        let emit = pending[..emit_len].to_string();
        self.streamed_text_len += emit_len;
        Some(emit)
    }

    fn apply_tool_call_deltas(&mut self, tool_calls: Vec<RawStreamToolCall>) {
        for call in tool_calls {
            let assembled = self.tool_calls.entry(call.index).or_default();

            if !call.id.is_empty() {
                assembled.id = call.id;
            }
            if let Some(name) = call.function.name {
                assembled.name.push_str(&name);
            }
            if let Some(arguments) = call.function.arguments {
                assembled
                    .arguments
                    .push_str(&arguments.as_argument_fragment());
            }
        }
    }

    fn into_response(self) -> ChatResponse {
        let tool_calls = self
            .tool_calls
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(StreamToolCallAssembly::into_tool_call)
            .collect::<Vec<_>>();

        let stop_reason = if !tool_calls.is_empty() {
            ChatStopReason::ToolUse
        } else {
            match self.finish_reason.as_deref() {
                Some("length") => ChatStopReason::MaxTokens,
                _ => ChatStopReason::Stop,
            }
        };

        ChatResponse {
            text: self.text,
            tool_calls,
            stop_reason,
        }
    }
}

/// Largest byte index ≤ `index` that sits on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[derive(Debug, Default)]
struct StreamToolCallAssembly {
    id: String,
    name: String,
    arguments: String,
}

impl StreamToolCallAssembly {
    fn into_tool_call(self) -> ToolCall {
        let arguments = if self.arguments.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&self.arguments).unwrap_or(Value::String(self.arguments))
        };

        ToolCall {
            id: if self.id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                self.id
            },
            name: self.name,
            arguments,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<OllamaModelInfo>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelInfo {
    name: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelInfo>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelInfo {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_strips_trailing_slash_and_v1() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/"),
            "http://localhost:11434"
        );
        assert_eq!(
            normalize_base_url("http://100.95.111.65:4000/v1"),
            "http://100.95.111.65:4000"
        );
        assert_eq!(
            normalize_base_url("http://100.95.111.65:4000/v1/"),
            "http://100.95.111.65:4000"
        );
    }

    #[test]
    fn url_prefers_openai_discovery_detects_v1_suffix() {
        assert!(url_prefers_openai_discovery("http://host:4000/v1"));
        assert!(url_prefers_openai_discovery("http://host:4000/v1/"));
        assert!(url_prefers_openai_discovery(
            "  https://api.groq.com/openai/v1  "
        ));
        assert!(!url_prefers_openai_discovery("http://localhost:11434"));
        assert!(!url_prefers_openai_discovery("http://host:4000/v1/chat"));
    }

    #[test]
    fn empty_api_key_is_filtered_and_v1_url_prefers_openai_discovery() {
        let provider = OllamaProvider::new(OllamaProviderConfig {
            base_url: "http://host:4000/v1".to_string(),
            api_key: Some("   ".to_string()),
            ..Default::default()
        });
        assert!(provider.config.api_key.is_none());
        assert!(provider.prefers_openai_discovery());
    }

    #[test]
    fn api_key_prefers_openai_discovery_even_without_v1_url() {
        let provider = OllamaProvider::new(OllamaProviderConfig {
            base_url: "https://api.groq.com/openai".to_string(),
            api_key: Some("secret".to_string()),
            ..Default::default()
        });
        assert_eq!(provider.config.api_key.as_deref(), Some("secret"));
        assert!(provider.prefers_openai_discovery());
    }

    #[test]
    fn non_ollama_host_prefers_openai_discovery_after_v1_strip() {
        assert!(host_prefers_openai_discovery("http://localhost:1234"));
        assert!(host_prefers_openai_discovery("http://100.95.111.65:4000"));
        assert!(!host_prefers_openai_discovery("http://localhost:11434/v1"));
        let provider = OllamaProvider::new(OllamaProviderConfig {
            base_url: "http://localhost:1234".to_string(),
            ..Default::default()
        });
        assert!(provider.prefers_openai_discovery());
    }

    #[test]
    fn map_chat_http_error_maps_unauthorized() {
        let err = OllamaProvider::map_chat_http_error(
            reqwest::StatusCode::UNAUTHORIZED,
            "invalid api key".to_string(),
            "qwen",
        );
        assert!(err.is_unauthorized());
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("invalid api key"));

        let forbidden = OllamaProvider::map_chat_http_error(
            reqwest::StatusCode::FORBIDDEN,
            String::new(),
            "qwen",
        );
        assert!(forbidden.is_unauthorized());
        assert!(forbidden.to_string().contains("check the API key"));
    }

    #[test]
    fn parses_openai_models_response() {
        let raw = r#"{"data":[{"id":"qwen3-coder:latest","object":"model"}],"object":"list"}"#;
        let parsed: OpenAiModelsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.data[0].id, "qwen3-coder:latest");
    }

    #[test]
    fn parses_tool_call_arguments_from_json_string() {
        let calls = OllamaProvider::parse_tool_calls(&[RawToolCall {
            id: "call_1".to_string(),
            function: RawFunction {
                name: "run_shell_command".to_string(),
                arguments: RawArguments::String(r#"{"command":"pwd"}"#.to_string()),
            },
        }]);

        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "run_shell_command");
        assert_eq!(calls[0].arguments, serde_json::json!({"command": "pwd"}));
    }

    #[test]
    fn parses_tool_call_arguments_from_json_object() {
        let calls = OllamaProvider::parse_tool_calls(&[RawToolCall {
            id: "call_1".to_string(),
            function: RawFunction {
                name: "read_files".to_string(),
                arguments: RawArguments::Value(serde_json::json!({"paths": ["src/lib.rs"]})),
            },
        }]);

        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "read_files");
        assert_eq!(
            calls[0].arguments,
            serde_json::json!({"paths": ["src/lib.rs"]})
        );
    }

    #[test]
    fn assembles_streamed_tool_call_arguments_from_fragments() {
        let mut assembly = StreamAssembly::default();
        assembly.apply_tool_call_deltas(vec![RawStreamToolCall {
            index: 0,
            id: "call_1".to_string(),
            function: RawStreamFunction {
                name: Some("run_shell_command".to_string()),
                arguments: Some(RawArguments::String(r#"{"command":"pw"#.to_string())),
            },
        }]);
        assembly.apply_tool_call_deltas(vec![RawStreamToolCall {
            index: 0,
            id: String::new(),
            function: RawStreamFunction {
                name: None,
                arguments: Some(RawArguments::String(r#"d"}"#.to_string())),
            },
        }]);

        let response = assembly.into_response();

        assert_eq!(response.text, "");
        assert_eq!(response.stop_reason, ChatStopReason::ToolUse);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "run_shell_command");
        assert_eq!(
            response.tool_calls[0].arguments,
            serde_json::json!({"command": "pwd"})
        );
    }

    #[tokio::test]
    async fn stream_line_emits_text_delta_and_accumulates_text() {
        let (tx, rx) = async_channel::unbounded();
        let mut assembly = StreamAssembly::default();

        let done = OllamaProvider::process_stream_line(
            r#"data: {"choices":[{"delta":{"content":"hello there friend"},"finish_reason":null}]}"#,
            &mut assembly,
            &tx,
        )
        .await
        .unwrap();

        assert!(!done);
        assert_eq!(assembly.text, "hello there friend");
        // Partial marker holdback keeps a short suffix until flush/end.
        let first = rx.recv().await.unwrap();
        let ChatStreamEvent::TextDelta { text: first_text } = first;
        assert!(assembly.text.starts_with(&first_text));
        assert!(first_text.len() < assembly.text.len());

        let flushed = assembly.flush_streamable_text_prefix().unwrap();
        assert_eq!(format!("{first_text}{flushed}"), "hello there friend");
    }

    #[test]
    fn model_supports_vision_detects_known_vision_model_families() {
        assert!(model_supports_vision("llava:13b"));
        assert!(model_supports_vision("bakllava:latest"));
        assert!(model_supports_vision("llama3.2-vision:11b"));
        assert!(model_supports_vision("minicpm-v:8b"));
        assert!(model_supports_vision("qwen2-vl:7b"));
        assert!(model_supports_vision("qwen2.5-vl:32b"));
        assert!(model_supports_vision("QWEN2-VL-7B-INSTRUCT"));
    }

    #[test]
    fn model_supports_vision_rejects_non_vision_models() {
        assert!(!model_supports_vision("qwen2.5-coder:7b"));
        assert!(!model_supports_vision("llama3.1:8b"));
        assert!(!model_supports_vision("qwen3-coder:latest"));
        assert!(!model_supports_vision("mistral:7b"));
    }

    #[test]
    fn translate_messages_serializes_images_as_data_urls_when_vision_enabled() {
        let messages = vec![Message::User(UserMessage {
            parts: vec![
                ContentPart::Text("what is in this image?".to_string()),
                ContentPart::Image {
                    mime_type: "image/png".to_string(),
                    data_base64: "AAAA".to_string(),
                    file_name: Some("shot.png".to_string()),
                },
            ],
        })];

        let translated = OllamaProvider::translate_messages(&messages, true);
        let content = serde_json::to_value(&translated[0].content).unwrap();
        let parts = content.as_array().expect("expected content parts array");

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is in this image?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn translate_messages_strips_images_to_text_when_vision_disabled() {
        let messages = vec![Message::User(UserMessage {
            parts: vec![
                ContentPart::Text("describe this".to_string()),
                ContentPart::Image {
                    mime_type: "image/png".to_string(),
                    data_base64: "AAAA".to_string(),
                    file_name: None,
                },
            ],
        })];

        let translated = OllamaProvider::translate_messages(&messages, false);
        let content = serde_json::to_value(&translated[0].content).unwrap();

        assert_eq!(content, serde_json::json!("describe this"));
    }

    #[test]
    fn translate_messages_keeps_plain_text_for_text_only_message_even_with_vision_enabled() {
        let messages = vec![Message::User(UserMessage::text("just text, no images"))];

        let translated = OllamaProvider::translate_messages(&messages, true);
        let content = serde_json::to_value(&translated[0].content).unwrap();

        assert_eq!(content, serde_json::json!("just text, no images"));
    }

    #[test]
    fn stream_assembly_withholds_qwen_tool_markup_from_deltas() {
        let mut assembly = StreamAssembly::default();
        assembly
            .text
            .push_str("Looking.\n\n<function=file_glob_v2>\n");
        let safe = assembly.drain_streamable_text_prefix().unwrap();
        assert_eq!(safe, "Looking.\n\n");
        assert!(assembly.drain_streamable_text_prefix().is_none());
        assert!(assembly.flush_streamable_text_prefix().is_none());
    }
}
