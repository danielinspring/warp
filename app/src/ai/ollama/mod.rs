use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageOwned,
}

#[derive(Debug, Deserialize)]
struct ChatMessageOwned {
    content: String,
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
    pub async fn chat(&self, model: &str, messages: Vec<ChatMessage>) -> Result<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = ChatRequest {
            model: model.to_string(),
            messages,
            stream: false,
        };

        let mut req = self
            .client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(120));
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
        chat_resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow!("Ollama returned no choices"))
    }
}
