//! Ollama LLM provider implementation.
//!
//! Talks to Ollama's OpenAI-compatible `/v1/chat/completions` endpoint.

use std::collections::BTreeMap;

use anyhow::anyhow;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ChatRequest, ChatResponse, ChatStopReason, ChatStreamEvent, LLMProvider, ProviderCapabilities,
};
use crate::error::ProviderError;
use crate::messages::Message;
use crate::tools::ToolCall;

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

        let tools: Option<Vec<Value>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(|t| t.to_openai_tool()).collect())
        };

        let body = OllamaChatRequest {
            model: request.model,
            messages: Self::translate_messages(&request.messages),
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
                    message: format!("Ollama request failed: {e}"),
                }
            } else {
                ProviderError::RequestFailed(anyhow!("Ollama request failed: {e}"))
            }
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            let body_lower = body_text.to_ascii_lowercase();
            if status.as_u16() == 429 {
                return Err(ProviderError::RateLimited);
            }
            if status.as_u16() == 404 {
                return Err(ProviderError::ModelNotFound { model });
            }
            if status.as_u16() == 408 || status.is_server_error() {
                return Err(ProviderError::Transient {
                    message: format!("Ollama returned {status}: {body_text}"),
                });
            }
            if status.as_u16() == 400
                && ["context", "token limit", "too many tokens"]
                    .iter()
                    .any(|needle| body_lower.contains(needle))
            {
                return Err(ProviderError::ContextWindowExceeded { message: body_text });
            }
            return Err(ProviderError::RequestFailed(anyhow!(
                "Ollama returned {status}: {body_text}"
            )));
        }

        Ok(resp)
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
                    let _ = event_tx
                        .send(ChatStreamEvent::TextDelta { text: content })
                        .await;
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

        Ok(ChatResponse {
            text,
            tool_calls,
            stop_reason,
        })
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
                ProviderError::RequestFailed(anyhow!("Ollama stream failed: {}", e))
            })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_index) = buffer.find('\n') {
                let mut line = buffer[..newline_index].to_string();
                if line.ends_with('\r') {
                    line.pop();
                }
                buffer.drain(..=newline_index);

                if Self::process_stream_line(&line, &mut assembly, &event_tx).await? {
                    return Ok(assembly.into_response());
                }
            }
        }

        if !buffer.trim().is_empty() {
            let _ = Self::process_stream_line(&buffer, &mut assembly, &event_tx).await?;
        }

        Ok(assembly.into_response())
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
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
    tool_calls: BTreeMap<usize, StreamToolCallAssembly>,
    finish_reason: Option<String>,
}

impl StreamAssembly {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            r#"data: {"choices":[{"delta":{"content":"hel"},"finish_reason":null}]}"#,
            &mut assembly,
            &tx,
        )
        .await
        .unwrap();

        assert!(!done);
        assert_eq!(assembly.text, "hel");
        assert_eq!(
            rx.recv().await.unwrap(),
            ChatStreamEvent::TextDelta {
                text: "hel".to_string()
            }
        );
    }
}
