//! The agent configuration shown in the pane, read from the local agent service.
//!
//! The system prompt and tool list live in the service now, so the pane asks for them rather than
//! computing them. A run against the cloud has no service to ask, which is why every field
//! degrades to an empty default instead of an error.

use local_agent_runtime::ToolSchema;
use serde::Deserialize;

/// What `GET /debug/spec` returns.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentSpec {
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<ToolSchema>,
}

impl AgentSpec {
    /// Placeholder shown when the service cannot be reached.
    pub fn offline() -> Self {
        Self {
            system_prompt: "The local agent service is not reachable, so its prompt and tools are \
                            unknown. Start it with `cargo run -p warp_local_agent`."
                .to_string(),
            tools: Vec::new(),
        }
    }
}

/// Fetch the service's prompt and tool list.
pub async fn fetch(base_url: &str) -> anyhow::Result<AgentSpec> {
    let url = format!("{}/debug/spec", base_url.trim_end_matches('/'));
    let spec = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<AgentSpec>()
        .await?;
    Ok(spec)
}
