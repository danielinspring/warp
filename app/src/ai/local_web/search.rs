//! Keyless DuckDuckGo HTML search backend for local web_search.

use super::extract::html_to_text;
use super::fetch::build_client;
use super::policy::{validate_http_url, WebPolicy};
use super::{SearchResponse, SearchResultItem};

const PROVIDER: &str = "duckduckgo_html";

pub async fn search_web(
    query: &str,
    num_results: usize,
    policy: &WebPolicy,
) -> Result<SearchResponse, String> {
    let encoded = urlencoding::encode(query);
    let search_url = format!("https://html.duckduckgo.com/html/?q={encoded}");
    // Policy-check the search endpoint itself (public HTTPS).
    validate_http_url(&search_url, policy).map_err(|e| e.to_string())?;

    let client = build_client(policy)?;
    let response = client
        .get(&search_url)
        .header("Accept", "text/html")
        .send()
        .await
        .map_err(|e| format!("search request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("search HTTP {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read search body: {e}"))?;
    if bytes.len() > policy.max_response_bytes {
        return Err("search response exceeds size limit".to_string());
    }
    let html = String::from_utf8_lossy(&bytes);
    let mut results = parse_duckduckgo_html(&html);
    results.truncate(num_results);

    // If structured parse fails, fall back to a short plain-text extract so the
    // model still gets something actionable rather than a hard empty list.
    if results.is_empty() {
        let (_title, text) = html_to_text(&html);
        let snippet: String = text.chars().take(500).collect();
        if !snippet.is_empty() {
            results.push(SearchResultItem {
                title: "DuckDuckGo search page".to_string(),
                url: search_url.clone(),
                snippet,
            });
        }
    }

    Ok(SearchResponse {
        query: query.to_string(),
        provider: PROVIDER.to_string(),
        results,
    })
}

/// Parse result links from DuckDuckGo's HTML endpoint.
///
/// The markup changes over time; we accept several common patterns and dedupe by URL.
fn parse_duckduckgo_html(html: &str) -> Vec<SearchResultItem> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Pattern: result__a anchors (classic HTML endpoint).
    for (href, title) in find_anchors_with_class(html, "result__a") {
        let url = normalize_ddg_href(&href);
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let snippet = nearby_snippet(html, &href).unwrap_or_default();
        results.push(SearchResultItem {
            title: if title.is_empty() { url.clone() } else { title },
            url,
            snippet,
        });
    }

    // Pattern: uddg= redirect links often embed the destination URL.
    if results.is_empty() {
        for url in find_uddg_urls(html) {
            if !seen.insert(url.clone()) {
                continue;
            }
            results.push(SearchResultItem {
                title: url.clone(),
                url,
                snippet: String::new(),
            });
        }
    }

    results
}

fn find_anchors_with_class(html: &str, class_name: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let needle = format!("class=\"{class_name}\"");
    let mut rest = html;
    while let Some(idx) = rest.find(&needle) {
        // Walk back to the start of this <a ...>
        let before = &rest[..idx];
        let Some(a_start_rel) = before.rfind("<a") else {
            rest = &rest[idx + needle.len()..];
            continue;
        };
        let a_slice = &rest[a_start_rel..];
        let Some(tag_end) = a_slice.find('>') else {
            rest = &rest[idx + needle.len()..];
            continue;
        };
        let open_tag = &a_slice[..=tag_end];
        let after_open = &a_slice[tag_end + 1..];
        let Some(close_rel) = after_open.to_ascii_lowercase().find("</a>") else {
            rest = &rest[idx + needle.len()..];
            continue;
        };
        let inner = &after_open[..close_rel];
        let href = attr_value(open_tag, "href").unwrap_or_default();
        let title = collapse(&strip_tags_simple(inner));
        out.push((href, title));
        rest = &after_open[close_rel + 4..];
    }
    out
}

fn find_uddg_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(idx) = rest.find("uddg=") {
        let after = &rest[idx + 5..];
        let end = after
            .find(|c: char| c == '&' || c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(after.len());
        let encoded = &after[..end];
        if let Ok(decoded) = urlencoding::decode(encoded) {
            let s = decoded.into_owned();
            if s.starts_with("http://") || s.starts_with("https://") {
                out.push(s);
            }
        }
        rest = &after[end..];
    }
    out
}

fn normalize_ddg_href(href: &str) -> String {
    let absolute = if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with("//") {
        format!("https:{href}")
    } else {
        return String::new();
    };

    // DuckDuckGo often wraps destinations as uddg= query params on a redirect URL.
    if let Some(idx) = absolute.find("uddg=") {
        let after = &absolute[idx + 5..];
        let end = after
            .find(|c: char| c == '&' || c == '"' || c == '\'')
            .unwrap_or(after.len());
        if let Ok(decoded) = urlencoding::decode(&after[..end]) {
            let s = decoded.into_owned();
            if s.starts_with("http://") || s.starts_with("https://") {
                return s;
            }
        }
    }
    absolute
}

fn nearby_snippet(html: &str, href: &str) -> Option<String> {
    let idx = html.find(href)?;
    let window_start = idx.saturating_sub(0);
    let window_end = (idx + href.len() + 800).min(html.len());
    let window = &html[window_start..window_end];
    // Prefer result__snippet class content.
    if let Some(snip_idx) = window.find("result__snippet") {
        let after = &window[snip_idx..];
        if let Some(gt) = after.find('>') {
            let content = &after[gt + 1..];
            if let Some(end) = content.find('<') {
                let s = collapse(&strip_tags_simple(&content[..end]));
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let patterns = [format!("{name}=\""), format!("{name}='")];
    for (i, pat) in patterns.iter().enumerate() {
        if let Some(idx) = tag.find(pat) {
            let after = &tag[idx + pat.len()..];
            let end_ch = if i == 0 { '"' } else { '\'' };
            if let Some(end) = after.find(end_ch) {
                return Some(after[..end].to_string());
            }
        }
    }
    None
}

fn strip_tags_simple(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_result_anchors() {
        let html = r##"
            <a rel="nofollow" class="result__a" href="https://docs.rs/serde">Serde docs</a>
            <a class="result__snippet" href="#">Serde is a framework for serializing.</a>
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">Example</a>
        "##;
        let results = parse_duckduckgo_html(html);
        assert!(!results.is_empty());
        assert_eq!(results[0].url, "https://docs.rs/serde");
        assert!(results[0].title.contains("Serde"));
        assert!(results.iter().any(|r| r.url == "https://example.com/page"));
    }
}
