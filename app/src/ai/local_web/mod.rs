//! Local web search and fetch for the Ollama agent runtime.
//!
//! These tools run in-process (not via a cloud AIAgentAction) with an explicit
//! SSRF/privacy policy. Search uses a keyless DuckDuckGo HTML backend by default;
//! fetch uses HTTP GET with HTML-to-text extraction and size caps.

mod extract;
mod fetch;
mod policy;
mod search;

use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::{ToolCall, ToolCallResult, ToolExecutionError};
pub use policy::WebPolicy;
use serde::Serialize;

const DEFAULT_SEARCH_RESULTS: u32 = 5;
const MAX_SEARCH_RESULTS: u32 = 10;
const DEFAULT_FETCH_CHARS: usize = 12_000;
const MAX_FETCH_CHARS: usize = 50_000;
const MAX_FETCH_URLS: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub provider: String,
    pub results: Vec<SearchResultItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchPageResult {
    pub url: String,
    pub title: String,
    pub ok: bool,
    pub text: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchResponse {
    pub pages: Vec<FetchPageResult>,
}

pub fn web_search_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "web_search",
        "Search the public web for current documentation, errors, APIs, or news. Returns title, URL, and snippet for each result. Use web_fetch to load a full page. Prefer this over inventing version numbers or URLs.",
    )
    .required_string("query", "Search query")
    .optional_number(
        "num_results",
        &format!("Number of results to return (1-{MAX_SEARCH_RESULTS}, default {DEFAULT_SEARCH_RESULTS})"),
    )
    .build()
}

pub fn web_fetch_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "web_fetch",
        "Fetch one or more public http(s) URLs and return extracted plain text. Private/local network addresses are blocked. Prefer web_search first when you do not know the URL.",
    )
    .required_string_array(
        "urls",
        "One or more absolute http(s) URLs to fetch (max 5)",
    )
    .optional_number(
        "max_chars_per_url",
        &format!(
            "Max characters of extracted text per URL (default {DEFAULT_FETCH_CHARS}, max {MAX_FETCH_CHARS})"
        ),
    )
    .build()
}

/// Execute a local web tool call. Returns structured JSON content.
pub async fn execute_web_tool(call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
    match call.name.as_str() {
        "web_search" => execute_web_search(call).await,
        "web_fetch" => execute_web_fetch(call).await,
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

async fn execute_web_search(call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
    let query = required_string(&call.arguments, "query")?;
    if query.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `web_search` requires a non-empty `query`".to_string(),
        });
    }
    let num_results = optional_u32(&call.arguments, "num_results")?
        .unwrap_or(DEFAULT_SEARCH_RESULTS)
        .clamp(1, MAX_SEARCH_RESULTS);

    let policy = WebPolicy::default();
    match search::search_web(&query, num_results as usize, &policy).await {
        Ok(response) => Ok(ToolCallResult::success(
            serde_json::to_string_pretty(&response).unwrap_or_else(|_| {
                r#"{"error":"failed to serialize search response"}"#.to_string()
            }),
        )),
        Err(err) => Ok(ToolCallResult::error(format!(
            "{{\"error\":\"web_search failed\",\"message\":{}}}",
            serde_json::to_string(&err.to_string()).unwrap_or_else(|_| "\"unknown\"".to_string())
        ))),
    }
}

async fn execute_web_fetch(call: &ToolCall) -> Result<ToolCallResult, ToolExecutionError> {
    let urls = required_string_array(&call.arguments, "urls")?;
    if urls.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `web_fetch` requires a non-empty `urls` array".to_string(),
        });
    }
    if urls.len() > MAX_FETCH_URLS {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Tool `web_fetch` accepts at most {MAX_FETCH_URLS} URLs"),
        });
    }
    let max_chars = optional_u32(&call.arguments, "max_chars_per_url")?
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_FETCH_CHARS)
        .clamp(1, MAX_FETCH_CHARS);

    let policy = WebPolicy::default();
    let mut pages = Vec::with_capacity(urls.len());
    for url in urls {
        pages.push(fetch::fetch_url(&url, max_chars, &policy).await);
    }
    let response = FetchResponse { pages };
    let any_ok = response.pages.iter().any(|p| p.ok);
    let content = serde_json::to_string_pretty(&response)
        .unwrap_or_else(|_| r#"{"error":"failed to serialize fetch response"}"#.to_string());
    if any_ok {
        Ok(ToolCallResult::success(content))
    } else {
        Ok(ToolCallResult::error(content))
    }
}

fn required_string<'a>(
    args: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, ToolExecutionError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Missing required string argument `{key}`"),
        })
}

fn required_string_array(
    args: &serde_json::Value,
    key: &str,
) -> Result<Vec<String>, ToolExecutionError> {
    let arr = args.get(key).and_then(|v| v.as_array()).ok_or_else(|| {
        ToolExecutionError::InvalidInput {
            reason: format!("Missing required string-array argument `{key}`"),
        }
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let Some(s) = item.as_str() else {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!("`{key}[{i}]` must be a string"),
            });
        };
        out.push(s.to_string());
    }
    Ok(out)
}

fn optional_u32(args: &serde_json::Value, key: &str) -> Result<Option<u32>, ToolExecutionError> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => {
            let Some(v) = n.as_u64() else {
                return Err(ToolExecutionError::InvalidInput {
                    reason: format!("`{key}` must be a non-negative integer"),
                });
            };
            if v > u32::MAX as u64 {
                return Err(ToolExecutionError::InvalidInput {
                    reason: format!("`{key}` is too large"),
                });
            }
            Ok(Some(v as u32))
        }
        Some(_) => Err(ToolExecutionError::InvalidInput {
            reason: format!("`{key}` must be a number"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use local_agent_runtime::ToolCall;

    use super::*;

    #[test]
    fn schemas_have_stable_names() {
        assert_eq!(web_search_schema().name, "web_search");
        assert_eq!(web_fetch_schema().name, "web_fetch");
    }

    #[tokio::test]
    async fn web_search_rejects_empty_query() {
        let call = ToolCall {
            id: "1".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({ "query": "  " }),
        };
        let err = execute_web_tool(&call).await.unwrap_err();
        assert!(matches!(err, ToolExecutionError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn web_fetch_rejects_empty_urls() {
        let call = ToolCall {
            id: "1".into(),
            name: "web_fetch".into(),
            arguments: serde_json::json!({ "urls": [] }),
        };
        let err = execute_web_tool(&call).await.unwrap_err();
        assert!(matches!(err, ToolExecutionError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn web_fetch_denies_private_url_without_network() {
        let call = ToolCall {
            id: "1".into(),
            name: "web_fetch".into(),
            arguments: serde_json::json!({ "urls": ["http://127.0.0.1/"] }),
        };
        let result = execute_web_tool(&call).await.unwrap();
        assert!(result.is_error);
        assert!(
            result.content.contains("127.0.0.1")
                || result.content.contains("blocked")
                || result.content.contains("private")
                || result.content.contains("loopback")
                || result.content.contains("error")
        );
    }
}
