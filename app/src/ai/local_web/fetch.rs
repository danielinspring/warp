//! HTTP fetch with policy-checked redirects and size caps.

use std::time::Duration;

use reqwest::redirect::Policy as RedirectPolicy;
use reqwest::{Client, StatusCode};
use url::Url;

use super::extract::{html_to_text, truncate_text};
use super::policy::{validate_http_url, validate_parsed_url, WebPolicy};
use super::FetchPageResult;

pub async fn fetch_url(raw_url: &str, max_chars: usize, policy: &WebPolicy) -> FetchPageResult {
    let url = match validate_http_url(raw_url, policy) {
        Ok(u) => u,
        Err(err) => {
            return FetchPageResult {
                url: raw_url.to_string(),
                title: String::new(),
                ok: false,
                text: String::new(),
                truncated: false,
                error: Some(err.to_string()),
            };
        }
    };

    match fetch_validated(&url, max_chars, policy).await {
        Ok(page) => page,
        Err(err) => FetchPageResult {
            url: url.to_string(),
            title: String::new(),
            ok: false,
            text: String::new(),
            truncated: false,
            error: Some(err),
        },
    }
}

async fn fetch_validated(
    url: &Url,
    max_chars: usize,
    policy: &WebPolicy,
) -> Result<FetchPageResult, String> {
    let client = build_client(policy)?;
    let response = client
        .get(url.clone())
        .header(
            "Accept",
            "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let final_url = response.url().clone();
    // Defense in depth if redirect policy is bypassed.
    validate_parsed_url(&final_url).map_err(|e| e.to_string())?;

    let status = response.status();
    if !(status.is_success() || status == StatusCode::NOT_MODIFIED) {
        return Err(format!("HTTP {status} for {final_url}"));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read body: {e}"))?;
    if bytes.len() > policy.max_response_bytes {
        return Err(format!(
            "response exceeds size limit ({} bytes > {})",
            bytes.len(),
            policy.max_response_bytes
        ));
    }

    let body = String::from_utf8_lossy(&bytes);
    let (title, text, truncated) =
        if content_type.contains("html") || body.trim_start().starts_with('<') {
            let (title, text) = html_to_text(&body);
            let (text, truncated) = truncate_text(text, max_chars);
            (title, text, truncated)
        } else {
            let (text, truncated) = truncate_text(body.into_owned(), max_chars);
            (String::new(), text, truncated)
        };

    Ok(FetchPageResult {
        url: final_url.to_string(),
        title,
        ok: true,
        text,
        truncated,
        error: None,
    })
}

pub(crate) fn build_client(policy: &WebPolicy) -> Result<Client, String> {
    let max_redirects = policy.max_redirects;
    Client::builder()
        .user_agent(policy.user_agent.clone())
        .connect_timeout(Duration::from_secs(policy.connect_timeout_secs))
        .timeout(Duration::from_secs(policy.request_timeout_secs))
        .redirect(RedirectPolicy::custom(move |attempt| {
            if attempt.previous().len() >= max_redirects {
                return attempt.stop();
            }
            match validate_parsed_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(_) => attempt.stop(),
            }
        }))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}
