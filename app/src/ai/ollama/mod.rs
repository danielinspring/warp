use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod agent_loop;

pub struct OllamaClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallSerialized>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallSerialized {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunctionSerialized,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallFunctionSerialized {
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            name: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageOwned,
    #[serde(default)]
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessageOwned {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallParsed>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallParsed {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub function: ToolCallFunctionParsed,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallFunctionParsed {
    pub name: String,
    /// JSON-encoded string per OpenAI spec.
    pub arguments: String,
}

/// Result of a single Ollama chat completion.
#[derive(Debug, Clone)]
pub struct ChatCompletion {
    pub text: String,
    pub tool_calls: Vec<ToolCallParsed>,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
pub struct OllamaModel {
    pub name: String,
}

impl OllamaClient {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.filter(|k| !k.is_empty()),
            client: reqwest::Client::new(),
        }
    }

    /// Fetch the list of locally-installed Ollama models.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/tags", self.base_url);
        let mut req = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("Could not reach Ollama at {}: {}", self.base_url, e))?;

        if !resp.status().is_success() {
            return Err(anyhow!("Ollama returned status {}", resp.status()));
        }

        let tags: TagsResponse = resp.json().await?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    /// Send a chat completion request and return the full response text.
    /// Kept for the legacy ai_assistant dialogue path (no tools, simple text).
    pub async fn chat(&self, model: &str, messages: Vec<ChatMessage>) -> Result<String> {
        let completion = self.chat_with_tools(model, messages, None).await?;
        Ok(completion.text)
    }

    /// Send a chat completion request, optionally advertising a set of
    /// OpenAI-style function tools. Returns either text content or a
    /// list of tool calls (or both, when the model interleaves them).
    pub async fn chat_with_tools(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        tools: Option<Vec<Value>>,
    ) -> Result<ChatCompletion> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = ChatRequest {
            model: model.to_string(),
            messages,
            stream: false,
            tools,
        };

        let mut req = self
            .client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(300));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("Ollama request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama returned {}: {}", status, body));
        }

        let chat_resp: ChatResponse = resp.json().await?;
        let choice = chat_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Ollama returned no choices"))?;
        let text = choice.message.content.unwrap_or_default();
        let tool_calls = choice.message.tool_calls.unwrap_or_default();
        Ok(ChatCompletion { text, tool_calls })
    }
}
